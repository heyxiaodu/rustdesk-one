//! 切换底层传输时的加密状态迁移。
//!
//! 由 feature `iroh-transport` 控制（仅在 iroh 版构建中编译）。
//!
//! # 为什么需要这个模块
//!
//! 把点对点数据通道换成 iroh/QUIC 时，采用的策略是
//! 「**先用老通道建连，再在连接中途换到 iroh**」——
//! 因为只有这条路不需要改动 hbbs/hbbr（见设计文档第 13 节）。
//!
//! 但 `FramedStream` 身上带着加密状态：
//!
//! ```ignore
//! pub struct FramedStream(
//!     pub Framed<DynTcpStream, BytesCodec>,
//!     pub SocketAddr,
//!     pub Option<Encrypt>,   // ← 加密状态在这里
//!     pub u64,
//! );
//!
//! pub struct Encrypt(pub Key, pub u64, pub u64);
//! //                        ^密钥   ^发送计数  ^接收计数
//! ```
//!
//! 那两个 `u64` 是 secretbox 的 **nonce 序号**：
//! `Encrypt::enc()` 把 `.1` 自增后用它构造 nonce，
//! `Encrypt::dec()` 把 `.2` 自增后用它构造 nonce。
//!
//! 所以换流时如果**只把底层字节流换掉、把计数器留在 (0, 0)**，
//! 而密钥不变，就会出现**同一 `(key, nonce)` 加密两条不同明文**。
//! 在 XSalsa20-Poly1305 下这会同时破坏机密性与完整性 —— 不可接受。
//!
//! 因此换流必须把 `(密钥, 发送计数, 接收计数)` 原样继承。
//! 本模块提供的三个方法就是这个迁移动作，
//! 并用测试把「不迁移 = nonce 复用」这个坑钉死。

use crate::tcp::{Encrypt, FramedStream};

impl FramedStream {
    /// 把已有的加密状态迁移到这条（新的）流上。
    ///
    /// 用于传输切换：老流 → 新流。
    pub fn adopt_crypto_from(&mut self, old: &FramedStream) {
        self.2 = old.2.clone();
    }

    /// 取走当前加密状态（迁移到新流时使用）。
    ///
    /// 取走后本流不再加密，因此调用方必须保证这条流不再用于收发业务数据。
    pub fn take_crypto(&mut self) -> Option<Encrypt> {
        self.2.take()
    }

    /// 当前的 (发送计数, 接收计数)，用于测试与遥测。
    pub fn crypto_seq(&self) -> Option<(u64, u64)> {
        self.2.as_ref().map(|e| (e.1, e.2))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytes_codec::BytesCodec;
    use crate::tcp::DynTcpStream;
    use sodiumoxide::crypto::secretbox;
    use std::net::SocketAddr;
    use tokio_util::codec::Framed;

    fn mk_pair(key: Option<secretbox::Key>) -> (FramedStream, FramedStream) {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let addr: SocketAddr = "127.0.0.1:23333".parse().unwrap();
        let mut sa = FramedStream(
            Framed::new(DynTcpStream(Box::new(a)), BytesCodec::new()),
            addr,
            None,
            0,
        );
        let mut sb = FramedStream(
            Framed::new(DynTcpStream(Box::new(b)), BytesCodec::new()),
            addr,
            None,
            0,
        );
        if let Some(k) = key {
            sa.set_key(k.clone());
            sb.set_key(k);
        }
        (sa, sb)
    }

    /// 把 nonce 复用的危害钉死。
    ///
    /// 判据：secretbox 对相同的 (密钥, nonce, 明文) 是确定性的，
    /// 所以**两条独立的新流对同一明文给出相同密文**，
    /// 就等价于「同一个 (key, nonce) 被用了两次」。
    #[test]
    fn resetting_counters_reuses_nonces() {
        let key = secretbox::gen_key();

        let mut a = Encrypt::new(key.clone());
        let c1 = a.enc(b"same-plaintext");

        // 忘了继承、直接新建：计数器从 0 开始
        let mut b = Encrypt::new(key.clone());
        let c2 = b.enc(b"same-plaintext");
        assert_eq!(
            c1, c2,
            "两条独立新流对同一明文给出相同密文 ⇒ 同一 (key, nonce) 被复用了"
        );

        // 正确做法：继承计数器，nonce 继续推进
        let mut c = Encrypt(key.clone(), a.1, a.2);
        let c3 = c.enc(b"same-plaintext");
        assert_ne!(c1, c3, "继承计数器后 nonce 推进，密文应当不同");
    }

    /// 端到端：换流后通信不中断，且计数器延续（不归零）。
    #[tokio::test]
    async fn swap_inherits_counters_and_keeps_working() {
        let key = secretbox::gen_key();

        // 老传输上收发 3 条
        let (mut old_a, mut old_b) = mk_pair(Some(key.clone()));
        for i in 0..3u8 {
            // 必须用 send_raw：它会走 key.enc 并推进发送计数器；
            // send_bytes 是「已分帧」的旁路，既不加密也不推进计数器。
            old_a.send_raw(vec![i]).await.unwrap();
            let got = old_b.next().await.unwrap().unwrap();
            assert_eq!(got[0], i);
        }
        assert_eq!(old_a.crypto_seq(), Some((3, 0)), "A 应已发出 3 条");
        assert_eq!(old_b.crypto_seq(), Some((0, 3)), "B 应已收到 3 条");

        // 换流：新建一对（模拟 iroh 通道），继承加密状态
        let (mut new_a, mut new_b) = mk_pair(None);
        new_a.adopt_crypto_from(&old_a);
        new_b.adopt_crypto_from(&old_b);

        assert_eq!(
            new_a.crypto_seq(),
            Some((3, 0)),
            "新流必须延续计数器，不能归零"
        );
        assert_eq!(new_b.crypto_seq(), Some((0, 3)));

        // 新通道上继续通信，计数器应推进到 4
        new_a.send_raw(b"after-swap".to_vec()).await.unwrap();
        let got = new_b.next().await.unwrap().unwrap();
        assert_eq!(&got[..], b"after-swap");
        assert_eq!(new_a.crypto_seq(), Some((4, 0)));
        assert_eq!(new_b.crypto_seq(), Some((0, 4)));
    }

    /// `take_crypto` 取走后本流不再加密，新流接手后仍能正常通信。
    #[tokio::test]
    async fn take_then_adopt_moves_state_atomically() {
        let key = secretbox::gen_key();
        let (mut old_a, mut old_b) = mk_pair(Some(key.clone()));
        old_a.send_raw(b"m1".to_vec()).await.unwrap();
        old_b.next().await.unwrap().unwrap();

        let (mut new_a, mut new_b) = mk_pair(None);
        let taken = old_a.take_crypto().expect("应当有加密状态");
        assert!(
            old_a.crypto_seq().is_none(),
            "取走后老流不应再有加密状态"
        );
        assert_eq!((taken.1, taken.2), (1, 0));
        new_a.2 = Some(taken);
        new_b.adopt_crypto_from(&old_b);

        new_a.send_raw(b"m2".to_vec()).await.unwrap();
        let got = new_b.next().await.unwrap().unwrap();
        assert_eq!(&got[..], b"m2");
        assert_eq!(new_a.crypto_seq(), Some((2, 0)));
        assert_eq!(new_b.crypto_seq(), Some((0, 2)));
    }
}
