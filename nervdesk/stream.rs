use crate::{config, tcp, websocket, ResultType};
#[cfg(feature = "webrtc")]
use crate::webrtc;
use sodiumoxide::crypto::secretbox::Key;
use std::net::SocketAddr;
use tokio::net::TcpStream;

// support Websocket and tcp.
pub enum Stream {
    #[cfg(feature = "webrtc")]
    WebRTC(webrtc::WebRTCStream),
    WebSocket(websocket::WsFramedStream),
    Tcp(tcp::FramedStream),
    /// 传输正在从老通道切换到 iroh 通道（NervDesk 扩展，见 `iroh_transition`）。
    ///
    /// 加这个变体而不是在连接层到处判断，是为了让上层代码**一行都不用改**：
    /// `send` / `next` / `set_raw` 等调用照旧，由这里负责路由到正确的通道。
    #[cfg(feature = "iroh-transport")]
    Transition(Box<crate::iroh_transition::TransitionStream>),
    /// 空占位。仅在「把老流从结构体里换出来」的那一瞬间使用，
    /// 不可用于实际收发。
    #[cfg(feature = "iroh-transport")]
    Empty,
}

impl Stream {
    #[inline]
    pub fn set_send_timeout(&mut self, ms: u64) {
        match self {
            #[cfg(feature = "webrtc")]
            Stream::WebRTC(s) => s.set_send_timeout(ms),
            Stream::WebSocket(s) => s.set_send_timeout(ms),
            Stream::Tcp(s) => s.set_send_timeout(ms),
            #[cfg(feature = "iroh-transport")]
            Stream::Transition(s) => s.set_send_timeout(ms),
            #[cfg(feature = "iroh-transport")]
            Stream::Empty => {}
        }
    }

    #[inline]
    pub fn set_raw(&mut self) {
        match self {
            #[cfg(feature = "webrtc")]
            Stream::WebRTC(s) => s.set_raw(),
            Stream::WebSocket(s) => s.set_raw(),
            Stream::Tcp(s) => s.set_raw(),
            #[cfg(feature = "iroh-transport")]
            Stream::Transition(s) => s.set_raw(),
            #[cfg(feature = "iroh-transport")]
            Stream::Empty => {}
        }
    }

    #[inline]
    pub async fn send_bytes(&mut self, bytes: bytes::Bytes) -> ResultType<()> {
        match self {
            #[cfg(feature = "webrtc")]
            Stream::WebRTC(s) => s.send_bytes(bytes).await,
            Stream::WebSocket(s) => s.send_bytes(bytes).await,
            Stream::Tcp(s) => s.send_bytes(bytes).await,
            // Box::pin 是为了打破 Stream ↔ TransitionStream 的**类型级**异步递归。
            // 运行时不会真的嵌套（内层老流永远不是 Transition），
            // 而且迁移完成后调用方应立刻 collapse()，所以热路径上没有这个分配。
            #[cfg(feature = "iroh-transport")]
            Stream::Transition(s) => Box::pin(s.send_bytes(bytes)).await,
            #[cfg(feature = "iroh-transport")]
            Stream::Empty => Err(anyhow::anyhow!("stream 处于过渡占位状态，不能发送")),
        }
    }

    #[inline]
    pub async fn send_raw(&mut self, bytes: Vec<u8>) -> ResultType<()> {
        match self {
            #[cfg(feature = "webrtc")]
            Stream::WebRTC(s) => s.send_raw(bytes).await,
            Stream::WebSocket(s) => s.send_raw(bytes).await,
            Stream::Tcp(s) => s.send_raw(bytes).await,
            #[cfg(feature = "iroh-transport")]
            Stream::Transition(s) => Box::pin(s.send_raw(bytes)).await,
            #[cfg(feature = "iroh-transport")]
            Stream::Empty => Err(anyhow::anyhow!("stream 处于过渡占位状态，不能发送")),
        }
    }

    #[inline]
    pub fn set_key(&mut self, key: Key) {
        match self {
            #[cfg(feature = "webrtc")]
            Stream::WebRTC(s) => s.set_key(key),
            Stream::WebSocket(s) => s.set_key(key),
            Stream::Tcp(s) => s.set_key(key),
            #[cfg(feature = "iroh-transport")]
            Stream::Transition(s) => s.set_key(key),
            #[cfg(feature = "iroh-transport")]
            Stream::Empty => {}
        }
    }

    #[inline]
    pub fn is_secured(&self) -> bool {
        match self {
            #[cfg(feature = "webrtc")]
            Stream::WebRTC(s) => s.is_secured(),
            Stream::WebSocket(s) => s.is_secured(),
            Stream::Tcp(s) => s.is_secured(),
            #[cfg(feature = "iroh-transport")]
            Stream::Transition(s) => s.is_secured(),
            #[cfg(feature = "iroh-transport")]
            Stream::Empty => false,
        }
    }

    #[inline]
    pub async fn next_timeout(
        &mut self,
        timeout: u64,
    ) -> Option<Result<bytes::BytesMut, std::io::Error>> {
        match self {
            #[cfg(feature = "webrtc")]
            Stream::WebRTC(s) => s.next_timeout(timeout).await,
            Stream::WebSocket(s) => s.next_timeout(timeout).await,
            Stream::Tcp(s) => s.next_timeout(timeout).await,
            #[cfg(feature = "iroh-transport")]
            Stream::Transition(s) => Box::pin(s.next_timeout(timeout)).await,
            #[cfg(feature = "iroh-transport")]
            Stream::Empty => None,
        }
    }

    /// establish connect from websocket
    #[inline]
    pub async fn connect_websocket(
        url: impl AsRef<str>,
        local_addr: Option<SocketAddr>,
        proxy_conf: Option<&config::Socks5Server>,
        timeout_ms: u64,
    ) -> ResultType<Self> {
        let ws_stream =
            websocket::WsFramedStream::new(url, local_addr, proxy_conf, timeout_ms).await?;
        log::debug!("WebSocket connection established");
        Ok(Self::WebSocket(ws_stream))
    }

    /// send message
    #[inline]
    pub async fn send(&mut self, msg: &impl protobuf::Message) -> ResultType<()> {
        match self {
            #[cfg(feature = "webrtc")]
            Self::WebRTC(s) => s.send(msg).await,
            Self::WebSocket(ws) => ws.send(msg).await,
            Self::Tcp(tcp) => tcp.send(msg).await,
            #[cfg(feature = "iroh-transport")]
            Self::Transition(s) => Box::pin(s.send(msg)).await,
            #[cfg(feature = "iroh-transport")]
            Self::Empty => Err(anyhow::anyhow!("stream 处于过渡占位状态，不能发送")),
        }
    }

    /// receive message
    #[inline]
    pub async fn next(&mut self) -> Option<Result<bytes::BytesMut, std::io::Error>> {
        match self {
            #[cfg(feature = "webrtc")]
            Self::WebRTC(s) => s.next().await,
            Self::WebSocket(ws) => ws.next().await,
            Self::Tcp(tcp) => tcp.next().await,
            #[cfg(feature = "iroh-transport")]
            Self::Transition(s) => Box::pin(s.next()).await,
            #[cfg(feature = "iroh-transport")]
            Self::Empty => None,
        }
    }

    #[inline]
    pub fn local_addr(&self) -> SocketAddr {
        match self {
            #[cfg(feature = "webrtc")]
            Self::WebRTC(s) => s.local_addr(),
            Self::WebSocket(ws) => ws.local_addr(),
            Self::Tcp(tcp) => tcp.local_addr(),
            #[cfg(feature = "iroh-transport")]
            Self::Transition(s) => s.local_addr(),
            #[cfg(feature = "iroh-transport")]
            Self::Empty => {
                SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 0)
            }
        }
    }

    /// 迁移完成后把 `Transition` 收敛成一条普通流。
    ///
    /// 目的：让 `Transition` 变体**只存在于切换窗口内**，
    /// 稳态（切换完成之后）不再经过它，也就不承担上面 `Box::pin` 的分配。
    ///
    /// 阶段不是 `UsingNew` 时原样返回（说明还不能收敛）。
    #[cfg(feature = "iroh-transport")]
    pub fn collapse_transition(self) -> Self {
        match self {
            Self::Transition(t) if t.phase() == crate::iroh_transition::Phase::UsingNew => {
                Self::Tcp(t.into_new())
            }
            other => other,
        }
    }

    #[inline]
    pub fn from(stream: TcpStream, stream_addr: SocketAddr) -> Self {
        Self::Tcp(tcp::FramedStream::from(stream, stream_addr))
    }

    #[inline]
    #[cfg(feature = "webrtc")]
    pub fn get_webrtc_stream(&self) -> Option<webrtc::WebRTCStream> {
        match self {
            Self::WebRTC(s) => Some(s.clone()),
            _ => None,
        }
    }
}

#[cfg(all(test, feature = "iroh-transport"))]
mod transition_variant_tests {
    use super::*;
    use crate::bytes_codec::BytesCodec;
    use crate::iroh_transition::{Phase, TransitionStream};
    use crate::tcp::DynTcpStream;
    use tokio_util::codec::Framed;

    fn addr() -> SocketAddr {
        "127.0.0.1:23333".parse().unwrap()
    }

    fn mk_stream() -> (Stream, Stream) {
        let (a, b) = tokio::io::duplex(64 * 1024);
        (
            Stream::Tcp(tcp::FramedStream(
                Framed::new(DynTcpStream(Box::new(a)), BytesCodec::new()),
                addr(),
                None,
                0,
            )),
            Stream::Tcp(tcp::FramedStream(
                Framed::new(DynTcpStream(Box::new(b)), BytesCodec::new()),
                addr(),
                None,
                0,
            )),
        )
    }

    fn mk_framed() -> (tcp::FramedStream, tcp::FramedStream) {
        let (a, b) = tokio::io::duplex(64 * 1024);
        (
            tcp::FramedStream(
                Framed::new(DynTcpStream(Box::new(a)), BytesCodec::new()),
                addr(),
                None,
                0,
            ),
            tcp::FramedStream(
                Framed::new(DynTcpStream(Box::new(b)), BytesCodec::new()),
                addr(),
                None,
                0,
            ),
        )
    }

    /// 用 `Stream::Empty` 做占位把老流换出来，再装成 `Transition` ——
    /// 这就是连接层实际会做的两步。
    #[tokio::test]
    async fn swap_in_place_like_connection_layer_does() {
        let (old_a, mut old_b) = mk_stream();
        let (new_a, mut new_b) = mk_framed();

        // 模拟连接层的换流两步
        let mut slot = old_a;
        let old = std::mem::replace(&mut slot, Stream::Empty);
        slot = Stream::Transition(Box::new(TransitionStream::new(old, new_a)));

        // 走 Transition 变体的发送路径
        slot.send_bytes(bytes::Bytes::from_static(b"via-transition"))
            .await
            .unwrap();
        let got = old_b.next().await.unwrap().unwrap();
        assert_eq!(&got[..], b"via-transition");

        // 切换后应走新通道
        match &mut slot {
            Stream::Transition(t) => {
                t.on_commit_ack_sent();
                assert_eq!(t.phase(), Phase::UsingNew);
            }
            _ => panic!("应为 Transition 变体"),
        }
        slot.send_bytes(bytes::Bytes::from_static(b"via-new"))
            .await
            .unwrap();
        let got = new_b.next().await.unwrap().unwrap();
        assert_eq!(&got[..], b"via-new");
    }

    /// 占位状态必须"响亮地失败"，而不是静默丢数据。
    #[tokio::test]
    async fn empty_variant_fails_loudly() {
        let mut s = Stream::Empty;
        assert!(s
            .send_bytes(bytes::Bytes::from_static(b"x"))
            .await
            .is_err());
        assert!(s.next().await.is_none());
        assert!(!s.is_secured());
        s.set_raw();
        s.set_send_timeout(100);
        assert_eq!(s.local_addr().port(), 0);
    }
}
