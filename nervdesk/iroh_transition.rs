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

/// 提交窗口内暂存下来的一条待发数据。
///
/// # 为什么必须区分明文和已分帧字节
///
/// `send_raw` 走的是**未加密的明文**：加密发生在底层 `FramedStream` 上，
/// 用的是那条流自己的 nonce 计数器。
/// 如果在提交窗口内先把明文加密成密文再暂存，那么这些密文用的是**老流**的计数器；
/// 等切到新流再发出去时，新流的计数器已经被 `adopt_crypto_from` 继承过了，
/// 双方对"第几个包"的理解就会错位。
///
/// 所以提交窗口内必须暂存**明文**，等确定走哪条通道之后再交给那条通道加密。
///
/// 而 `send_bytes` 是已经分帧/加密过的字节（例如中继转发路径），
/// 只能在目标通道上原样发出，不能再加密一次。
#[derive(Debug, Clone)]
enum Pending {
    /// 明文，由目标通道自行加密后发出
    Plain(Vec<u8>),
    /// 已分帧字节，原样发出
    Raw(Bytes),
}

impl Pending {
    fn len(&self) -> usize {
        match self {
            Pending::Plain(v) => v.len(),
            Pending::Raw(b) => b.len(),
        }
    }
}

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
    buffered: VecDeque<Pending>,
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
        self.switch_to_new();
    }

    /// **切换的唯一入口**：把加密状态从老流**搬**到新流，然后改相位。
    ///
    /// 为什么必须是"搬"而不是"拷"：
    /// `FramedStream` 的加密状态是 `(key, 发送计数, 接收计数)`，那两个计数
    /// 就是 secretbox 的 nonce 序号。如果新老两条流**同时**持有同一份状态，
    /// 它们就会用同一组 `(key, nonce)` 加密两份不同的明文 —— 在
    /// XSalsa20-Poly1305 下这同时破坏机密性与完整性。
    ///
    /// 也不能"不搬"：新流的 `Encrypt` 是 `None`，直接切过去等于
    /// RustDesk 这一层的内嵌加密被静默关掉（只剩 QUIC 的 TLS），
    /// 而 `is_secured()` 还会继续报告 true —— 账实不符。
    ///
    /// 时序上两端是对齐的：Commit / CommitAck 本身走老通道（有序），
    /// 天然充当同步点。切换那一刻两端的计数都是"老通道已交互的条数"，
    /// 因此搬到新流后计数器可以直接接着用，不会跳号也不会重号。
    ///
    /// 重复调用是安全的：已经切过就直接返回，否则第二次 `take()`
    /// 会把新流的加密状态又清成 `None`。
    fn switch_to_new(&mut self) {
        if self.phase == Phase::UsingNew {
            return;
        }
        if let crate::stream::Stream::Tcp(old) = &mut self.old {
            self.new.2 = old.2.take();
        }
        self.phase = Phase::UsingNew;
    }

    /// 响应方：刚把 CommitAck 写到老通道上。
    ///
    /// 响应方没有提交窗口：CommitAck 是它往老通道写的最后一条，
    /// 之后直接切到新通道（发起方在 Commit 之后也不会再写老通道）。
    pub fn on_commit_ack_sent(&mut self) {
        self.switch_to_new();
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
        while let Some(item) = self.buffered.pop_front() {
            self.buffered_bytes = self.buffered_bytes.saturating_sub(item.len());
            match item {
                Pending::Plain(v) => self.old.send_raw(v).await?,
                Pending::Raw(b) => self.old.send_bytes(b).await?,
            }
        }
        Ok(())
    }

    /// 把暂存数据按序发到新通道（切换到新通道之后使用）。
    pub async fn flush_to_new(&mut self) -> ResultType<()> {
        if self.phase != Phase::UsingNew {
            return Ok(());
        }
        while let Some(item) = self.buffered.pop_front() {
            self.buffered_bytes = self.buffered_bytes.saturating_sub(item.len());
            match item {
                Pending::Plain(v) => self.new.send_raw(v).await?,
                Pending::Raw(b) => self.new.send_bytes(b).await?,
            }
        }
        Ok(())
    }

    /// 发送**已分帧**字节（如中继转发路径）。
    pub async fn send_bytes(&mut self, bytes: Bytes) -> ResultType<()> {
        match self.phase {
            Phase::UsingOld => self.old.send_bytes(bytes).await,
            Phase::UsingNew => self.new.send_bytes(bytes).await,
            Phase::CommitWindow => {
                self.buffered_bytes += bytes.len();
                self.buffered.push_back(Pending::Raw(bytes));
                Ok(())
            }
        }
    }

    /// 发送**明文**（业务数据，由目标通道负责加密）。
    ///
    /// 提交窗口内暂存的是明文而不是密文，原因见 [`Pending`] 的说明。
    pub async fn send_raw(&mut self, msg: Vec<u8>) -> ResultType<()> {
        match self.phase {
            Phase::UsingOld => self.old.send_raw(msg).await,
            Phase::UsingNew => self.new.send_raw(msg).await,
            Phase::CommitWindow => {
                self.buffered_bytes += msg.len();
                self.buffered.push_back(Pending::Plain(msg));
                Ok(())
            }
        }
    }

    /// 发送 protobuf 消息（等价于 `Stream::send`）。
    pub async fn send(&mut self, msg: &impl protobuf::Message) -> ResultType<()> {
        self.send_raw(msg.write_to_bytes()?).await
    }

    /// 把 raw 模式同时应用到两条通道（端口转发场景会用到）。
    pub fn set_raw(&mut self) {
        self.old.set_raw();
        self.new.set_raw();
    }

    /// 把发送超时同时应用到两条通道。
    pub fn set_send_timeout(&mut self, ms: u64) {
        self.old.set_send_timeout(ms);
        self.new.set_send_timeout(ms);
    }

    /// 是否已启用加密。
    ///
    /// 必须看**当前生效**的那条通道：切换那一刻加密状态会从老流**搬**到新流
    /// （见 [`Self::switch_to_new`]），此后若还去问老流，就会谎报"未加密"。
    pub fn is_secured(&self) -> bool {
        match self.phase {
            Phase::UsingNew => self.new.is_secured(),
            _ => self.old.is_secured(),
        }
    }

    /// 新通道当前的 `(发送计数, 接收计数)`；未切换时通常为 `None`。
    ///
    /// 给遥测与测试用：这是验证「加密状态真的搬过来了」的唯一窗口。
    pub fn new_crypto_seq(&self) -> Option<(u64, u64)> {
        self.new.crypto_seq()
    }

    pub fn local_addr(&self) -> std::net::SocketAddr {
        self.old.local_addr()
    }

    /// 兼容 `Stream::set_key`：切换过程中的密钥应设置在新通道上。
    pub fn set_key(&mut self, key: sodiumoxide::crypto::secretbox::Key) {
        self.old.set_key(key.clone());
        self.new.set_key(key);
    }

    /// 取出新通道（用于迁移完成后把 `Stream::Transition` 收敛成普通流）。
    ///
    /// 只有在 `UsingNew` 阶段调用才安全；否则会把老通道上还没排空的数据丢掉。
    pub fn into_new(self) -> FramedStream {
        self.new
    }

    /// 带超时的读取（对应 `Stream::next_timeout`）。
    pub async fn next_timeout(
        &mut self,
        ms: u64,
    ) -> Option<Result<BytesMut, std::io::Error>> {
        if let Ok(res) = crate::timeout(ms, self.next()).await {
            res
        } else {
            None
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

    /// 切换的那一刻，加密状态必须从老流**搬**到新流。
    ///
    /// 两个方向都错的做法：
    ///   * **不搬**：新流的 `Encrypt` 是 `None`，RustDesk 这层的内嵌加密
    ///     被静默关掉（只剩 QUIC 的 TLS），而 `is_secured()` 还会报 true。
    ///   * **拷**（新老同时持有）：两条流会用同一组 `(key, nonce)` 加密
    ///     不同明文 —— XSalsa20-Poly1305 下同时破坏机密性与完整性。
    ///
    /// 所以这里同时钉死三件事：切换前新流不能有加密状态、切换后计数原样继承、
    /// 重复切换不能把状态又清掉。
    #[tokio::test]
    async fn switch_moves_crypto_state_from_old_to_new() {
        use crate::tcp::Encrypt;
        use sodiumoxide::crypto::secretbox;

        let (mut old_a, _old_b) = mk_stream();
        let (new_a, _new_b) = mk_framed();

        // 给"老流"装上加密状态，假装它已经用过这些 nonce
        if let crate::stream::Stream::Tcp(f) = &mut old_a {
            f.2 = Some(Encrypt(secretbox::Key([7u8; 32]), 41, 42));
        }
        assert!(old_a.is_secured(), "老流应当已加密");

        let mut t = TransitionStream::new(old_a, new_a);
        assert!(
            t.new_crypto_seq().is_none(),
            "切换之前新流不能持有加密状态（提前复制 = nonce 复用）"
        );

        t.on_commit_sent();
        t.on_commit_ack_received(); // 发起方：收到 CommitAck -> 换流

        assert_eq!(t.phase(), Phase::UsingNew);
        assert_eq!(
            t.new_crypto_seq(),
            Some((41, 42)),
            "加密状态（含 nonce 计数）必须原样继承到新流"
        );
        assert!(t.is_secured(), "换流之后仍应报告已加密");

        // 重复调用不能把刚搬过来的状态又清成 None
        t.on_commit_ack_received();
        assert_eq!(
            t.new_crypto_seq(),
            Some((41, 42)),
            "重复切换不该清空加密状态"
        );
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

    /// 核心语义验证：提交窗口内暂存的**明文**，切换后必须由新通道加密发出，
    /// 且双方 nonce 计数器要对齐（否则对端解不开）。
    ///
    /// 这正是"暂存明文而不是先加密再暂存"这个设计选择要保证的性质。
    #[tokio::test]
    async fn buffered_plaintext_is_encrypted_by_the_new_channel() {
        use sodiumoxide::crypto::secretbox;
        let key = secretbox::gen_key();

        // 老通道：双方设同一把钥匙，并收发一条以推进 nonce 计数器
        let (mut old_a, mut old_b) = mk_stream();
        old_a.set_key(key.clone());
        old_b.set_key(key.clone());
        old_a.send_raw(b"before".to_vec()).await.unwrap();
        assert_eq!(&old_b.next().await.unwrap().unwrap()[..], b"before");

        // 新通道：从老通道继承加密状态
        let (mut new_a, mut new_b) = mk_framed();
        match &old_a {
            crate::stream::Stream::Tcp(f) => new_a.adopt_crypto_from(f),
            _ => panic!("测试里只造 Tcp 流"),
        }
        match &old_b {
            crate::stream::Stream::Tcp(f) => new_b.adopt_crypto_from(f),
            _ => panic!("测试里只造 Tcp 流"),
        }

        let mut trans = TransitionStream::new(old_a, new_a);
        trans.on_commit_sent();
        // 提交窗口内暂存明文
        trans.send_raw(b"secret".to_vec()).await.unwrap();
        assert_eq!(trans.buffered_len(), 1);

        trans.on_commit_ack_received();
        trans.flush_to_new().await.unwrap();

        // 对端用继承过状态的新通道解密 —— 计数器对齐才可能成功
        let got = new_b.next().await.unwrap().unwrap();
        assert_eq!(&got[..], b"secret", "暂存的明文应由新通道加密且计数器对齐");
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
