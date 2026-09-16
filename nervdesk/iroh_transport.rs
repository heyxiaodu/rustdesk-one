//! 用 iroh(QUIC) 承载 RustDesk 的点对点数据通道。
//!
//! 由 feature `iroh-transport` 控制，默认关闭。
//!
//! # 设计要点
//!
//! RustDesk 的传输抽象是 `crate::tcp::FramedStream`（`libs/hbb_common/src/tcp.rs:32`）：
//!
//! ```ignore
//! pub struct FramedStream(
//!     pub Framed<DynTcpStream, BytesCodec>,   // 长度前缀分帧
//!     pub SocketAddr,
//!     pub Option<Encrypt>,                    // secretbox 端到端加密
//!     pub u64,
//! );
//! ```
//!
//! 它内部只要求底层是 `AsyncRead + AsyncWrite`（见 `tcp.rs:28` 的 `TcpStreamTrait`），
//! **不关心**这个字节流来自 TCP、WebSocket 还是 QUIC。
//!
//! 所以本模块的全部工作就是：
//!
//! 1. 把 iroh 的一条 QUIC 双向流包装成 [`IrohDuplex`]（`AsyncRead + AsyncWrite`）；
//! 2. 用它构造出**真正的** `FramedStream`。
//!
//! 上层（视频 / 音频 / 剪贴板 / 文件传输 / 输入注入）一行都不用改。
//!
//! # 身份绑定
//!
//! iroh 的 `EndpointId` 本质就是一个 Ed25519 公钥（`iroh_base::PublicKey`），
//! 而 RustDesk 的设备身份也是一对 Ed25519 密钥（`Config::get_key_pair()`）。
//! 两者可以直接统一：用 RustDesk 私钥的种子派生出 iroh 的 `SecretKey`，
//! 于是 **iroh 的 EndpointId 就等于 RustDesk 的公钥 pk**。
//!
//! 见 [`secret_key_from_rustdesk`] 与测试 `identity_binding`。
//! 这样一来，后续 `secure_connection()` 里既有的 pk 校验
//! 就同时完成了对 iroh 对端身份的认证，不需要额外发明一套绑定机制。

use crate::bytes_codec::BytesCodec;
use crate::tcp::{DynTcpStream, FramedStream};
use crate::ResultType;
use anyhow::anyhow;
use bytes::Bytes;
use iroh::endpoint::{RecvStream, SendStream};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, SecretKey, TransportAddr, endpoint::presets};
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::OnceCell;
use tokio_util::codec::Framed;

/// ALPN 必须与其它 iroh 应用区分开，
/// 否则这个监听端口可能被当成通用 iroh 服务被滥用。
pub const ALPN: &[u8] = b"nervdesk/rustdesk-iroh-transport/0";

/// iroh 没有 IP 语义的 socket 地址，但 `FramedStream` 需要一个 `SocketAddr`。
///
/// 用这个固定哨兵值占位，好处是上层日志与统计能一眼认出「这是 iroh 连接」。
///
/// 已知待办（对应设计文档阶段 C）：RustDesk 有几处会用 `local_addr()` 做
/// NAT 类型推断与日志，接入竞速时需要把这些判断与 iroh 连接区分开。
pub fn sentinel_addr() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
}

// ===========================================================================
// 字节流适配
// ===========================================================================

/// 一条 iroh QUIC 双向流，表现为一个普通的异步字节流。
///
/// 语义上等价于 RustDesk 里的一个 TCP 连接。
pub struct IrohDuplex {
    send: SendStream,
    recv: RecvStream,
}

impl IrohDuplex {
    pub fn new(send: SendStream, recv: RecvStream) -> Self {
        Self { send, recv }
    }
}

impl AsyncRead for IrohDuplex {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // 必须用完全限定语法：noq 的 SendStream/RecvStream 除了实现 tokio 的
        // AsyncRead/AsyncWrite 之外，还带有返回自己错误类型的同名固有方法，
        // 直接 `.poll_read(..)` 会解析到固有方法而不匹配。
        AsyncRead::poll_read(Pin::new(&mut self.get_mut().recv), cx, buf)
    }
}

impl AsyncWrite for IrohDuplex {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        AsyncWrite::poll_write(Pin::new(&mut self.get_mut().send), cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_flush(Pin::new(&mut self.get_mut().send), cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_shutdown(Pin::new(&mut self.get_mut().send), cx)
    }
}

/// 把一条 iroh 双向流变成 RustDesk 的原生连接类型。
///
/// 这是本模块存在的全部意义：返回的 `FramedStream` 与 TCP 得到的那个
/// 在类型上完全一致，所以入口处只需要一个 `Stream::Tcp(...)` 包装。
pub fn framed_stream_from_iroh(send: SendStream, recv: RecvStream) -> FramedStream {
    let framed = Framed::new(
        DynTcpStream(Box::new(IrohDuplex::new(send, recv))),
        BytesCodec::new(),
    );
    // 第三个字段是端到端加密状态，由上层 secure_tcp/secure_connection 负责设置，
    // 这里给 None 与 FramedStream::new 的行为一致。
    FramedStream(framed, sentinel_addr(), None, 0)
}

// ===========================================================================
// 身份绑定
// ===========================================================================

/// 由 RustDesk 的 Ed25519 密钥对派生出 iroh 的 `SecretKey`。
///
/// `key_pair` 就是 `Config::get_key_pair()` 的返回值：
/// `(sk.0.to_vec(), pk.0.into())`，其中 `sk` 是 libsodium 的
/// 64 字节私钥（`seed(32) || public_key(32)`）。
/// iroh/ed25519-dalek 需要的是前 32 字节种子。
///
/// 因此派生出的 iroh 身份其公钥 == RustDesk 的 pk，实现天然绑定。
pub fn secret_key_from_rustdesk(key_pair: &(Vec<u8>, Bytes)) -> ResultType<SecretKey> {
    let sk = &key_pair.0;
    if sk.len() < 32 {
        return Err(anyhow!(
            "RustDesk 私钥长度异常: {} （期望 libsodium 的 64 字节）",
            sk.len()
        ));
    }
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&sk[..32]);
    Ok(SecretKey::from_bytes(&seed))
}

// ===========================================================================
// endpoint 生命周期
// ===========================================================================

/// iroh endpoint 的配置。
#[derive(Clone, Debug, Default)]
pub struct IrohConfig {
    /// 绑定的本地地址，例如 `"0.0.0.0:0"`。为空则用 `0.0.0.0:0`。
    pub bind_addr: Option<String>,
    /// iroh relay 地址。为空则 `RelayMode::Disabled`（纯直连）。
    ///
    /// 生产环境应指向自建的 iroh-relay，而不是 n0 的公共 relay。
    pub relay_urls: Vec<String>,
    /// RustDesk 的密钥对。给了就用它派生稳定的 iroh 身份。
    pub secret_key: Option<SecretKey>,
}

static ENDPOINT: OnceCell<Endpoint> = OnceCell::const_new();

/// 初始化（或取得已初始化的）iroh endpoint。
///
/// 进程内只初始化一次；重复调用会返回同一个 endpoint。
pub async fn init_endpoint(config: IrohConfig) -> ResultType<Endpoint> {
    let endpoint = ENDPOINT
        .get_or_try_init(|| async move { build_endpoint(config).await })
        .await?;
    Ok(endpoint.clone())
}

async fn build_endpoint(config: IrohConfig) -> ResultType<Endpoint> {
    let bind: SocketAddr = config
        .bind_addr
        .as_deref()
        .unwrap_or("0.0.0.0:0")
        .parse()
        .map_err(|e| anyhow!("iroh 绑定地址解析失败: {e}"))?;

    let mut builder = Endpoint::builder(presets::Minimal)
        .bind_addr(bind)
        .map_err(|e| anyhow!("iroh 绑定地址无效: {e}"))?
        .alpns(vec![ALPN.to_vec()]);

    if let Some(sk) = config.secret_key {
        builder = builder.secret_key(sk);
    }

    builder = if config.relay_urls.is_empty() {
        builder.relay_mode(RelayMode::Disabled)
    } else {
        let urls = config
            .relay_urls
            .iter()
            .map(|s| s.parse())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow!("iroh relay 地址解析失败: {e}"))?;
        builder.relay_mode(RelayMode::custom(urls))
    };

    let endpoint = builder
        .bind()
        .await
        .map_err(|e| anyhow!("iroh endpoint 启动失败: {e}"))?;
    log::info!(
        "iroh endpoint 已启动: id={} relay={}",
        endpoint.id(),
        if config.relay_urls.is_empty() {
            "disabled".to_owned()
        } else {
            config.relay_urls.join(",")
        }
    );
    Ok(endpoint)
}

/// 取得已初始化的全局 endpoint。
///
/// 生产环境下一个进程只有一个 RustDesk 身份，所以只维护一个 endpoint。
pub fn endpoint() -> ResultType<Endpoint> {
    ENDPOINT
        .get()
        .cloned()
        .ok_or_else(|| anyhow!("iroh endpoint 尚未初始化，请先调用 init_endpoint()"))
}

/// 本 endpoint 的 id（即 RustDesk pk，若按上文方式派生）。
pub fn endpoint_id() -> ResultType<EndpointId> {
    Ok(endpoint()?.id())
}

/// 本机实际监听到的 socket 地址（`bind_addr` 端口为 0 时用它取真实端口）。
pub fn bound_socket() -> ResultType<SocketAddr> {
    endpoint()?
        .bound_sockets()
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("iroh endpoint 没有绑定任何 socket"))
}

/// 构造一个只含直连 IP 地址的 `EndpointAddr`。
///
/// 真实接入时这个地址通过信令通道交换（见设计文档第 3 节），
/// 这里提供手工构造用于测试与内网直连场景。
pub fn addr_with_ip(endpoint: &Endpoint, sock: SocketAddr) -> EndpointAddr {
    let mut addrs = std::collections::BTreeSet::new();
    addrs.insert(TransportAddr::Ip(sock));
    EndpointAddr {
        id: endpoint.id(),
        addrs,
    }
}

// ===========================================================================
// 连接 / 接受
// ===========================================================================

/// 用全局 endpoint 连接到对端，返回 RustDesk 原生连接类型。
pub async fn connect(peer: EndpointAddr) -> ResultType<FramedStream> {
    let (stream, _peer_id) = connect_with_peer(peer).await?;
    Ok(stream)
}

/// 同 [`connect`]，但额外返回对端的 iroh `EndpointId`。
///
/// 上层应校验它与预期设备的 pk 一致（我们已让两者等同，见
/// [`secret_key_from_rustdesk`]），不一致则立即关闭连接。
pub async fn connect_with_peer(peer: EndpointAddr) -> ResultType<(FramedStream, EndpointId)> {
    connect_from(&endpoint()?, peer).await
}

/// 用指定的 endpoint 发起连接。
///
/// 单独暴露是为了一个进程内存在多个 endpoint 的场景（例如单元测试）；
/// 生产路径用 [`connect_with_peer`] 即可。
pub async fn connect_from(
    endpoint: &Endpoint,
    peer: EndpointAddr,
) -> ResultType<(FramedStream, EndpointId)> {
    let remote_id = peer.id;
    let conn = endpoint
        .connect(peer, ALPN)
        .await
        .map_err(|e| anyhow!("iroh 连接失败: {e}"))?;

    // 二次确认：握手后的远端身份必须与请求的一致。
    let actual = conn.remote_id();
    if actual != remote_id {
        conn.close(1u32.into(), b"endpoint id mismatch");
        return Err(anyhow!(
            "iroh 对端身份不一致: 期望 {remote_id}, 实际 {actual}"
        ));
    }

    let (send, recv) = conn
        .open_bi()
        .await
        .map_err(|e| anyhow!("iroh open_bi 失败: {e}"))?;
    Ok((framed_stream_from_iroh(send, recv), actual))
}

/// 接受一条对端发起的 iroh 连接。
///
/// 返回 `None` 表示 endpoint 已关闭。
pub async fn accept_one(endpoint: &Endpoint) -> ResultType<Option<(FramedStream, EndpointId)>> {
    let Some(incoming) = endpoint.accept().await else {
        return Ok(None);
    };
    let conn = incoming
        .accept()
        .map_err(|e| anyhow!("iroh 接受连接失败: {e}"))?
        .await
        .map_err(|e| anyhow!("iroh 握手失败: {e}"))?;
    let peer_id = conn.remote_id();
    let (send, recv) = conn
        .accept_bi()
        .await
        .map_err(|e| anyhow!("iroh accept_bi 失败: {e}"))?;
    Ok(Some((framed_stream_from_iroh(send, recv), peer_id)))
}

/// 持续接受 iroh 连接，把每条连接交给 `handler`。
///
/// 对应 `src/server.rs:273` 的 `accept_connection`：
/// 那里接收一个 `Stream`，所以这里产出的 `FramedStream` 直接
/// `Stream::Tcp(stream)` 包一下就能走同一条路径。
pub async fn run_accept_loop<F, Fut>(endpoint: Endpoint, handler: F) -> ResultType<()>
where
    F: Fn(FramedStream, EndpointId) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    loop {
        match accept_one(&endpoint).await {
            Ok(Some((stream, peer_id))) => {
                log::info!("iroh 已接受来自 {peer_id} 的连接");
                handler(stream, peer_id).await;
            }
            Ok(None) => {
                log::info!("iroh endpoint 已关闭，接受循环退出");
                return Ok(());
            }
            Err(e) => {
                // QUIC 监听在 UDP 上，任何可达的主机都能发包，
                // 因此握手失败是常态而不是致命错误，记录后继续。
                log::warn!("iroh 接受连接失败（已忽略）: {e}");
            }
        }
    }
}

/// 让编译器检查 `IrohDuplex` 满足 `FramedStream` 要求的 `Send + Sync`。
///
/// `DynTcpStream` 的定义是 `Box<dyn TcpStreamTrait + Send + Sync>`，
/// 如果这一条不成立，[`framed_stream_from_iroh`] 根本无法编译。
#[allow(dead_code)]
fn _assert_duplex_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<IrohDuplex>();
    assert_send_sync::<FramedStream>();
    assert_send_sync::<Arc<IrohDuplex>>();
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;

    /// iroh 身份与 RustDesk 身份必须是同一个公钥。
    ///
    /// 这是设计文档第 8 节「iroh EndpointId 必须与 pk 绑定」的落地验证：
    /// 我们不需要发明绑定机制，两者本来就是同一个 Ed25519 公钥。
    #[test]
    fn identity_binding() {
        use crate::sodiumoxide::crypto::sign;

        for _ in 0..8 {
            let (pk, sk) = sign::gen_keypair();
            let key_pair: (Vec<u8>, Bytes) = (sk.0.to_vec(), Bytes::copy_from_slice(&pk.0));

            let iroh_sk = secret_key_from_rustdesk(&key_pair).expect("派生 iroh 私钥失败");
            let iroh_pk = iroh_sk.public();

            assert_eq!(
                iroh_pk.as_bytes(),
                &pk.0,
                "iroh EndpointId 与 RustDesk pk 不一致"
            );
        }
    }

    /// 畸形私钥必须被拒绝，而不是 panic 或静默截断。
    #[test]
    fn identity_binding_rejects_short_key() {
        let bad: (Vec<u8>, Bytes) = (vec![1u8; 16], vec![0u8; 32].into());
        assert!(secret_key_from_rustdesk(&bad).is_err());
    }

    /// 端到端：两个真实的 iroh endpoint 之间，
    /// 用 **RustDesk 原生 `FramedStream`** 收发分帧消息。
    ///
    /// 这是 P1 的核心验收点——证明替换点确实只是一个字节流。
    #[tokio::test]
    async fn framed_stream_over_iroh_roundtrip() {
        // 服务端用全局 endpoint
        init_endpoint(IrohConfig {
            bind_addr: Some("127.0.0.1:0".to_owned()),
            ..Default::default()
        })
        .await
        .expect("初始化 endpoint 失败");
        let server_ep = endpoint().expect("取全局 endpoint 失败");
        let server_sock = bound_socket().expect("取绑定地址失败");
        let peer_addr = addr_with_ip(&server_ep, server_sock);

        // 客户端必须用**另一个** endpoint：iroh 明确禁止连接自己。
        let client_ep = build_endpoint(IrohConfig {
            bind_addr: Some("127.0.0.1:0".to_owned()),
            ..Default::default()
        })
        .await
        .expect("创建客户端 endpoint 失败");

        let cases: Vec<(u8, usize)> = vec![
            (1, 0),
            (2, 63),
            (3, 64),
            (4, 16_383),
            (5, 16_384),
            (6, 256 * 1024),
        ];
        let expect = cases.clone();

        // 服务端：接受一条连接，把收到的每条消息原样回显
        let server = tokio::spawn(async move {
            let (mut framed, _peer) = accept_one(&server_ep)
                .await
                .expect("accept 出错")
                .expect("endpoint 关闭");
            let mut n = 0usize;
            while let Some(item) = framed.next().await {
                let msg = match item {
                    Ok(m) => m,
                    Err(_) => break,
                };
                // 用 FramedStream 自带的 send_bytes（而非 Sink::send）：
                // 它上面有一个接收 protobuf Message 的固有 send 方法，会遮蔽 Sink trait。
                if framed.send_bytes(msg.freeze()).await.is_err() {
                    break;
                }
                n += 1;
            }
            n
        });

        // 客户端：用类型完全等同于 TCP 的 FramedStream 收发
        let (mut client, _peer) = connect_from(&client_ep, peer_addr)
            .await
            .expect("iroh 连接失败");
        for (tag, len) in &cases {
            let mut payload = Vec::with_capacity(len + 1);
            payload.push(*tag);
            payload.extend(std::iter::repeat((*tag % 251) as u8).take(*len));

            client
                .send_bytes(Bytes::from(payload))
                .await
                .expect("发送失败");

            let got: BytesMut = client
                .next()
                .await
                .expect("对端提前关闭")
                .expect("接收失败");
            assert_eq!(got.len(), len + 1, "tag={tag} 长度不符");
            assert_eq!(got[0], *tag, "tag={tag} 内容不符");
        }

        // 关掉客户端，服务端应能正常收敛
        drop(client);
        let echoed = server.await.expect("服务端任务 panic");
        assert_eq!(echoed, expect.len(), "服务端回显条数不符");
    }
}
