//! 传输切换过渡层：把「老通道 → iroh 通道」这个切换过程封装成一个可测组件。
//!
//! 由 feature `iroh-transport` 控制，默认关闭。
//!
//! # 为什么需要这一层
//!
//! 协议（见 [`super::iroh_upgrade`]）只规定了**什么时候**可以切，但没有解决
//! 「切换的那一瞬间，两个方向上的数据分别往哪走」。这里有一个很容易写错的窗口：
//!
//! ```text
//!   发起方 A                                响应方 B
//!      |--- Commit -------------------------->|
//!      |                                      | (B 可能还有在途数据发往 A)
//!      |<-- CommitAck ------------------------|
//!      |                                      |
//!      X======== 双方切换 ====================X
//! ```
//!
//! 在 `Commit` 到 `CommitAck` 之间：
//!
//! * **A 不能再往老通道写**：B 收到 Commit 后马上就会切换，
//!   A 此时写进老通道的数据 B 读不到，会**静默丢失**。
//! * **A 必须继续从老通道读**：B 在收到 Commit 之前发出的数据还在路上，
//!   不排空就会丢。
//!
//! 所以需要：
//! 1. 进入提交窗口后，**应用数据的写请求先暂存**，等切到新通道再发；
//! 2. 读始终跟着当前阶段走（窗口内读老通道，切换后读新通道）；
//! 3. 升级失败要能把暂存的数据**倒回老通道**，不能凭空丢掉。
//!
//! 这些语义全部在本模块里，且有测试覆盖，因此连接层只需要构造它、
//! 在正确的时机调用三个 `on_*` 方法即可。

use crate::tcp::FramedStream;
use crate::ResultType;
use bytes::{Bytes, BytesMut};
use std::collections::VecDeque;

/// `TransitionStream` 当前处于哪个阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// 升级尚未提交：读写都在老通道。
    UsingOld,
    /// 已提交（发起方已发 Commit，尚未收到 CommitAck）：
    /// **写暂存**、**读仍在老通道**（排空对端在途数据）。
    CommitWindow,
    /// 已完成切换：读写都在新通道。
    UsingNew,
}

/// 把老通道与新的 iroh 通道串起来的过渡层。
///
/// 生命周期由三个 `on_*` 方法驱动，分别对应协议里的三个时刻：
///
/// | 时刻 | 角色 | 调用 |
/// |---|---|---|
/// | 发出 Commit 之后 | 发起方 | [`Self::on_commit_sent`] |
/// | 收到 CommitAck 之后 | 发起方 | [`Self::on_commit_ack_received`] |
/// | 发出 CommitAck 之后 | 响应方 | [`Self::on_commit_ack_sent`] |
///
/// 升级失败时调用 [`Self::abort`] 把暂存数据倒回老通道。
pub struct TransitionStream {
    old: crate::stream::Stream,
    new: FramedStream,
    phase: Phase,
    /// 提交窗口内暂存的应用数据（等切到新通道后按序发出）
    buffered: VecDeque<Bytes>,
    /// 统计：一共暂存过多少字节（便于诊断"卡了多久"）
    buffered_bytes: usize,
}

impl TransitionStream {
    pub fn new(old: crate::stream::Stream, new: FramedStream) -> Self {
        Self {
            old,
            new,
            phase: Phase::UsingOld,
            buffered: VecDeque::new(),
            buffered_bytes: 0,
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// 暂存中的数据条数（遥测用）。
    pub fn buffered_len(&self) -> usize {
        self.buffered.len()
    }

    pub fn buffered_bytes(&self) -> usize {
        self.buffered_bytes
    }

    /// 发起方：刚把 Commit 写到老通道上。
    ///
    /// 从这一刻起**不能再往老通道写应用数据**，否则会丢。
    pub fn on_commit_sent(&mut self) {
        if self.phase == Phase::UsingOld {
            self.phase = Phase::CommitWindow;
        }
    }

    /// 发起方：收到了 CommitAck。可以切到新通道并冲刷暂存数据。
    pub fn on_commit_ack_received(&mut self) {
        self.phase = Phase::UsingNew;
    }

    /// 响应方：刚把 CommitAck 写到老通道上。
    ///
    /// 响应方没有提交窗口：CommitAck 是它往老通道写的最后一条，
    /// 之后直接切到新通道（发起方在 Commit 之后也不会再写老通道）。
    pub fn on_commit_ack_sent(&mut self) {
        self.phase = Phase::UsingNew;
    }

    /// 升级失败：把暂存数据倒回老通道，恢复原状。
    ///
    /// 调用方需要接着调用一次 [`Self::flush_to_old`]（或直接继续 send），
    /// 这里只负责复位阶段。
    pub fn abort(&mut self) {
        self.phase = Phase::UsingOld;
    }

    /// 把暂存数据按序发到老通道（`abort` 之后使用）。
    pub async fn flush_to_old(&mut self) -> ResultType<()> {
        if self.phase != Phase::UsingOld {
            return Ok(());
        }
        while let Some(b) = self.buffered.pop_front() {
            self.buffered_bytes = self.buffered_bytes.saturating_sub(b.len());
            self.old.send_bytes(b).await?;
        }
        Ok(())
    }

    /// 把暂存数据按序发到新通道（切换到新通道之后使用）。
    pub async fn flush_to_new(&mut self) -> ResultType<()> {
        if self.phase != Phase::UsingNew {
            return Ok(());
        }
        while let Some(b) = self.buffered.pop_front() {
            self.buffered_bytes = self.buffered_bytes.saturating_sub(b.len());
            self.new.send_bytes(b).await?;
        }
        Ok(())
    }

    /// 发送应用数据。
    ///
    /// * `UsingOld`：直接走老通道
    /// * `CommitWindow`：**暂存**（不能走老通道，否则对端读不到）
    /// * `UsingNew`：直接走新通道
    pub async fn send_bytes(&mut self, bytes: Bytes) -> ResultType<()> {
        match self.phase {
            Phase::UsingOld => self.old.send_bytes(bytes).await,
            Phase::UsingNew => self.new.send_bytes(bytes).await,
            Phase::CommitWindow => {
                self.buffered_bytes += bytes.len();
                self.buffered.push_back(bytes);
                Ok(())
            }
        }
    }

    /// 读取对端数据。
    ///
    /// `UsingOld` 与 `CommitWindow` 都读老通道：窗口内必须继续排空
    /// 对端在 Commit 之前发出的在途数据。
    pub async fn next(&mut self) -> Option<Result<BytesMut, std::io::Error>> {
        match self.phase {
            Phase::UsingOld | Phase::CommitWindow => self.old.next().await,
            Phase::UsingNew => self.new.next().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytes_codec::BytesCodec;
    use crate::tcp::DynTcpStream;
    use std::net::SocketAddr;
    use tokio_util::codec::Framed;

    fn addr() -> SocketAddr {
        "127.0.0.1:23333".parse().unwrap()
    }

    /// 造一条基于内存管道的 `Stream`（等价于一条 TCP 连接）。
    fn mk_stream() -> (crate::stream::Stream, crate::stream::Stream) {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let sa = crate::stream::Stream::Tcp(FramedStream(
            Framed::new(DynTcpStream(Box::new(a)), BytesCodec::new()),
            addr(),
            None,
            0,
        ));
        let sb = crate::stream::Stream::Tcp(FramedStream(
            Framed::new(DynTcpStream(Box::new(b)), BytesCodec::new()),
            addr(),
            None,
            0,
        ));
        (sa, sb)
    }

    fn mk_framed() -> (FramedStream, FramedStream) {
        let (a, b) = tokio::io::duplex(64 * 1024);
        (
            FramedStream(
                Framed::new(DynTcpStream(Box::new(a)), BytesCodec::new()),
                addr(),
                None,
                0,
            ),
            FramedStream(
                Framed::new(DynTcpStream(Box::new(b)), BytesCodec::new()),
                addr(),
                None,
                0,
            ),
        )
    }

    /// 切换前：读写都走老通道。
    #[tokio::test]
    async fn before_commit_everything_uses_old() {
        let (old_a, mut old_b) = mk_stream();
        let (new_a, mut new_b) = mk_framed();
        let mut t = TransitionStream::new(old_a, new_a);
        assert_eq!(t.phase(), Phase::UsingOld);

        t.send_bytes(Bytes::from_static(b"hello")).await.unwrap();
        let got = old_b.next().await.unwrap().unwrap();
        assert_eq!(&got[..], b"hello");

        // 老通道上的数据也要能读到
        let mut old_a2 = old_b;
        old_a2.send_bytes(Bytes::from_static(b"world")).await.unwrap();
        let got = t.next().await.unwrap().unwrap();
        assert_eq!(&got[..], b"world");

        // 新通道此时不该有任何数据。
        // 注意：不能用 next().await.is_none() —— 只有对端关闭才会返回 None，
        // 那样会永久阻塞。必须用超时来断言"没有数据"。
        let r = tokio::time::timeout(std::time::Duration::from_millis(50), new_b.next()).await;
        assert!(r.is_err(), "切换前新通道不应有数据");
    }

    /// 提交窗口：写被暂存（**不能**走老通道），读仍然走老通道。
    #[tokio::test]
    async fn commit_window_buffers_writes_but_drains_old() {
        let (old_a, mut old_b) = mk_stream();
        let (new_a, mut new_b) = mk_framed();
        let mut t = TransitionStream::new(old_a, new_a);

        t.on_commit_sent();
        assert_eq!(t.phase(), Phase::CommitWindow);

        // 窗口内写应用数据 → 必须暂存
        t.send_bytes(Bytes::from_static(b"pending-1")).await.unwrap();
        t.send_bytes(Bytes::from_static(b"pending-2")).await.unwrap();
        assert_eq!(t.buffered_len(), 2);
        assert!(t.buffered_bytes() > 0);

        // 关键：老通道上**不该**出现这两个包
        let timeout = tokio::time::timeout(std::time::Duration::from_millis(50), old_b.next()).await;
        assert!(timeout.is_err(), "提交窗口内不得往老通道写应用数据");

        // 窗口内仍然能从老通道读到对端的在途数据（排空）
        old_b.send_bytes(Bytes::from_static(b"in-flight")).await.unwrap();
        let got = t.next().await.unwrap().unwrap();
        assert_eq!(&got[..], b"in-flight");

        // 切换后冲刷：暂存数据应完整落到新通道
        t.on_commit_ack_received();
        assert_eq!(t.phase(), Phase::UsingNew);
        t.flush_to_new().await.unwrap();
        assert_eq!(t.buffered_len(), 0);
        assert_eq!(t.buffered_bytes(), 0);
        let got = new_b.next().await.unwrap().unwrap();
        assert_eq!(&got[..], b"pending-1");
        let got = new_b.next().await.unwrap().unwrap();
        assert_eq!(&got[..], b"pending-2");
    }

    /// 响应方：发完 CommitAck 直接切到新通道，没有提交窗口。
    #[tokio::test]
    async fn responder_switches_right_after_ack() {
        let (old_a, mut old_b) = mk_stream();
        let (new_a, mut new_b) = mk_framed();
        let mut t = TransitionStream::new(old_a, new_a);

        t.send_bytes(Bytes::from_static(b"last-old")).await.unwrap();
        t.on_commit_ack_sent();
        assert_eq!(t.phase(), Phase::UsingNew);

        t.send_bytes(Bytes::from_static(b"first-new")).await.unwrap();
        let got = old_b.next().await.unwrap().unwrap();
        assert_eq!(&got[..], b"last-old");
        let got = new_b.next().await.unwrap().unwrap();
        assert_eq!(&got[..], b"first-new");
    }

    /// 升级失败：暂存数据必须能倒回老通道，不能丢。
    #[tokio::test]
    async fn abort_flushes_buffered_back_to_old() {
        let (old_a, mut old_b) = mk_stream();
        let (new_a, _new_b) = mk_framed();
        let mut t = TransitionStream::new(old_a, new_a);

        t.on_commit_sent();
        t.send_bytes(Bytes::from_static(b"buf-1")).await.unwrap();
        t.send_bytes(Bytes::from_static(b"buf-2")).await.unwrap();
        assert_eq!(t.buffered_len(), 2);

        t.abort();
        assert_eq!(t.phase(), Phase::UsingOld);
        t.flush_to_old().await.unwrap();
        assert_eq!(t.buffered_len(), 0);
        let got = old_b.next().await.unwrap().unwrap();
        assert_eq!(&got[..], b"buf-1");
        let got = old_b.next().await.unwrap().unwrap();
        assert_eq!(&got[..], b"buf-2");
    }

    /// 阶段推进必须是单向的，不能被乱序调用搞坏。
    #[tokio::test]
    async fn phase_transitions_are_monotonic() {
        let (old_a, _) = mk_stream();
        let (new_a, _) = mk_framed();
        let mut t = TransitionStream::new(old_a, new_a);

        // 还没提交就收到 Ack：不应误切
        t.on_commit_ack_received();
        assert_eq!(t.phase(), Phase::UsingNew, "Ack 到达即视为可切换");

        let (old_a, _) = mk_stream();
        let (new_a, _) = mk_framed();
        let mut t = TransitionStream::new(old_a, new_a);
        // 重复调用 on_commit_sent 不应改变已切换的状态
        t.on_commit_sent();
        t.on_commit_ack_received();
        t.on_commit_sent();
        assert_eq!(t.phase(), Phase::UsingNew, "已切换后不应被拉回提交窗口");
    }

    /// 切换之后读写都走新通道，老通道不再被使用。
    #[tokio::test]
    async fn after_switch_only_new_is_used() {
        let (old_a, mut old_b) = mk_stream();
        let (new_a, mut new_b) = mk_framed();
        let mut t = TransitionStream::new(old_a, new_a);

        t.on_commit_ack_sent();
        t.send_bytes(Bytes::from_static(b"n1")).await.unwrap();
        assert_eq!(&new_b.next().await.unwrap().unwrap()[..], b"n1");

        new_b.send_bytes(Bytes::from_static(b"n2")).await.unwrap();
        assert_eq!(&t.next().await.unwrap().unwrap()[..], b"n2");

        // 老通道上不应再有东西
        let r = tokio::time::timeout(std::time::Duration::from_millis(50), old_b.next()).await;
        assert!(r.is_err(), "切换后不应再往老通道写");
    }
}
