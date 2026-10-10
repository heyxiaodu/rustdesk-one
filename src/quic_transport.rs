//! 2a（Path B）：把 QUIC 双向流焊进 RustDesk 的 `FramedStream` 接缝。
//!
//! 目标接缝：`libs/hbb_common/src/tcp.rs:169` 的
//! `FramedStream::from(stream, addr)` 要求 `stream: impl TcpStreamTrait + Send + Sync + 'static`，
//! 而 `tcp.rs:339` 的空泛实现 `impl<R: AsyncRead + AsyncWrite + Unpin> TcpStreamTrait for R {}`
//! 让「任何 `AsyncRead + AsyncWrite + Unpin` 类型」自动满足 `TcpStreamTrait`。
//! 因此把一个 `quinn` 双向流经 `tokio::io::join` 焊成双工对象后，即可按
//! `Stream::Tcp` 的形态交给上层 —— `libs/hbb_common` **零改动**
//! （`docs/11-phase1-plan.md` §8.1 的硬验收）。
//!
//! 边界（坦诚声明）：Path B 把 QUIC 塌缩成一条双向字节流，语义与 TCP/KCP 相同，
//! **不**交付 stream 多路复用（那是 2b / Path A 的目标）。
//!
//! 本模块受 `quic` feature 门控；默认构建完全不编译它。

use hbb_common::{
    tcp::FramedStream,
    tokio::io::{AsyncRead, AsyncWrite, ReadBuf},
};
use quinn::{ClientConfig as QuinnClientConfig, RecvStream, SendStream, ServerConfig as QuinnServerConfig};
#[cfg(feature = "quic")]
use {
    quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig},
    quinn::udp::{RecvMeta, Transmit},
    quinn::{default_runtime, AsyncUdpSocket, EndpointConfig, UdpPoller},
    ring::rand::SystemRandom,
    ring::signature::{Ed25519KeyPair, KeyPair},
    rustls::client::danger::{
        HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
    },
    rustls::crypto::{
        verify_tls13_signature_with_raw_key, CryptoProvider, WebPkiSupportedAlgorithms,
    },
    rustls::pki_types::{
        CertificateDer, PrivatePkcs8KeyDer, ServerName, SubjectPublicKeyInfoDer, UnixTime,
    },
    rustls::server::AlwaysResolvesServerRawPublicKeys,
    rustls::sign::CertifiedKey,
    rustls::{ClientConfig, DigitallySignedStruct, Error, ServerConfig, SignatureScheme},
    rustls::{crypto::ring::default_provider, crypto::ring::sign::any_eddsa_type},
    rustls::version::TLS13,
};
use std::{
    io::{self, IoSlice},
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

// ---------- P0-2：默认关闭的「最近一条连接句柄」诊断槽（plan.md §12）----------
//
// 为什么需要：`quinn::Connection` 是唯一能取到 RTT / 丢包 / cwnd / 字节数的句柄，而产品路径
// 在返回 `FramedStream` 时立刻把它 drop（缺口 G1/G2）。但**多留一个句柄会推迟
// CONNECTION_CLOSE**：quinn 只在最后一个句柄 drop 时才关闭连接，于是对端可能长时间看到半开
// 连接，那条已打洞的 UDP socket 也会多占用一段空闲超时。所以默认**不保留**：
//
//   NERV_QUIC_KEEPALIVE 未设 / 非 "1" / 非 "true" ⇒ 与改动前逐字一致（句柄照旧立即 drop）
//   NERV_QUIC_KEEPALIVE=1（或 true）            ⇒ 在返回前把句柄存进下面的槽
//
// 已知限界（同步登记在 `docs/NERV_DESK_STATUS.md`）：① 开启会延长连接存活；② 槽只保留**最近
// 一条**，并发两条会话会互相覆盖；③ 仅供诊断，不得作为产品行为依赖。
#[cfg(feature = "quic")]
static LAST_CONN: std::sync::OnceLock<std::sync::Mutex<Option<quinn::Connection>>> =
    std::sync::OnceLock::new();

#[cfg(feature = "quic")]
fn keepalive_enabled() -> bool {
    matches!(
        std::env::var("NERV_QUIC_KEEPALIVE").map(|v| v.trim().to_ascii_lowercase()),
        Ok(v) if v == "1" || v == "true"
    )
}

#[cfg(feature = "quic")]
fn last_conn_slot() -> &'static std::sync::Mutex<Option<quinn::Connection>> {
    LAST_CONN.get_or_init(|| std::sync::Mutex::new(None))
}

/// 开关开启时保留最近一条连接句柄；默认关闭时是**空操作**（调用点不需要判断开关）。
#[cfg(feature = "quic")]
fn retain_conn_for_stats(conn: &quinn::Connection) {
    if !keepalive_enabled() {
        return;
    }
    if let Ok(mut slot) = last_conn_slot().lock() {
        *slot = Some(conn.clone());
    }
}

/// P1-stats 的取值函数：把最近一条被保留的连接的通路统计拼成**一行**（无连接时 `None`）。
///
/// 抽成独立函数是为了让 `--quic-pair-mode` 也能把它打到 **stdout**（无头脚本好抓），而产品
/// 路径只走 `log_quic_stats`。
#[cfg(feature = "quic")]
pub fn quic_stats_line(role: &str) -> Option<String> {
    let conn = last_conn_slot().lock().ok().and_then(|g| (*g).clone())?;
    let s = conn.stats();
    Some(format!(
        "QUIC-STATS role={role} rtt_ms={} cwnd={} lost_pkts={} lost_bytes={} tx_bytes={} rx_bytes={} tx_dgrams={} rx_dgrams={} cong_events={}",
        s.path.rtt.as_millis(),
        s.path.cwnd,
        s.path.lost_packets,
        s.path.lost_bytes,
        s.udp_tx.bytes,
        s.udp_rx.bytes,
        s.udp_tx.datagrams,
        s.udp_rx.datagrams,
        s.path.congestion_events,
    ))
}

/// P1-stats：把上面那一行写进日志（每次会话调用一次）。
///
/// 开关关闭（槽为空）时**只在进程内首次**打印一行提醒，避免每次连接都刷日志。
/// 这是「建立时刻的快照」，不是曲线；1Hz 轮询属后续可选增强。
#[cfg(feature = "quic")]
pub fn log_quic_stats(role: &str) {
    match quic_stats_line(role) {
        Some(line) => hbb_common::log::info!("{line}"),
        None => {
            static WARNED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                hbb_common::log::info!(
                    "QUIC-STATS unavailable (NERV_QUIC_KEEPALIVE off)：未保留连接句柄，取不到 RTT/丢包/cwnd"
                );
            }
        }
    }
}

/// QUIC 发送 + 接收两条单向流焊成的单一双工对象。
///
/// `tokio::io::join(reader, writer)` 是最小融合：得到的 `Join<R, W>` 在 `R`
/// 满足 `AsyncRead` 处是 `AsyncRead`，在 `W` 满足 `AsyncWrite` 处是 `AsyncWrite`，
/// 且两者均 `Unpin` 时 `Join` 也 `Unpin`。
pub struct QuicDuplex(hbb_common::tokio::io::Join<RecvStream, SendStream>);

impl QuicDuplex {
    /// 用一条 QUIC 双向流的两个半部构造双工对象。
    pub fn new(recv: RecvStream, send: SendStream) -> Self {
        Self(hbb_common::tokio::io::join(recv, send))
    }
}

impl AsyncRead for QuicDuplex {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}

impl AsyncWrite for QuicDuplex {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.0.is_write_vectored()
    }
}

/// 2a 的落点：把 QUIC 双向流交给真实的 `FramedStream`。
///
/// 返回的 `FramedStream` 与 TCP 路径拿到的是**同一个类型** —— 上层随后照常
/// `set_key` / `send_raw` / `next`，E2E secretbox 链不变。
pub fn quic_into_framed_stream(
    recv: RecvStream,
    send: SendStream,
    addr: SocketAddr,
) -> FramedStream {
    let duplex = QuicDuplex::new(recv, send);
    FramedStream::from(duplex, addr)
}

/// rustls 0.23 要求在任何配置存在之前安装进程级 crypto provider。
///
/// ring provider 已在 P1a 验证可在 rustc 1.75 的 `windows-msvc` 目标编译，
/// 且不自带 aws-lc-rs 的 C 工具链负担（`repos/rustdesk/Cargo.toml:91` 的
/// reqwest 只启用 `rustls-tls`，因此原本就不拉 aws-lc-rs）。
pub fn install_ring_provider() {
    let _ = quinn::rustls::crypto::ring::default_provider().install_default();
}

// ---------- 2a-3（task-27，docs/11 §9）：QUIC_MODE 三档开关的传输选择点接线 ----------

/// QUIC 失败后的处置判定：`connect()` 与行为矩阵共用同一逻辑。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuicFailure {
    /// `prefer`：记 `warn` 并回退到原传输（TCP/KCP/WebRTC/relay）。
    Fallback,
    /// `required`：把真实错误如实上报，不回退。
    RealError,
}

pub fn quic_failure_disposition(mode: crate::common::QuicMode) -> QuicFailure {
    match mode {
        crate::common::QuicMode::Prefer => QuicFailure::Fallback,
        crate::common::QuicMode::Required => QuicFailure::RealError,
        // `disabled` 永不发起 QUIC 尝试（`connect()` 直接走原路径），此分支只是穷尽
        // 枚举；按回退处置与「不尝试」语义等价。
        crate::common::QuicMode::Disabled => QuicFailure::Fallback,
    }
}

// ---------- R4-3（task-15）：让 quinn 骑在 RustDesk 自己打洞出来的 UDP socket 上 ----------
//
// 设计依据与取舍全文见 `analysis/network/quic-server-wiring.md`。三条硬事实：
//
// 1. **NAT 映射是打洞 socket 的源四元组**（`docs/00-current-state.md` §8 R3）。因此 QUIC 端点
//    不能自己 `Endpoint::client(local_addr)` 新开 socket（会从没打洞的源端口出去），必须复用
//    `rendezvous_mediator::udp_nat_listen`（被控端）/ `client::udp_nat_connect`（主控端）
//    手里那个**已被 punch_udp 打过洞的** socket，与 KCP 互斥。
// 2. quinn 只接受 `AsyncUdpSocket`，所以需要下面这层 tokio 适配（quinn-0.11.9/src/runtime.rs:44）。
//    tokio 1.44.2 提供 `poll_send_ready` / `try_send_to` / `poll_recv_from`，无需任何新依赖。
// 3. 打洞 socket 是 `connect()` 过的单对端 socket；quinn 只会发往它从 `Endpoint::connect`
//    学到的那个地址，因此 `try_send_to` 到该地址与 `send` 等价（Linux/Windows 同）。
//
// 已知代价（诚实声明）：`punch_udp` 的监听侧在"对端第一个真实数据包"上返回并**消费**该包
// （`src/common.rs:2810-2818` 的设计），而 quinn 0.11.9 的 `Endpoint` **没有**把数据报注回
// 协议栈的公有 API（`endpoint.rs` 无 `pub fn handle`）。所以那第一个 QUIC Initial 会被丢弃，
// 由客户端按 PTO 重传才能开始握手；重传时刻由下面的 `QUIC_INITIAL_RTT` 钉住，
// 不再取 quinn 默认的 999ms（那已经吃满 `client.rs` 给这条路径的全部预算）。
#[cfg(feature = "quic")]
#[derive(Debug)]
struct TokioAsyncUdpSocket {
    socket: Arc<hbb_common::tokio::net::UdpSocket>,
}

#[cfg(feature = "quic")]
#[derive(Debug)]
struct TokioUdpPoller {
    socket: Arc<hbb_common::tokio::net::UdpSocket>,
}

#[cfg(feature = "quic")]
impl UdpPoller for TokioUdpPoller {
    fn poll_writable(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        // 把 quinn 的写就绪等待直接接到 tokio 的 reactor 上；`poll_send_ready` 可以无限次
        // 复用（UdpPoller 的契约要求如此），不需要自己缓存 future/waker。
        self.socket.poll_send_ready(cx)
    }
}

#[cfg(feature = "quic")]
impl AsyncUdpSocket for TokioAsyncUdpSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(TokioUdpPoller {
            socket: self.socket.clone(),
        })
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        match self
            .socket
            .try_send_to(transmit.contents, transmit.destination)
        {
            Ok(_) => Ok(()),
            // quinn 的契约：WouldBlock 时必须让 `UdpPoller::poll_writable` 去登记唤醒。
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "QUIC udp send would block",
            )),
            Err(e) => Err(e),
        }
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [std::io::IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        // `max_receive_segments()` 取默认值 1，所以一次只填一个 buffer / 一条 RecvMeta。
        let (Some(buf), Some(meta0)) = (bufs.first_mut(), meta.first_mut()) else {
            return Poll::Ready(Ok(0));
        };
        let mut read_buf = ReadBuf::new(buf);
        match self.socket.poll_recv_from(cx, &mut read_buf) {
            Poll::Ready(Ok(addr)) => {
                let len = read_buf.filled().len();
                *meta0 = RecvMeta {
                    addr,
                    len,
                    stride: len,
                    ecn: None,
                    dst_ip: None,
                };
                Poll::Ready(Ok(1))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}

/// R4-3（task-15）：本机 Sodium ed25519 身份 → `(raw pk, seed)`，供 QUIC 服务端固定 RPK。
///
/// 零改动 `libs/hbb_common`（task-15 Q3）：`Config::get_key_pair()`
/// （`libs/hbb_common/src/config.rs:1116`）已经是 pub，返回 `(sk, pk)`（`type KeyPair =
/// (Vec<u8>, Vec<u8>)`，同文件 `:61`）。Sodium 的 `sign::SecretKey` 是 64 字节 seed‖pk
/// （`libs/hbb_common/src/config.rs:1127` 的 `sign::gen_keypair()`），故 seed = `sk[..32]`。
#[cfg(feature = "quic")]
pub fn quic_local_identity() -> hbb_common::ResultType<([u8; 32], [u8; 32])> {
    let (sk, pk) = hbb_common::config::Config::get_key_pair();
    let pk: [u8; 32] = pk
        .as_slice()
        .try_into()
        .map_err(|_| hbb_common::anyhow::anyhow!("local ed25519 public key is not 32 bytes"))?;
    let seed: [u8; 32] = sk
        .get(..32)
        .ok_or_else(|| hbb_common::anyhow::anyhow!("local ed25519 secret key shorter than 32B"))?
        .try_into()
        .map_err(|_| hbb_common::anyhow::anyhow!("local ed25519 seed is not 32 bytes"))?;
    Ok((pk, seed))
}

/// R4-3（task-15）：这个数据报是不是 QUIC **v1 长首部**？
///
/// 监听侧用它做**纯本地**的协议分流：命中 ⇒ 交给 `quic_accept_attempt`，返回 false ⇒ 逐字节
/// 走原来的 `KcpStream::accept` 路径。判据（Lead 复核后收紧，见
/// `analysis/network/quic-server-wiring.md` §4.1）：
///
/// 1. 首字节 bit7（长首部形态）与 bit6（fixed bit）**都**置位；
/// 2. 版本字段 `datagram[1..5]` 恰为 QUIC v1 = `0x00 0x00 0x00 0x01`。
///
/// 判据 2 同时排除了版本 0（Version Negotiation）：它不会出现在客户端首包里，本模块的客户端
/// 也不协商其它版本（见下）。**旧判据（`bit7 != 0 && len > 8 && datagram[1..5] != [0;4]`）
/// 已被取代，原因是「KCP 首包的 `datagram[1..5]` 恒非 0」不成立**：本仓的 KCP 线格式不是经典
/// ikcp，而是 `kcp-sys`（branch `rustdesk-patches`）的 14 字节
/// `KcpPacketHeader { conv: u32, src_session_id: u32, dst_session_id: u32, flag: u8, rsv: u8 }`
/// （`kcp-sys/src/packet_def.rs:23-30`，无前缀，`KcpStream::accept` 原样注入）。其首字节是
/// `conv & 0xFF`，而 conv 来自 `cur_conv: AtomicU32::new(rand::random())`
/// （`kcp-sys/src/endpoint.rs:531`）⇒ bit7 约 1/2 概率置位；`datagram[1..5]` 的末字节是
/// `src_session_id & 0xFF`，`KcpStream::connect` 硬编码传 0（`src/kcp_stream.rs:117`），故
/// `!= [0;4]` 在 `conv >= 256` 时恒真（≈1 − 2⁻²⁴）。合起来 ⇒ 旧判据对 KCP SYN 有约 **50%**
/// 的假阳性，会让被控端把 KCP 连接误导入 QUIC 并等满超时后**杀掉这条打洞连接**。
///
/// 收紧后 KCP SYN 要假阳性必须同时满足 `conv < 256`（`conv>>8/16/24` 全 0）**且**
/// `src_session_id & 0xFF == 1`，而后者由本仓唯一的 connect 调用点固定为 0 ⇒ **结构上不可能**。
///
/// 收紧方向是安全方向：漏判只会让被控端退回 `KcpStream::accept`（把该包原样注入 KCP，无害），
/// 误判才会杀掉连接。
///
/// 本模块的客户端恒发 v1：quinn-proto 的 `ClientConfig::new` 默认 `version: 1`
/// （`quinn-proto-0.11.14/src/config/mod.rs:576`），`make_quic_client_config` 没有覆盖
/// `.version(...)`。客户端首包的长首部形态为 `LONG_HEADER_FORM | FIXED_BIT | (pn_len − 1)`
/// （`src/packet.rs:835`）⇒ 首字节 ∈ {0xC0..0xC3}；fixed bit 不会被 grease 掉，因为客户端在
/// 收到服务端 transport parameters 之前构造首包，此时 `peer_params.grease_quic_bit` 仍是默认
/// `false`（`src/transport_parameters.rs:127`），grease 只在 `peer_params.grease_quic_bit` 为真
/// 时随机翻转 fixed bit（`src/connection/packet_builder.rs:127`）。
pub fn looks_like_quic_long_header(datagram: &[u8]) -> bool {
    /// QUIC v1 的版本字段（RFC 9000）。
    const QUIC_V1: [u8; 4] = [0x00, 0x00, 0x00, 0x01];
    let Some(first) = datagram.first() else {
        return false;
    };
    // 短首部（1-RTT）bit7 = 0；fixed bit 是 bit6。`get(1..5)` 在长度 < 5 时返回 None。
    first & 0xC0 == 0xC0 && datagram.get(1..5) == Some(&QUIC_V1[..])
}

/// R4-3（task-15）：产品入口 —— 在**已打洞**的 UDP socket 上向对端发起 QUIC（Path B）。
///
/// 与调试用的 `quic_dial_quinn` 的唯一区别：socket 由调用方提供（`client.rs:udp_nat_connect`
/// 里刚 `punch_udp` 成功的那个），而不是自己新开一个 —— 原因见本段开头的第 1 条硬事实。
/// 对端地址直接取自 socket 的 `peer_addr()`（该 socket 已被 `connect()` 到对端）。
///
/// 一旦返回 `Ok(FramedStream)`，上层按 `Stream::Tcp` 照常 `set_key`/`send_raw`/`next`，
/// `stream.rs` 之上的 E2E secretbox 链不变。`peer_raw_pubkey` 是对端 32 字节 ed25519 公钥，
/// 来自 `signed_id_pk` 经 `common::decode_id_pk` 解出、且已在调用侧做过 `id == peer_id` 校验
/// （`src/client.rs`，task-15 Q4 裁决 A+）—— 它就是 RFC 7250 的信任锚。
/// 与 `quic_direct_attempt` 逐字同体，只多交回 `quinn::Connection` 句柄 —— 只有诊断入口
/// （`run_quic_pair_mode`）需要它：拿到句柄才能在收尾阶段**事件驱动**地等连接真正关闭
/// （`analysis/round23-quic-measure/plan.md` §12 的 D-2）。产品路径继续用
/// `quic_direct_attempt`（丢弃句柄，行为与改动前逐字一致）：多留一个句柄会推迟
/// CONNECTION_CLOSE，因为 quinn 只在最后一个句柄 drop 时才关闭连接。
#[cfg(feature = "quic")]
pub async fn quic_direct_attempt_with_conn(
    socket: Arc<hbb_common::tokio::net::UdpSocket>,
    peer_raw_pubkey: &[u8; 32],
    connect_timeout_ms: u64,
) -> hbb_common::ResultType<(FramedStream, quinn::Connection)> {
    // ring provider 是本模块的唯一全局前置条件。此前产品路径没有任何调用者装它，只有
    // `run_quic_probe_mode` 与测试装（缺口 G3，见
    // `analysis/network/phase3-wiring-options.md`）。在这里装一次是幂等的，从而不必去改
    // 共享核心文件 `src/lib.rs`（AGENTS.md：共享文件只留 thin hook）。
    install_ring_provider();
    // P2（task-4，`analysis/round23-quic-measure/plan.md` §4）：握手耗时此前完全不可见 ——
    // 产品路径只有一条「建立成功」的 INFO，既没有耗时也没有本地端口，跨机排查时无法区分
    // 「握手慢」与「根本没连上」。
    let t0 = std::time::Instant::now();

    let peer = socket.peer_addr()?;
    let client_cfg = make_quic_client_config(peer_raw_pubkey)?;
    let mut endpoint = quinn::Endpoint::new_with_abstract_socket(
        EndpointConfig::default(),
        None,
        Arc::new(TokioAsyncUdpSocket {
            socket: socket.clone(),
        }),
        default_runtime()
            .ok_or_else(|| hbb_common::anyhow::anyhow!("no async runtime available for QUIC"))?,
    )?;
    endpoint.set_default_client_config(client_cfg);
    let sn = ServerName::IpAddress(peer.ip().into());
    let connecting = endpoint
        .connect(peer, &sn.to_str())
        .map_err(|e| hbb_common::anyhow::anyhow!("quinn Endpoint::connect: {e:?}"))?;
    let conn = hbb_common::tokio::time::timeout(
        std::time::Duration::from_millis(connect_timeout_ms),
        connecting,
    )
    .await
    .map_err(|_| {
        hbb_common::anyhow::anyhow!("quic dial timeout after {connect_timeout_ms}ms")
    })??;
    let (send, recv) = conn
        .open_bi()
        .await
        .map_err(|e| hbb_common::anyhow::anyhow!("open_bi failed: {e:?}"))?;
    hbb_common::log::info!(
        "QUIC 直连建立：对端 raw ed25519 公钥已固定（RFC 7250 RPK）；握手耗时 {} ms",
        t0.elapsed().as_millis()
    );
    // N3（复核 944177453）：地址属敏感信息，默认级别不留 IP/端口 —— 需要时用 debug 级复现。
    hbb_common::log::debug!("QUIC 直连建立细节：local={:?} peer={}", socket.local_addr(), peer);
    // P0-2：仅在 NERV_QUIC_KEEPALIVE 开启时保留句柄（默认关闭 = 与改动前逐字一致）。
    retain_conn_for_stats(&conn);
    Ok((quic_into_framed_stream(recv, send, peer), conn))
}

/// 产品路径入口：只要那条双向字节流，丢弃连接句柄（行为与改动前逐字一致）。
#[cfg(feature = "quic")]
pub async fn quic_direct_attempt(
    socket: Arc<hbb_common::tokio::net::UdpSocket>,
    peer_raw_pubkey: &[u8; 32],
    connect_timeout_ms: u64,
) -> hbb_common::ResultType<FramedStream> {
    quic_direct_attempt_with_conn(socket, peer_raw_pubkey, connect_timeout_ms)
        .await
        .map(|(stream, _conn)| stream)
}

/// R4-3（task-15）：产品入口 —— 在**已打洞**的 UDP socket 上接受一条 QUIC 连接（被控端）。
///
/// 由 `rendezvous_mediator::udp_nat_listen` 在 `punch_udp` 之后、`KcpStream::accept` 之前调用，
/// 且只在「对端首包是 QUIC 长首部」时调用，因此同一个 socket 不会同时被 quinn 与 KCP 持有。
///
/// 身份方向性（诚实声明）：`make_quic_server_config` 用 `with_no_client_auth`
/// （`src/quic_transport.rs` 内），即**QUIC 层只让客户端认证服务端**，服务端不认证客户端 ——
/// 主控端身份仍由上层既有的 `identity_handshake` 完成。这与服务端侧既有行为一致，未新增缺口。
#[cfg(feature = "quic")]
pub async fn quic_accept_attempt_with_conn(
    socket: Arc<hbb_common::tokio::net::UdpSocket>,
    peer: SocketAddr,
    accept_timeout_ms: u64,
    server_cfg: QuinnServerConfig,
) -> hbb_common::ResultType<(FramedStream, quinn::Connection)> {
    install_ring_provider();
    // P2（task-4）：被控端同样需要握手耗时与本地端口 —— 它是「首包被 `punch_udp` 吃掉、
    // 只能靠 PTO 重传」这一时序缺陷的受害侧，没有这条日志就只能靠猜。
    let t0 = std::time::Instant::now();

    let endpoint = quinn::Endpoint::new_with_abstract_socket(
        EndpointConfig::default(),
        Some(server_cfg),
        Arc::new(TokioAsyncUdpSocket {
            socket: socket.clone(),
        }),
        default_runtime()
            .ok_or_else(|| hbb_common::anyhow::anyhow!("no async runtime available for QUIC"))?,
    )?;
    let incoming = hbb_common::tokio::time::timeout(
        std::time::Duration::from_millis(accept_timeout_ms),
        endpoint.accept(),
    )
    .await
    .map_err(|_| hbb_common::anyhow::anyhow!("quic accept timeout after {accept_timeout_ms}ms"))?
    .ok_or_else(|| hbb_common::anyhow::anyhow!("QUIC endpoint closed before any connection"))?;
    let conn = hbb_common::tokio::time::timeout(
        std::time::Duration::from_millis(accept_timeout_ms),
        incoming,
    )
    .await
    .map_err(|_| hbb_common::anyhow::anyhow!("quic handshake timeout"))??;
    let (send, recv) = conn
        .accept_bi()
        .await
        .map_err(|e| hbb_common::anyhow::anyhow!("accept_bi failed: {e:?}"))?;
    hbb_common::log::info!(
        "QUIC 入站连接已建立：对端 raw ed25519 公钥已按 RFC 7250 RPK 固定；握手耗时 {} ms",
        t0.elapsed().as_millis()
    );
    // N3（复核 944177453）：地址属敏感信息，默认级别不留 IP/端口 —— 需要时用 debug 级复现。
    hbb_common::log::debug!("QUIC 入站连接细节：local={:?} peer={}", socket.local_addr(), peer);
    // P0-2：仅在 NERV_QUIC_KEEPALIVE 开启时保留句柄（默认关闭 = 与改动前逐字一致）。
    retain_conn_for_stats(&conn);
    Ok((quic_into_framed_stream(recv, send, peer), conn))
}

/// 产品路径入口（被控端只用它）：只要那条双向字节流，丢弃连接句柄（行为与改动前逐字一致）。
#[cfg(feature = "quic")]
pub async fn quic_accept_attempt(
    socket: Arc<hbb_common::tokio::net::UdpSocket>,
    peer: SocketAddr,
    accept_timeout_ms: u64,
    server_cfg: QuinnServerConfig,
) -> hbb_common::ResultType<FramedStream> {
    quic_accept_attempt_with_conn(socket, peer, accept_timeout_ms, server_cfg)
        .await
        .map(|(stream, _conn)| stream)
}

// ---------- P1d-tail-3：`--quic-probe-mode` 调试入口（仅 RT-01 / Win7 字节级实测用）----------
//
// 调用形态：
//   hbbndesk-client.exe --quic-probe-mode self-check
//     在 loopback 上跑 C1 RPK + QUIC dial + 5B hello/ok；退出码 0/1。
//   hbbndesk-client.exe --quic-probe-mode <peer_ip:port> <peer_pk_hex32>
//     对指定二方主机跑同样流程；<peer_pk_hex32> 是 64 hex 字符（32 字节裸 ed25519 pk）。
//
// 行为：装 ring provider → 解析 peer + pk → 用 `quic_dial_quinn` 拨号 → open_bi
// → 交换 5B hello/ok → close。打印每步日志 + 退出码。
// 退出码：0 = 全程成功；1 = 任一步失败（错误已打到 stderr）。
//
// **本入口不进产品路径**：仅供 Win7 RT-01 用，未来也可作为 `rpk-probe ↔ quic_transport`
// 二进制的桥。AGENTS.md「minimal intrusion」原则：单独 fn、`#[cfg(feature="quic")]` 包裹，
// 不修改其它任何核心路径。

#[cfg(feature = "quic")]
pub fn run_quic_probe_mode(args: &[String]) -> Option<bool> {
    use hbb_common::tokio::runtime::Builder;
    use std::time::Duration;

    install_ring_provider();
    println!("[quic-probe] ring provider installed");

    // 解析参数
    enum Mode {
        SelfCheck,
        Dial { peer: std::net::SocketAddr, pk: [u8; 32] },
    }
    let mode = match args.get(1).map(|s| s.as_str()) {
        Some("self-check") => Mode::SelfCheck,
        Some(peer_str) => {
            let peer: std::net::SocketAddr = match peer_str.parse() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("[quic-probe] bad peer address `{peer_str}`: {e}");
                    return Some(false);
                }
            };
            let pk_hex = match args.get(2) {
                Some(s) => s,
                None => {
                    eprintln!("[quic-probe] missing <peer_pk_hex32>");
                    return Some(false);
                }
            };
            if pk_hex.len() != 64 {
                eprintln!(
                    "[quic-probe] peer_pk_hex32 must be 64 hex chars, got {}",
                    pk_hex.len()
                );
                return Some(false);
            }
            let mut pk = [0u8; 32];
            let mut ok = true;
            for (i, chunk) in pk_hex.as_bytes().chunks(2).enumerate() {
                let s = std::str::from_utf8(chunk).unwrap_or("");
                match u8::from_str_radix(s, 16) {
                    Ok(b) => pk[i] = b,
                    Err(_) => {
                        ok = false;
                        break;
                    }
                }
            }
            if !ok {
                eprintln!("[quic-probe] peer_pk_hex32 has non-hex chars");
                return Some(false);
            }
            Mode::Dial { peer, pk }
        }
        None => {
            eprintln!("[quic-probe] usage:");
            eprintln!("  --quic-probe-mode self-check");
            eprintln!("  --quic-probe-mode <peer_ip:port> <peer_pk_hex32>");
            return Some(false);
        }
    };

    // self-check：loopback 启动 + 拨号
    let rt = match Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("[quic-probe] tokio runtime build failed: {e}");
            return Some(false);
        }
    };
    let outcome = rt.block_on(async move {
        // 用 ring 直接由 32B seed 导出匹配的 32B 公钥 —— 与 quic_c1_rpk_handshake_loopback 同款
        let seed: [u8; 32] = [0x42u8; 32];
        let keypair = match ring::signature::Ed25519KeyPair::from_seed_unchecked(&seed) {
            Ok(kp) => kp,
            Err(e) => {
                eprintln!("[quic-probe] Ed25519KeyPair::from_seed_unchecked: {e:?}");
                return false;
            }
        };
        let mut pk = [0u8; 32];
        pk.copy_from_slice(keypair.public_key().as_ref());

        let (peer_addr, pk_arg) = match &mode {
            Mode::SelfCheck => {
                // loopback 服务端
                let server_cfg = match make_quic_server_config(&pk, &seed) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("[quic-probe] make_quic_server_config: {e}");
                        return false;
                    }
                };
                let server = match quinn::Endpoint::server(server_cfg, "127.0.0.1:0".parse().unwrap()) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[quic-probe] Endpoint::server: {e:?}");
                        return false;
                    }
                };
                let addr = server.local_addr().unwrap();
                let server_task = hbb_common::tokio::spawn(async move {
                    let incoming = match server.accept().await {
                        Some(i) => i,
                        None => return,
                    };
                    let conn = match incoming.await {
                        Ok(c) => c,
                        Err(_) => return,
                    };
                    let (mut send, mut recv) = match conn.accept_bi().await {
                        Ok(sr) => sr,
                        Err(_) => return,
                    };
                    use hbb_common::tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 5];
                    let _ = recv.read_exact(&mut buf).await;
                    let _ = send.write_all(b"ok").await;
                    conn.closed().await;
                });
                (addr, pk)
            }
            Mode::Dial { peer, pk } => (*peer, *pk),
        };

        let local_addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        println!(
            "[quic-probe] dialing {peer_addr} local={local_addr} timeout=3000ms pk={}",
            hex::encode(pk_arg)
        );
        let started = std::time::Instant::now();
        let (recv, send, _peer) =
            match quic_dial_quinn(peer_addr, local_addr, 3_000, &pk_arg).await {
                Ok(t) => t,
                Err(e) => {
                    eprintln!(
                        "[quic-probe] quic_dial_quinn FAILED after {:?}: {e}",
                        started.elapsed()
                    );
                    return false;
                }
            };
        let handshake_ms = started.elapsed().as_millis();
        println!("[quic-probe] handshake OK in {handshake_ms} ms");

        // 5B hello/ok 握手
        let (mut recv, mut send) = (recv, send);
        use hbb_common::tokio::io::{AsyncReadExt, AsyncWriteExt};
        if let Err(e) = send.write_all(b"hello").await {
            eprintln!("[quic-probe] write_all(hello) failed: {e:?}");
            return false;
        }
        let mut ack = [0u8; 2];
        match recv.read_exact(&mut ack).await {
            Ok(_) => {}
            Err(e) => {
                eprintln!("[quic-probe] read_exact(ok) failed: {e:?}");
                return false;
            }
        }
        if &ack[..] != b"ok" {
            eprintln!(
                "[quic-probe] handshake reply wrong: got {:?} want b\"ok\"",
                &ack[..]
            );
            return false;
        }
        let rtt_ms = started.elapsed().as_millis();
        println!("[quic-probe] hello/ok exchange OK in {rtt_ms} ms total");

        drop(send);
        drop(recv);
        hbb_common::tokio::time::sleep(Duration::from_millis(50)).await;
        println!("[quic-probe] PASS");
        true
    });
    Some(outcome)
}

// ---------- P1d-tail-2：拨号原语（低层，RT-01 / 测试共享）----------
//
// 把「真正能拨通的 QUIC 连接」独立成 `quic_dial_quinn`，返回 `(RecvStream, SendStream)`，
// 与上层「把它焊成 `FramedStream`」解耦：
// - 上层 `quic_direct_attempt`（product 入口）保持「当前没部署对端 → 真实 Err」语义，
//   不被这个原语覆盖；commitment 边界不破；
// - 测试（`quic_dial_quinn_loopback`）和未来的 `rpk-probe ↔ quic_transport` 二进制
//   可以直接调它做跨机 RTT/吞吐，**不需要伪造产品路径成功**；
// - RT-01 实测二方主机时，把 `_peer` / `_local_addr` / `_peer_raw_pubkey` 真实参数
//   传进来即可（Round-Trip 自带超时 + C1 RPK 校验），跑的是真路径不是纸面。

/// QUIC 拨号原语：返回 `(recv, send, peer_addr)` 三元组，调用方按需焊成 `FramedStream`。
///
/// - `peer`：对端 UDP 地址（如 `1.2.3.4:4433`）；
/// - `local_addr`：本端 bind 地址（一般 `0.0.0.0:0` 让系统挑）；
/// - `connect_timeout_ms`：握手超时（毫秒）；
/// - `peer_raw_pubkey`：对端 32 字节 ed25519 裸公钥，传给 `NervRpkVerifier`。
///
/// **本函数要求调用方先 `install_ring_provider()`**（`src/lib.rs` 启动期会装）。
#[cfg(feature = "quic")]
pub async fn quic_dial_quinn(
    peer: SocketAddr,
    local_addr: SocketAddr,
    connect_timeout_ms: u64,
    peer_raw_pubkey: &[u8; 32],
) -> hbb_common::ResultType<(RecvStream, SendStream, SocketAddr)> {
    use hbb_common::tokio::time::Duration;
    let client_cfg = make_quic_client_config(peer_raw_pubkey)?;
    let mut endpoint = quinn::Endpoint::client(local_addr)?;
    endpoint.set_default_client_config(client_cfg);
    // SN 用 IP 字面量形式 —— RustDesk 走的是纯 IP 拨号，无 DNS；
    // 用 `ServerName::IpAddress(...)` 须 rustls 0.23 + pki-types 支持；这里给字面量形式。
    let sn = rustls::pki_types::ServerName::IpAddress(
        peer.ip().into(),
    );
    let connecting = endpoint
        .connect(peer, &sn.to_str())
        .map_err(|e| hbb_common::anyhow::anyhow!("quinn Endpoint::connect: {e:?}"))?;
    let conn = hbb_common::tokio::time::timeout(
        Duration::from_millis(connect_timeout_ms),
        connecting,
    )
    .await
    .map_err(|_| hbb_common::anyhow::anyhow!("quic dial timeout after {connect_timeout_ms}ms"))??;
    let (send, recv) = conn
        .open_bi()
        .await
        .map_err(|e| hbb_common::anyhow::anyhow!("open_bi failed: {e:?}"))?;
    Ok((recv, send, peer))
}

// ---------------- C1（RFC 7250 Raw Public Key）----------------
// 设计：analysis/quic-integration/C1-IMPLEMENTATION-DESIGN.md v2
// 锁版：quinn 0.11.9 / rustls 0.23.28 / ring 0.17.14（rustc 1.75.0 实证）
// 证据：analysis/rpk-probe（3/3 PASS，2026-10-01）

/// RFC 8410 ed25519 SubjectPublicKeyInfo 固定 12 字节前缀
/// `SEQUENCE { SEQUENCE { OID 1.3.101.112 }, BIT STRING { ... } }`。
const ED25519_SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

/// 从 32 字节 ed25519 裸公钥构造 RFC 7250 raw public key（== SPKI DER，44 字节）。
fn ed25519_rpk(raw_pubkey: &[u8; 32]) -> CertificateDer<'static> {
    let mut spki = Vec::with_capacity(44);
    spki.extend_from_slice(&ED25519_SPKI_PREFIX);
    spki.extend_from_slice(raw_pubkey);
    CertificateDer::from(spki)
}

/// PKCS#8 v2 ED25519 PrivateKeyInfo DER 头部（16 字节，不含 seed 与尾部）。
///
/// 结构（RFC 8410 §7）：
/// ```text
/// SEQUENCE(46) {                          -- 30 2e
///   INTEGER 0                              -- 02 01 00
///   SEQUENCE { OID 1.3.101.112 }           -- 30 05 06 03 2b 65 70
///   OCTET STRING(34) {                     -- 04 22
///     OCTET STRING(32) { <seed> }          -- 04 20 <seed>
///   }
/// }
/// ```
const ED25519_PKCS8_HEADER: [u8; 16] = [
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];

/// RustDesk 现有身份体系是 Sodium `crypto_sign::ed25519` 32 字节 seed；
/// ring 的 `any_eddsa_type` 接受 PKCS#8 v2 DER，故包装一层（48 字节）。
///
/// 设计 §6 唯一未知：Sodium seed → PKCS#8 v2 ED25519 包装。本函数落地该转换，
/// 结构按 RFC 8410 §7 固定（VERSION 0，OID 1.3.101.112，OCTET STRING 嵌
/// CurvePrivateKey OCTET STRING）；ring 0.17.14 接受该形态（rpk-probe 已实证）。
fn sodium_seed_to_pkcs8_v2_ed25519(seed: &[u8; 32]) -> Vec<u8> {
    let mut der = Vec::with_capacity(48);
    der.extend_from_slice(&ED25519_PKCS8_HEADER);
    der.extend_from_slice(seed);
    der
}

/// NERV 客户端 verifier：只信任一个 raw public key（RFC 7250）。
///
/// `verify_server_cert` 做字节恒等（RPK 语义无链可验）；`verify_tls13_signature`
/// 委托给 rustls 自带的 `verify_tls13_signature_with_raw_key`（公开 API，ring
/// backend），不写自定义密码学。形态与 `analysis/rpk-probe`（3/3 PASS）实测一致。
pub struct NervRpkVerifier {
    /// 期望的对端 SPKI（44 字节 RPK）
    expected_spki: CertificateDer<'static>,
    /// ring 默认签名算法集（值类型，非引用）
    supported: WebPkiSupportedAlgorithms,
}

impl std::fmt::Debug for NervRpkVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NervRpkVerifier").finish_non_exhaustive()
    }
}

impl NervRpkVerifier {
    pub fn new(peer_raw_pubkey: &[u8; 32]) -> Self {
        Self {
            expected_spki: ed25519_rpk(peer_raw_pubkey),
            supported: default_provider().signature_verification_algorithms,
        }
    }
}

impl ServerCertVerifier for NervRpkVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        if end_entity.as_ref() == self.expected_spki.as_ref() {
            Ok(ServerCertVerified::assertion())
        } else {
            // rustls 0.23.28 的 CertificateError 没有 NotValidForName 变体；
            // UnknownIssuer 是「非信任根」语义最贴近 RPK 失配的现成枚举。
            Err(Error::InvalidCertificate(
                rustls::CertificateError::UnknownIssuer,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        // quinn 强制 TLS 1.3；TLS 1.2 路径不可达。
        Err(Error::General("NERVDesk QUIC: TLS 1.2 is never negotiated".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        // rpk-probe 实测：用收到的 cert（即对端 RPK SPKI 自身）构造 SPKI 再委托。
        let spki = SubjectPublicKeyInfoDer::from(cert.as_ref().to_vec());
        verify_tls13_signature_with_raw_key(message, &spki, dss, &self.supported)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }

    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

/// 握手初始 RTT 假设：quinn 默认 `initial_rtt = 333ms`
/// （`quinn-proto-0.11.14/src/config/transport.rs:373`）⇒ 首次 PTO =
/// `333 + max(4*166.5, 1) = 999ms`（`connection/paths.rs:326`）。而接受侧 `punch_udp`
/// 会消费掉第一个 Initial（见本文件开头「已知代价」），发起侧因此**必须**等一次 PTO 重传，
/// 999ms 已经吃满 `client.rs` 给这条路径的全部预算（局域网路径的 `connect_timeout`
/// 就是 `const MIN` = 1000ms），必然后续超时。
///
/// 取 50ms ⇒ 首次 PTO = `50 + max(4*25, 1) = 150ms`，余下约 850ms 覆盖 `PTO + 2×RTT`，
/// 即真实 RTT 直到约 400ms 仍能在 1000ms 内握手完成。代价：首次 PTO 提前到 150ms，
/// 在 RTT > 150ms 的链路上会多发一个 Initial —— QUIC 对重复 Initial 幂等，无害。
const QUIC_INITIAL_RTT: std::time::Duration = std::time::Duration::from_millis(50);

/// 见 `QUIC_INITIAL_RTT`。两端都挂：客户端 PTO 决定「被吃掉的 Initial」何时重传，
/// 服务端 PTO 决定它自己的握手 flight 丢失时何时重传，两者都落在同一个 1000ms 预算内。
fn quic_transport_config() -> quinn::TransportConfig {
    let mut transport = quinn::TransportConfig::default();
    transport.initial_rtt(QUIC_INITIAL_RTT);
    transport
}

/// 构造 QUIC 服务端 rustls 配置（C1 RPK 版）。
///
/// `raw_pubkey`：本端 ID 公钥（32 字节）；用作 RPK 证书。
/// `signing_seed`：本端 Sodium ed25519 32 字节 seed；内部包装为 PKCS#8 v2。
pub fn make_quic_server_config(
    raw_pubkey: &[u8; 32],
    signing_seed: &[u8; 32],
) -> hbb_common::ResultType<QuinnServerConfig> {
    let pkcs8_der = sodium_seed_to_pkcs8_v2_ed25519(signing_seed);
    let signing_key = any_eddsa_type(&PrivatePkcs8KeyDer::from(pkcs8_der))
        .map_err(|e| hbb_common::anyhow::anyhow!("ring any_eddsa_type failed: {e:?}"))?;
    let rpk_cert = ed25519_rpk(raw_pubkey);
    let certified = CertifiedKey::new(vec![rpk_cert], signing_key);

    let provider: CryptoProvider = default_provider();
    let rustls_cfg = rustls::ServerConfig::builder_with_provider(provider.into())
        .with_protocol_versions(&[&TLS13])
        .map_err(|e| hbb_common::anyhow::anyhow!("TLS 1.3 unsupported: {e:?}"))?
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(AlwaysResolvesServerRawPublicKeys::new(Arc::new(
            certified,
        ))));

    let quic_cfg = QuicServerConfig::try_from(Arc::new(rustls_cfg))
        .map_err(|e| hbb_common::anyhow::anyhow!("QuicServerConfig::try_from: {e:?}"))?;
    let mut server_cfg = QuinnServerConfig::with_crypto(Arc::new(quic_cfg));
    server_cfg.transport_config(Arc::new(quic_transport_config()));
    Ok(server_cfg)
}

/// 构造 QUIC 客户端 rustls 配置（C1 RPK 版）。
///
/// `peer_raw_pubkey`：期望的对端 ID 公钥（32 字节）；NervRpkVerifier 用它做
/// 字节恒等比对，0-RTT 关闭（NERV 决策）。
pub fn make_quic_client_config(
    peer_raw_pubkey: &[u8; 32],
) -> hbb_common::ResultType<QuinnClientConfig> {
    let provider: CryptoProvider = default_provider();
    let rustls_cfg = rustls::ClientConfig::builder_with_provider(provider.into())
        .with_protocol_versions(&[&TLS13])
        .map_err(|e| hbb_common::anyhow::anyhow!("TLS 1.3 unsupported: {e:?}"))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NervRpkVerifier::new(peer_raw_pubkey)))
        .with_no_client_auth();
    // 0-RTT 关闭（不引入重放面；NERV 决策）
    let mut rustls_cfg = rustls_cfg;
    rustls_cfg.enable_early_data = false;

    let quic_cfg = QuicClientConfig::try_from(Arc::new(rustls_cfg))
        .map_err(|e| hbb_common::anyhow::anyhow!("QuicClientConfig::try_from: {e:?}"))?;
    let mut client_cfg = QuinnClientConfig::new(Arc::new(quic_cfg));
    client_cfg.transport_config(Arc::new(quic_transport_config()));
    Ok(client_cfg)
}

// ---------- task-5：`--quic-pair-mode` 跨机 QUIC 实测诊断入口（无头、机器可判定）----------
//
// 与 `--quic-probe-mode`（上面的 `run_quic_probe_mode`）的**本质区别**：本入口复用产品同一
// `punch_udp`（`src/common.rs:2898`），并**在同一条已打洞的 UDP socket 上**做 QUIC ——
// 与产品主控侧 `udp_nat_connect`（`src/client.rs:5646` → QUIC 调用点 `:5686`）和被控侧
// `udp_nat_listen`（`src/rendezvous_mediator.rs:1475` → QUIC 分支 `:1493`）的顺序一致。
// 因此它回答的是「打洞之后 QUIC 能不能过」，而不是「无 NAT 的裸 QUIC 能不能过」；
// 后者正是 probe-mode 的限界，也是「探针 PASS、跨机从未成功」的成因 —— 打洞函数会吃掉
// 对端第一个数据报，也就是首个 QUIC Initial（`src/common.rs:2963-2968`）。
//
// 用法：
//   nervdesk --quic-pair-mode genkey
//   nervdesk --quic-pair-mode dial   --local-port <P> --peer <ip:port> --peer-key <64hex>
//                                    [--key <64hex>] [--timeout-ms N] [--no-punch]
//   nervdesk --quic-pair-mode listen --port <P> [--peer <ip:port>] [--key <64hex>]
//                                    [--peer-key <64hex>] [--timeout-ms N] [--no-punch]
//
// `listen` 带 `--peer` = 与产品同序（先 connect 再打洞）；不带 `--peer` 时先在 `recv_from`
// 上学习对端地址（只适用于本端公开可达的一侧）—— 这是与产品顺序的**已知偏差**，实测报告
// 里必须如实标注。`--no-punch` 是负对照（跨 NAT 预期 FAIL）。`--key` 省略时随机生成
// 临时身份（限界：不是产品 RustDesk 身份键，见报告模板的登记项）。
//
// 输出：最后一行 `QUIC-PAIR …`；`exesha=` 是**运行中二进制**的 sha256，两端一致才说明
// 两侧跑的是同一个 `--features quic` 产物。密钥只作 hex 入参，绝不回显。
#[cfg(feature = "quic")]
pub fn run_quic_pair_mode(args: &[String]) -> Option<bool> {
    use hbb_common::tokio::net::UdpSocket;
    use hbb_common::tokio::runtime::Builder;
    use std::time::{Duration, Instant};

    fn to_hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
    fn hex32(s: &str) -> Option<[u8; 32]> {
        if s.len() != 64 {
            return None;
        }
        let mut out = [0u8; 32];
        for (i, c) in s.as_bytes().chunks(2).enumerate() {
            out[i] = u8::from_str_radix(std::str::from_utf8(c).ok()?, 16).ok()?;
        }
        Some(out)
    }
    fn new_seed() -> Option<[u8; 32]> {
        let mut s = [0u8; 32];
        ring::rand::SecureRandom::fill(&SystemRandom::new(), &mut s).ok()?;
        Some(s)
    }
    fn pk_of(seed: &[u8; 32]) -> Option<[u8; 32]> {
        let kp = Ed25519KeyPair::from_seed_unchecked(seed).ok()?;
        let mut p = [0u8; 32];
        p.copy_from_slice(kp.public_key().as_ref());
        Some(p)
    }
    /// 运行中二进制的 sha256：两端一致，才证明两侧是同一个 `--features quic` 产物。
    fn exe_sha() -> String {
        match std::env::current_exe()
            .ok()
            .and_then(|p| std::fs::read(p).ok())
        {
            Some(bytes) => to_hex(ring::digest::digest(&ring::digest::SHA256, &bytes).as_ref()),
            None => "-".to_owned(),
        }
    }

    let sub = args.get(1).map(String::as_str).unwrap_or("");
    if sub == "genkey" {
        let Some(seed) = new_seed() else {
            eprintln!("[quic-pair] cannot read entropy");
            return Some(false);
        };
        let Some(pk) = pk_of(&seed) else {
            eprintln!("[quic-pair] key derive failed");
            return Some(false);
        };
        println!("priv={} pub={}", to_hex(&seed), to_hex(&pk));
        return Some(true);
    }
    if sub != "dial" && sub != "listen" {
        eprintln!("[quic-pair] usage:");
        eprintln!("  --quic-pair-mode genkey");
        eprintln!(
            "  --quic-pair-mode dial   --local-port <P> --peer <ip:port> --peer-key <64hex> \
             [--key <64hex>] [--timeout-ms N] [--no-punch]"
        );
        eprintln!(
            "  --quic-pair-mode listen --port <P> [--peer <ip:port>] [--key <64hex>] \
             [--peer-key <64hex>] [--timeout-ms N] [--no-punch]"
        );
        return Some(false);
    }
    let role_dial = sub == "dial";
    let (mut local_port, mut port): (u16, u16) = (0, 0);
    let (mut peer, mut peer_pk): (Option<SocketAddr>, Option<[u8; 32]>) = (None, None);
    let mut seed: Option<[u8; 32]> = None;
    let (mut timeout_ms, mut punch): (u64, bool) = (15_000, true);
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--local-port" => {
                match args.get(i + 1).and_then(|v| v.parse::<u16>().ok()) {
                    Some(v) => local_port = v,
                    None => {
                        eprintln!("[quic-pair] bad --local-port");
                        return Some(false);
                    }
                }
                i += 2;
            }
            "--port" => {
                match args.get(i + 1).and_then(|v| v.parse::<u16>().ok()) {
                    Some(v) => port = v,
                    None => {
                        eprintln!("[quic-pair] bad --port");
                        return Some(false);
                    }
                }
                i += 2;
            }
            "--peer" => {
                match args.get(i + 1).and_then(|v| v.parse::<SocketAddr>().ok()) {
                    Some(v) => peer = Some(v),
                    None => {
                        eprintln!("[quic-pair] bad --peer");
                        return Some(false);
                    }
                }
                i += 2;
            }
            "--key" => {
                match args.get(i + 1).and_then(|v| hex32(v)) {
                    Some(v) => seed = Some(v),
                    None => {
                        eprintln!("[quic-pair] bad --key (need 64 hex chars)");
                        return Some(false);
                    }
                }
                i += 2;
            }
            "--peer-key" => {
                match args.get(i + 1).and_then(|v| hex32(v)) {
                    Some(v) => peer_pk = Some(v),
                    None => {
                        eprintln!("[quic-pair] bad --peer-key (need 64 hex chars)");
                        return Some(false);
                    }
                }
                i += 2;
            }
            "--timeout-ms" => {
                match args.get(i + 1).and_then(|v| v.parse::<u64>().ok()) {
                    Some(v) => timeout_ms = v,
                    None => {
                        eprintln!("[quic-pair] bad --timeout-ms");
                        return Some(false);
                    }
                }
                i += 2;
            }
            "--no-punch" => {
                punch = false;
                i += 1;
            }
            other => {
                eprintln!("[quic-pair] unknown argument `{other}`");
                return Some(false);
            }
        }
    }
    if role_dial && (peer.is_none() || peer_pk.is_none()) {
        eprintln!("[quic-pair] dial needs --peer and --peer-key");
        return Some(false);
    }
    if !role_dial && port == 0 {
        eprintln!("[quic-pair] listen needs --port");
        return Some(false);
    }
    let Some(my_seed) = seed.or_else(new_seed) else {
        eprintln!("[quic-pair] cannot read entropy");
        return Some(false);
    };
    let Some(my_pk) = pk_of(&my_seed) else {
        eprintln!("[quic-pair] local key derive failed");
        return Some(false);
    };
    let exesha = exe_sha();
    let bind: SocketAddr = if role_dial {
        SocketAddr::from(([0, 0, 0, 0], local_port))
    } else {
        SocketAddr::from(([0, 0, 0, 0], port))
    };
    println!(
        "[quic-pair] role={} bind={} my_pub={} peer={:?} peer_key={} punch={} timeout_ms={}",
        if role_dial { "dial" } else { "listen" },
        bind,
        to_hex(&my_pk),
        peer,
        peer_pk.is_some(),
        punch as u8,
        timeout_ms
    );
    let rt = match Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("[quic-pair] tokio runtime build failed: {e}");
            return Some(false);
        }
    };
    let t_all = Instant::now();
    let (ok, line) = rt.block_on(async move {
        let (mut init_len, mut first_byte) = (0usize, 0u8);
        let (mut punch_ms, mut hs_ms, mut echo_bytes) = (0u128, 0u128, 0usize);
        // `mode` 必须在 `macro_rules!` 之前声明：宏体里的自由标识符在展开点按定义处的
        // 语法上下文解析，定义之后才 `let` 绑定的名字会报 E0425（macro hygiene）。
        let mut mode = "with-peer";
        macro_rules! outcome {
            ($ok:expr, $local:expr, $peer:expr, $err:expr) => {
                format!(
                    "role={} result={} punch={} mode={} local={} peer={} punch_ms={} hs_ms={} \
                     init_len={} first_byte=0x{:02x} echo_bytes={} total_ms={} exesha={} err=\"{}\"",
                    if role_dial { "dial" } else { "listen" },
                    if $ok { "PASS" } else { "FAIL" },
                    punch as u8,
                    mode,
                    $local,
                    $peer,
                    punch_ms,
                    hs_ms,
                    init_len,
                    first_byte,
                    echo_bytes,
                    t_all.elapsed().as_millis(),
                    exesha,
                    $err
                )
            };
        }
        let peer_addr: SocketAddr;
        let socket = match UdpSocket::bind(bind).await {
            Ok(s) => Arc::new(s),
            Err(e) => {
                return (false, outcome!(false, bind, "-", format!("bind: {e}")));
            }
        };
        // D-1：报 bind 地址还是报真实本地地址，之前不一致 —— 失败行报 `bind`（用户传的），
        // 成功行报 `local_addr()`。同一格字段两种含义会让两机比对时误判端口；统一取真实值。
        let local = socket.local_addr().unwrap_or(bind);
        if role_dial {
            let p = peer.unwrap();
            if let Err(e) = socket.connect(p).await {
                return (false, outcome!(false, local, p, format!("connect: {e}")));
            }
            peer_addr = p;
        } else {
            match peer {
                Some(p) => {
                    if let Err(e) = socket.connect(p).await {
                        return (false, outcome!(false, local, p, format!("connect: {e}")));
                    }
                    peer_addr = p;
                }
                None => {
                    // D-1：先置 `mode` 再尝试学习对端，否则超时行会报 `mode=with-peer`
                    // （用户明明没给 `--peer`）；两机比对时这一格是判「谁没发包」的入口。
                    mode = "learn-peer";
                    // 与产品顺序的已知偏差：产品由 ID Server 提供对端地址；本入口在此学习。
                    let mut buf = [0u8; 1500];
                    let dur = Duration::from_millis(timeout_ms.clamp(1_000, 10_000));
                    match hbb_common::tokio::time::timeout(dur, socket.recv_from(&mut buf)).await {
                        Ok(Ok((n, from))) => {
                            init_len = n;
                            first_byte = buf.first().copied().unwrap_or(0);
                            if let Err(e) = socket.connect(from).await {
                                return (false, outcome!(false, local, from, format!("connect: {e}")));
                            }
                            peer_addr = from;
                        }
                        Ok(Err(e)) => {
                            return (false, outcome!(false, local, "-", format!("recv_from: {e}")));
                        }
                        Err(_) => {
                            return (
                                false,
                                outcome!(
                                    false,
                                    local,
                                    "-",
                                    "learn-peer timeout: 未收到任何入站数据报（NAT 未打洞或对端未发）"
                                ),
                            );
                        }
                    }
                }
            }
        }
        install_ring_provider();
        if punch {
            let t = Instant::now();
            let r = if role_dial {
                crate::punch_udp(socket.clone(), false).await
            } else {
                crate::punch_udp(socket.clone(), true).await
            };
            punch_ms = t.elapsed().as_millis();
            match r {
                Ok(Some(b)) => {
                    if init_len == 0 {
                        init_len = b.len();
                        first_byte = b.first().copied().unwrap_or(0);
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    return (false, outcome!(false, local, peer_addr, format!("punch_udp: {e}")));
                }
            }
        }
        let t = Instant::now();
        let (mut stream, conn) = if role_dial {
            let pk = peer_pk.unwrap();
            match quic_direct_attempt_with_conn(socket.clone(), &pk, timeout_ms).await {
                Ok(pair) => pair,
                Err(e) => {
                    hs_ms = t.elapsed().as_millis();
                    return (
                        false,
                        outcome!(false, local, peer_addr, format!("quic_direct_attempt: {e}")),
                    );
                }
            }
        } else {
            let cfg = match make_quic_server_config(&my_pk, &my_seed) {
                Ok(c) => c,
                Err(e) => {
                    return (
                        false,
                        outcome!(false, local, peer_addr, format!("make_quic_server_config: {e}")),
                    );
                }
            };
            match quic_accept_attempt_with_conn(socket.clone(), peer_addr, timeout_ms, cfg).await {
                Ok(pair) => pair,
                Err(e) => {
                    hs_ms = t.elapsed().as_millis();
                    return (
                        false,
                        outcome!(false, local, peer_addr, format!("quic_accept_attempt: {e}")),
                    );
                }
            }
        };
        hs_ms = t.elapsed().as_millis();
        // P0-2 + P1-stats 的**本地可验证**出口：`_with_conn` 在 `NERV_QUIC_KEEPALIVE` 开启时已把
        // 句柄存进 `LAST_CONN`，这里取出来打到 **stdout**（无头脚本直接抓，不用翻日志文件）。
        // 开关关闭 ⇒ `quic_stats_line` 返回 None ⇒ 一个字都不打印（默认行为与今天一致）。
        if let Some(line) = quic_stats_line(if role_dial { "pair-dial" } else { "pair-listen" }) {
            println!("{line}");
        }
        // 握手成功 ≠ 数据能过：两端各发 8 字节、各读一次。
        let payload: Vec<u8> = if role_dial {
            b"qp:ping\n".to_vec()
        } else {
            b"qp:pong\n".to_vec()
        };
        if let Err(e) = stream.send_raw(payload).await {
            return (false, outcome!(false, local, peer_addr, format!("send_raw: {e}")));
        }
        match hbb_common::tokio::time::timeout(Duration::from_secs(5), stream.next()).await {
            Ok(Some(Ok(b))) => echo_bytes = b.len(),
            Ok(Some(Err(e))) => {
                return (false, outcome!(false, local, peer_addr, format!("read: {e}")));
            }
            Ok(None) => {
                return (
                    false,
                    outcome!(false, local, peer_addr, "stream closed before any byte"),
                );
            }
            Err(_) => {
                return (
                    false,
                    outcome!(false, local, peer_addr, "byte roundtrip timeout"),
                );
            }
        }
        // D-2（task-6 §12 的补丁项）：此前这里是固定 `sleep(500ms)` 等 quinn 把缓冲字节
        // 发到线上 —— 那是时间猜测，慢机器会复发。根因是 `poll_write` 只保证写进缓冲，
        // 而 `core_main` 的挂载点拿到返回值后立刻 `std::process::exit(...)`：进程一退，
        // 最后一段数据就没了（首轮 loopback 实测里 listen 侧的 `qp:pong` 正是这样丢的，
        // dial 侧报 `byte roundtrip timeout` 而 listen 侧 echo_bytes=8 正常）。
        //
        // 改法：不再猜时间，也不再主动 `conn.close()` —— quinn 的 `close()` 会立刻停发
        // 且丢弃未确认的流数据（`Connection::close` 文档：pending operations fail
        // immediately），刚写完的那 8 字节就可能在关闭时被丢掉。这里改为「保持连接打开、
        // 等到对端关闭或到达上限」：这段时间连接仍是活的，quinn 的连接驱动会照常重传
        // 未确认字节，因此等待本身就把「已写入但未上线」的数据推出去了。两端对称。
        let _ = hbb_common::tokio::time::timeout(Duration::from_secs(3), conn.closed()).await;
        (
            true,
            outcome!(true, local, peer_addr, ""),
        )
    });
    println!("QUIC-PAIR {line}");
    Some(ok)
}

#[cfg(all(test, feature = "quic"))]
mod tests {
    use super::{install_ring_provider, quic_into_framed_stream};
    use hbb_common::sodiumoxide::crypto::secretbox;
    use quinn::{
        crypto::rustls::QuicClientConfig, rustls::RootCertStore,
        rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer}, ClientConfig,
        Endpoint, ServerConfig,
    };
    use ring::signature::{Ed25519KeyPair, KeyPair as _};
    use std::sync::Arc;

    /// 一次性测试证书（openssl 生成，仅测试使用，配方与 DER 见
    /// `analysis/quic-integration/certs/`）。结构照抄探针
    /// `analysis/quic-probe/src/lib.rs:123-142` 的验证过的形态：
    /// **CA（CA:TRUE）+ 由 CA 签发的 localhost 叶（CA:FALSE、
    /// extendedKeyUsage=serverAuth、SAN=DNS:localhost）**；
    /// 客户端根库放 CA，服务端出示叶+私钥。
    const TEST_CA_DER: &[u8] = &
    [
    0x30, 0x82, 0x03, 0x33, 0x30, 0x82, 0x02, 0x1b, 0xa0, 0x03, 0x02, 0x01, 0x02, 0x02, 0x14, 0x47,
    0x1f, 0xd1, 0xb2, 0xc9, 0x24, 0xce, 0x1e, 0x80, 0xf6, 0x72, 0x7b, 0x17, 0xed, 0x73, 0x39, 0x52,
    0x68, 0xa6, 0x09, 0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b,
    0x05, 0x00, 0x30, 0x20, 0x31, 0x1e, 0x30, 0x1c, 0x06, 0x03, 0x55, 0x04, 0x03, 0x0c, 0x15, 0x4e,
    0x45, 0x52, 0x56, 0x44, 0x65, 0x73, 0x6b, 0x20, 0x51, 0x55, 0x49, 0x43, 0x20, 0x54, 0x65, 0x73,
    0x74, 0x20, 0x43, 0x41, 0x30, 0x20, 0x17, 0x0d, 0x32, 0x36, 0x30, 0x39, 0x32, 0x38, 0x31, 0x32,
    0x31, 0x33, 0x33, 0x35, 0x5a, 0x18, 0x0f, 0x32, 0x31, 0x32, 0x36, 0x30, 0x39, 0x30, 0x34, 0x31,
    0x32, 0x31, 0x33, 0x33, 0x35, 0x5a, 0x30, 0x20, 0x31, 0x1e, 0x30, 0x1c, 0x06, 0x03, 0x55, 0x04,
    0x03, 0x0c, 0x15, 0x4e, 0x45, 0x52, 0x56, 0x44, 0x65, 0x73, 0x6b, 0x20, 0x51, 0x55, 0x49, 0x43,
    0x20, 0x54, 0x65, 0x73, 0x74, 0x20, 0x43, 0x41, 0x30, 0x82, 0x01, 0x22, 0x30, 0x0d, 0x06, 0x09,
    0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05, 0x00, 0x03, 0x82, 0x01, 0x0f, 0x00,
    0x30, 0x82, 0x01, 0x0a, 0x02, 0x82, 0x01, 0x01, 0x00, 0x98, 0x1a, 0x95, 0xd2, 0xbe, 0xe9, 0x51,
    0x89, 0x94, 0x47, 0x20, 0xc3, 0x99, 0x79, 0x24, 0x49, 0xf9, 0x45, 0xb3, 0xef, 0x22, 0xa3, 0x9c,
    0xb5, 0xc8, 0xfd, 0xd7, 0x64, 0xdd, 0xfa, 0x05, 0xeb, 0xae, 0xa1, 0xc2, 0xbd, 0xd3, 0x54, 0x9b,
    0xe9, 0x6d, 0x7d, 0x29, 0x36, 0x03, 0x82, 0xfd, 0xc3, 0xcd, 0x7a, 0xba, 0x26, 0x60, 0x71, 0xe9,
    0xc0, 0x93, 0xd0, 0xb8, 0x21, 0x26, 0x33, 0x4f, 0x86, 0xf0, 0x14, 0xb9, 0x44, 0x64, 0xcb, 0x91,
    0x46, 0xba, 0x89, 0xd8, 0xc7, 0x48, 0xb4, 0xd0, 0x03, 0x15, 0x86, 0xc6, 0xdb, 0xdf, 0x38, 0x25,
    0x79, 0x95, 0x78, 0xa0, 0xaf, 0x99, 0xc0, 0x68, 0x25, 0x19, 0x56, 0xd4, 0x88, 0xc7, 0xce, 0xe6,
    0xc9, 0xec, 0xaf, 0x28, 0x2d, 0x94, 0xbf, 0xae, 0x83, 0xe0, 0xbe, 0x60, 0x65, 0x41, 0x55, 0x24,
    0x2a, 0x41, 0xe0, 0x14, 0xe8, 0xdb, 0x8e, 0xf2, 0x5c, 0x1e, 0x55, 0x11, 0xdc, 0xce, 0x9f, 0x14,
    0x7f, 0x5d, 0x47, 0x25, 0x06, 0xf2, 0x68, 0x05, 0x39, 0x6f, 0x7b, 0x1d, 0x4f, 0xd1, 0xef, 0x80,
    0x65, 0xde, 0xd5, 0x2a, 0x26, 0x07, 0x1d, 0xf6, 0x08, 0xa2, 0x58, 0xe0, 0x7b, 0xde, 0x82, 0xf0,
    0x9d, 0x1f, 0x0c, 0xc4, 0x9a, 0x45, 0xf7, 0x86, 0x2f, 0xf4, 0x4c, 0x36, 0xe4, 0xd0, 0x35, 0x8f,
    0x22, 0x00, 0xf6, 0x9f, 0x28, 0xe8, 0xd7, 0x85, 0x20, 0x78, 0x97, 0x5c, 0x6d, 0xfc, 0x20, 0xf0,
    0x11, 0x9d, 0x5b, 0x87, 0xbc, 0xcb, 0xd0, 0x16, 0xa2, 0x40, 0xdf, 0xa5, 0x4a, 0x88, 0xec, 0xfc,
    0x54, 0xfa, 0x42, 0x03, 0x9f, 0xc3, 0x46, 0x4f, 0x19, 0x70, 0x49, 0xd3, 0xa5, 0xad, 0x61, 0x22,
    0x8f, 0x65, 0xd0, 0x64, 0x59, 0x92, 0x3f, 0xfa, 0xda, 0x27, 0x34, 0xbf, 0xdd, 0xd0, 0x12, 0x7d,
    0xbf, 0x4c, 0xda, 0xde, 0xc9, 0x6c, 0x40, 0xf7, 0xb3, 0x02, 0x03, 0x01, 0x00, 0x01, 0xa3, 0x63,
    0x30, 0x61, 0x30, 0x1d, 0x06, 0x03, 0x55, 0x1d, 0x0e, 0x04, 0x16, 0x04, 0x14, 0x46, 0x18, 0xdd,
    0xab, 0x2e, 0x40, 0xe8, 0x94, 0xf7, 0x1d, 0x76, 0xdc, 0x35, 0x2c, 0xfd, 0x4f, 0x35, 0x50, 0x1e,
    0xf3, 0x30, 0x1f, 0x06, 0x03, 0x55, 0x1d, 0x23, 0x04, 0x18, 0x30, 0x16, 0x80, 0x14, 0x46, 0x18,
    0xdd, 0xab, 0x2e, 0x40, 0xe8, 0x94, 0xf7, 0x1d, 0x76, 0xdc, 0x35, 0x2c, 0xfd, 0x4f, 0x35, 0x50,
    0x1e, 0xf3, 0x30, 0x0f, 0x06, 0x03, 0x55, 0x1d, 0x13, 0x01, 0x01, 0xff, 0x04, 0x05, 0x30, 0x03,
    0x01, 0x01, 0xff, 0x30, 0x0e, 0x06, 0x03, 0x55, 0x1d, 0x0f, 0x01, 0x01, 0xff, 0x04, 0x04, 0x03,
    0x02, 0x01, 0x06, 0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b,
    0x05, 0x00, 0x03, 0x82, 0x01, 0x01, 0x00, 0x37, 0xe1, 0xcf, 0x43, 0xe8, 0x94, 0x46, 0xf7, 0xf9,
    0x33, 0x81, 0x94, 0x6f, 0x4f, 0x09, 0x77, 0xd8, 0xc1, 0x65, 0x72, 0xca, 0x69, 0xce, 0xaf, 0xc5,
    0x62, 0x4f, 0xaa, 0xca, 0x0f, 0xfd, 0x34, 0x2b, 0x2d, 0x63, 0x87, 0xcf, 0x1f, 0xa7, 0xde, 0x0f,
    0x8e, 0x3e, 0x1d, 0xec, 0x8a, 0xf1, 0xf9, 0x22, 0x7a, 0xc5, 0x45, 0xd7, 0x68, 0x13, 0xcf, 0x87,
    0x1e, 0x9c, 0x56, 0x89, 0x18, 0x0a, 0x27, 0xbf, 0x79, 0x9a, 0x98, 0x91, 0x17, 0x10, 0x34, 0x06,
    0xdf, 0xc6, 0x25, 0xd4, 0x08, 0x65, 0xcf, 0x94, 0x9c, 0x8c, 0xa9, 0x71, 0x9f, 0xf5, 0xb8, 0xe3,
    0xfe, 0x8f, 0x2a, 0x59, 0x5b, 0x8c, 0xd6, 0xb8, 0xa2, 0xd6, 0x6c, 0xc1, 0xbb, 0xc8, 0xdc, 0x05,
    0x22, 0xb2, 0xca, 0x42, 0xe4, 0xba, 0x3a, 0x3c, 0xde, 0x05, 0x80, 0x55, 0x21, 0xdb, 0x29, 0xf9,
    0x13, 0x8e, 0x36, 0xc1, 0xde, 0xa7, 0x30, 0x50, 0x3f, 0x9c, 0x9e, 0x34, 0x91, 0x46, 0xb4, 0x95,
    0x73, 0x7e, 0xe7, 0x14, 0x90, 0x41, 0x6c, 0x83, 0xb3, 0x59, 0x18, 0x7e, 0x87, 0xc6, 0x21, 0xb0,
    0x81, 0xbf, 0xc4, 0xb3, 0xb8, 0x61, 0x19, 0xf3, 0x58, 0xa7, 0x17, 0x70, 0x92, 0xc9, 0x2a, 0x5c,
    0x19, 0x96, 0x3d, 0x44, 0xf9, 0x07, 0xcc, 0x1b, 0x56, 0x50, 0xd1, 0x42, 0xa5, 0x07, 0x13, 0x67,
    0x1c, 0x11, 0x93, 0x06, 0xd4, 0x92, 0xee, 0x92, 0xb7, 0x57, 0x6c, 0xdd, 0xac, 0x87, 0x9b, 0xef,
    0xbf, 0x7d, 0x01, 0x39, 0xa2, 0x66, 0x4d, 0xd9, 0x21, 0x78, 0x9f, 0x72, 0x75, 0x25, 0x9c, 0x75,
    0x54, 0xec, 0xff, 0x8f, 0xc9, 0xf3, 0x7c, 0xf4, 0xdf, 0xef, 0x1a, 0xae, 0xb0, 0x94, 0xbd, 0xff,
    0x1e, 0xc8, 0x30, 0xbd, 0x82, 0x40, 0xa6, 0xec, 0x6d, 0xe9, 0x2d, 0x82, 0x3b, 0xbc, 0xb8, 0x22,
    0x4a, 0x61, 0x57, 0x25, 0x95, 0x42, 0xc9,
    ];

    const TEST_SERVER_CERT_DER: &[u8] = &
    [
    0x30, 0x82, 0x03, 0x51, 0x30, 0x82, 0x02, 0x39, 0xa0, 0x03, 0x02, 0x01, 0x02, 0x02, 0x14, 0x51,
    0x43, 0x84, 0x36, 0xee, 0xea, 0x59, 0x13, 0x69, 0x50, 0x96, 0xc8, 0xed, 0xac, 0xcb, 0x77, 0x52,
    0xf7, 0xfd, 0x18, 0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b,
    0x05, 0x00, 0x30, 0x20, 0x31, 0x1e, 0x30, 0x1c, 0x06, 0x03, 0x55, 0x04, 0x03, 0x0c, 0x15, 0x4e,
    0x45, 0x52, 0x56, 0x44, 0x65, 0x73, 0x6b, 0x20, 0x51, 0x55, 0x49, 0x43, 0x20, 0x54, 0x65, 0x73,
    0x74, 0x20, 0x43, 0x41, 0x30, 0x20, 0x17, 0x0d, 0x32, 0x36, 0x30, 0x39, 0x32, 0x38, 0x31, 0x32,
    0x31, 0x33, 0x33, 0x36, 0x5a, 0x18, 0x0f, 0x32, 0x31, 0x32, 0x36, 0x30, 0x39, 0x30, 0x34, 0x31,
    0x32, 0x31, 0x33, 0x33, 0x36, 0x5a, 0x30, 0x14, 0x31, 0x12, 0x30, 0x10, 0x06, 0x03, 0x55, 0x04,
    0x03, 0x0c, 0x09, 0x6c, 0x6f, 0x63, 0x61, 0x6c, 0x68, 0x6f, 0x73, 0x74, 0x30, 0x82, 0x01, 0x22,
    0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05, 0x00, 0x03,
    0x82, 0x01, 0x0f, 0x00, 0x30, 0x82, 0x01, 0x0a, 0x02, 0x82, 0x01, 0x01, 0x00, 0xac, 0x7b, 0xcc,
    0xed, 0xa6, 0xcb, 0x5d, 0x2c, 0xca, 0x2f, 0xb0, 0xc4, 0x78, 0x0c, 0x24, 0x26, 0x90, 0x5c, 0x85,
    0x05, 0xe7, 0x67, 0xd5, 0xda, 0x20, 0x0b, 0xb9, 0xc9, 0x3c, 0x2c, 0x16, 0xcc, 0x60, 0xa3, 0xb2,
    0x8c, 0x95, 0xb4, 0xc1, 0x29, 0x78, 0x21, 0x07, 0x1b, 0x07, 0x64, 0x51, 0x9a, 0xce, 0x7c, 0x3d,
    0x3f, 0x1a, 0x77, 0xe6, 0xf1, 0x5d, 0xce, 0x12, 0xa7, 0x2b, 0xa8, 0xe3, 0x63, 0xef, 0x3c, 0x27,
    0x32, 0x76, 0x12, 0x6d, 0xfc, 0x07, 0x43, 0xc7, 0x25, 0x0f, 0x83, 0x8c, 0x90, 0x8b, 0x15, 0x4a,
    0x76, 0xd2, 0x76, 0x05, 0x2c, 0x3a, 0xd1, 0xbe, 0xde, 0x77, 0x34, 0xf6, 0x40, 0x89, 0x5d, 0x59,
    0x26, 0x0c, 0xa8, 0xff, 0xda, 0xf6, 0x37, 0x76, 0x2f, 0xbe, 0x13, 0x48, 0x70, 0xe8, 0xb1, 0x7d,
    0x87, 0xe6, 0x9a, 0xea, 0xc5, 0x26, 0x52, 0xc2, 0x29, 0xc4, 0x12, 0x00, 0x21, 0x7f, 0x54, 0xc3,
    0x95, 0xdb, 0x02, 0x2b, 0x51, 0x24, 0x8c, 0x99, 0x47, 0x4b, 0x00, 0x3c, 0xdf, 0x5f, 0xc6, 0x3d,
    0x2f, 0xc3, 0x01, 0x61, 0xdb, 0x50, 0xc3, 0x3a, 0xb5, 0x64, 0xc3, 0xa4, 0xdf, 0x64, 0x5b, 0x94,
    0x01, 0xc4, 0xd1, 0x70, 0xd2, 0x10, 0x64, 0x60, 0xf2, 0x0f, 0x25, 0x64, 0xb3, 0xed, 0x51, 0xde,
    0x9b, 0x6a, 0xee, 0x5d, 0x60, 0x38, 0xbe, 0x78, 0xab, 0x82, 0x08, 0x66, 0xde, 0xe3, 0x32, 0x9f,
    0x05, 0x9f, 0xe3, 0x58, 0x0d, 0x93, 0xe5, 0x57, 0x71, 0x49, 0x5b, 0xe2, 0xf9, 0x48, 0x26, 0xc2,
    0x32, 0xa1, 0xe5, 0x6d, 0x13, 0x9e, 0x4d, 0x27, 0xd5, 0xc3, 0xc7, 0x54, 0x6d, 0xcd, 0xab, 0xc1,
    0x28, 0xdb, 0x03, 0x27, 0x2c, 0x24, 0x7a, 0x92, 0xee, 0x14, 0xdb, 0xc7, 0xe2, 0x77, 0x78, 0xff,
    0x8f, 0x95, 0xf4, 0xf6, 0x64, 0x65, 0xda, 0x72, 0xac, 0x8a, 0x5b, 0xfd, 0x31, 0x02, 0x03, 0x01,
    0x00, 0x01, 0xa3, 0x81, 0x8c, 0x30, 0x81, 0x89, 0x30, 0x0c, 0x06, 0x03, 0x55, 0x1d, 0x13, 0x01,
    0x01, 0xff, 0x04, 0x02, 0x30, 0x00, 0x30, 0x0e, 0x06, 0x03, 0x55, 0x1d, 0x0f, 0x01, 0x01, 0xff,
    0x04, 0x04, 0x03, 0x02, 0x05, 0xa0, 0x30, 0x13, 0x06, 0x03, 0x55, 0x1d, 0x25, 0x04, 0x0c, 0x30,
    0x0a, 0x06, 0x08, 0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01, 0x30, 0x14, 0x06, 0x03, 0x55,
    0x1d, 0x11, 0x04, 0x0d, 0x30, 0x0b, 0x82, 0x09, 0x6c, 0x6f, 0x63, 0x61, 0x6c, 0x68, 0x6f, 0x73,
    0x74, 0x30, 0x1d, 0x06, 0x03, 0x55, 0x1d, 0x0e, 0x04, 0x16, 0x04, 0x14, 0x3b, 0x52, 0x7e, 0xda,
    0xb4, 0xdc, 0xc6, 0x9b, 0x5a, 0x6f, 0xa4, 0x49, 0x83, 0xc9, 0x22, 0xb2, 0x4f, 0x7a, 0x54, 0x0f,
    0x30, 0x1f, 0x06, 0x03, 0x55, 0x1d, 0x23, 0x04, 0x18, 0x30, 0x16, 0x80, 0x14, 0x46, 0x18, 0xdd,
    0xab, 0x2e, 0x40, 0xe8, 0x94, 0xf7, 0x1d, 0x76, 0xdc, 0x35, 0x2c, 0xfd, 0x4f, 0x35, 0x50, 0x1e,
    0xf3, 0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b, 0x05, 0x00,
    0x03, 0x82, 0x01, 0x01, 0x00, 0x1f, 0xf9, 0x6a, 0x9f, 0x2e, 0x41, 0xc4, 0x3d, 0xdd, 0x20, 0x01,
    0x15, 0x02, 0xd1, 0x2f, 0xf8, 0x5a, 0xf5, 0x92, 0x07, 0xee, 0x24, 0x74, 0x5b, 0x18, 0x40, 0x27,
    0x02, 0xa0, 0xd1, 0x02, 0x87, 0x1f, 0x61, 0x2a, 0x05, 0x49, 0x6a, 0x69, 0x00, 0x40, 0x69, 0xf2,
    0x56, 0xa1, 0xf2, 0x7d, 0x25, 0x39, 0xa8, 0x6c, 0x5c, 0x47, 0x38, 0xcc, 0xff, 0x56, 0x30, 0x56,
    0x4c, 0xf0, 0x10, 0x94, 0x6e, 0x9e, 0xef, 0xa4, 0x81, 0x25, 0xe7, 0x86, 0x17, 0x06, 0xb0, 0x96,
    0xf4, 0x9f, 0x8b, 0x05, 0x7f, 0x4b, 0x68, 0x9b, 0x4c, 0xd4, 0x96, 0x04, 0x6a, 0x7c, 0x4c, 0x5f,
    0xca, 0x93, 0x14, 0xf0, 0xf2, 0xc1, 0x7f, 0x9e, 0x7d, 0xa9, 0xaa, 0x53, 0xce, 0x92, 0xe0, 0x34,
    0x49, 0x2e, 0xba, 0x95, 0x30, 0x71, 0x97, 0x15, 0x41, 0x8f, 0xda, 0xaa, 0x25, 0x4e, 0x6c, 0x1d,
    0x12, 0x0b, 0x31, 0x30, 0x12, 0xe3, 0x81, 0xf4, 0x6c, 0x13, 0x53, 0xd1, 0x2e, 0xc7, 0xfb, 0xc5,
    0x2a, 0xaa, 0xf1, 0xa1, 0x33, 0x40, 0xe0, 0x60, 0x20, 0x68, 0x47, 0x43, 0xe3, 0x39, 0x47, 0xf6,
    0x3a, 0x33, 0x18, 0xda, 0x9c, 0xa4, 0x41, 0x48, 0x09, 0xfa, 0xdc, 0x05, 0xf1, 0x24, 0xc9, 0xd2,
    0x4b, 0x71, 0x84, 0xe8, 0x67, 0x3b, 0x32, 0x7d, 0x8b, 0x2a, 0x9d, 0xc1, 0xaf, 0x1b, 0x66, 0xcd,
    0xa4, 0x5d, 0xda, 0x23, 0xc6, 0xff, 0x94, 0xcc, 0x81, 0xe2, 0x8e, 0x51, 0x2a, 0x02, 0xf1, 0x07,
    0x2c, 0xfd, 0x1f, 0x72, 0xf3, 0x2b, 0x2d, 0x75, 0x49, 0x4d, 0xc8, 0xb9, 0x79, 0x41, 0xf1, 0xfb,
    0x69, 0x7e, 0x61, 0xce, 0x62, 0x02, 0xcc, 0xe2, 0x22, 0xe8, 0x3d, 0x75, 0xc7, 0x31, 0x7c, 0xa7,
    0xb6, 0x70, 0xd5, 0x27, 0x2f, 0x3f, 0x8b, 0x5d, 0x7c, 0x96, 0xd7, 0xe8, 0x8b, 0xbc, 0xce, 0xc5,
    0xd9, 0x1e, 0x0c, 0x50, 0x01,
    ];

    const TEST_SERVER_KEY_DER: &[u8] = &
    [
    0x30, 0x82, 0x04, 0xbe, 0x02, 0x01, 0x00, 0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7,
    0x0d, 0x01, 0x01, 0x01, 0x05, 0x00, 0x04, 0x82, 0x04, 0xa8, 0x30, 0x82, 0x04, 0xa4, 0x02, 0x01,
    0x00, 0x02, 0x82, 0x01, 0x01, 0x00, 0xac, 0x7b, 0xcc, 0xed, 0xa6, 0xcb, 0x5d, 0x2c, 0xca, 0x2f,
    0xb0, 0xc4, 0x78, 0x0c, 0x24, 0x26, 0x90, 0x5c, 0x85, 0x05, 0xe7, 0x67, 0xd5, 0xda, 0x20, 0x0b,
    0xb9, 0xc9, 0x3c, 0x2c, 0x16, 0xcc, 0x60, 0xa3, 0xb2, 0x8c, 0x95, 0xb4, 0xc1, 0x29, 0x78, 0x21,
    0x07, 0x1b, 0x07, 0x64, 0x51, 0x9a, 0xce, 0x7c, 0x3d, 0x3f, 0x1a, 0x77, 0xe6, 0xf1, 0x5d, 0xce,
    0x12, 0xa7, 0x2b, 0xa8, 0xe3, 0x63, 0xef, 0x3c, 0x27, 0x32, 0x76, 0x12, 0x6d, 0xfc, 0x07, 0x43,
    0xc7, 0x25, 0x0f, 0x83, 0x8c, 0x90, 0x8b, 0x15, 0x4a, 0x76, 0xd2, 0x76, 0x05, 0x2c, 0x3a, 0xd1,
    0xbe, 0xde, 0x77, 0x34, 0xf6, 0x40, 0x89, 0x5d, 0x59, 0x26, 0x0c, 0xa8, 0xff, 0xda, 0xf6, 0x37,
    0x76, 0x2f, 0xbe, 0x13, 0x48, 0x70, 0xe8, 0xb1, 0x7d, 0x87, 0xe6, 0x9a, 0xea, 0xc5, 0x26, 0x52,
    0xc2, 0x29, 0xc4, 0x12, 0x00, 0x21, 0x7f, 0x54, 0xc3, 0x95, 0xdb, 0x02, 0x2b, 0x51, 0x24, 0x8c,
    0x99, 0x47, 0x4b, 0x00, 0x3c, 0xdf, 0x5f, 0xc6, 0x3d, 0x2f, 0xc3, 0x01, 0x61, 0xdb, 0x50, 0xc3,
    0x3a, 0xb5, 0x64, 0xc3, 0xa4, 0xdf, 0x64, 0x5b, 0x94, 0x01, 0xc4, 0xd1, 0x70, 0xd2, 0x10, 0x64,
    0x60, 0xf2, 0x0f, 0x25, 0x64, 0xb3, 0xed, 0x51, 0xde, 0x9b, 0x6a, 0xee, 0x5d, 0x60, 0x38, 0xbe,
    0x78, 0xab, 0x82, 0x08, 0x66, 0xde, 0xe3, 0x32, 0x9f, 0x05, 0x9f, 0xe3, 0x58, 0x0d, 0x93, 0xe5,
    0x57, 0x71, 0x49, 0x5b, 0xe2, 0xf9, 0x48, 0x26, 0xc2, 0x32, 0xa1, 0xe5, 0x6d, 0x13, 0x9e, 0x4d,
    0x27, 0xd5, 0xc3, 0xc7, 0x54, 0x6d, 0xcd, 0xab, 0xc1, 0x28, 0xdb, 0x03, 0x27, 0x2c, 0x24, 0x7a,
    0x92, 0xee, 0x14, 0xdb, 0xc7, 0xe2, 0x77, 0x78, 0xff, 0x8f, 0x95, 0xf4, 0xf6, 0x64, 0x65, 0xda,
    0x72, 0xac, 0x8a, 0x5b, 0xfd, 0x31, 0x02, 0x03, 0x01, 0x00, 0x01, 0x02, 0x82, 0x01, 0x00, 0x4d,
    0xb6, 0x43, 0xa9, 0x09, 0x7e, 0xd3, 0xde, 0xa3, 0xc3, 0xe3, 0xf1, 0x49, 0x23, 0x33, 0x40, 0x7a,
    0xc7, 0x6c, 0x99, 0xb1, 0xde, 0x8b, 0x30, 0x09, 0x43, 0x2f, 0x34, 0x37, 0x1b, 0xa8, 0x33, 0xf2,
    0x10, 0x9f, 0x18, 0x0f, 0x35, 0x4e, 0xd8, 0x57, 0xcb, 0x0d, 0xb0, 0x04, 0x5f, 0x13, 0x13, 0x5c,
    0x18, 0x06, 0xe2, 0x41, 0x13, 0x27, 0xa6, 0xb4, 0xfc, 0x6d, 0x54, 0x0b, 0x6c, 0x3e, 0xf3, 0x5e,
    0x2c, 0xec, 0x7e, 0x24, 0x4b, 0x7e, 0x69, 0x50, 0x7e, 0x9d, 0xa1, 0x99, 0x81, 0x4c, 0x1a, 0x2d,
    0xc7, 0xec, 0x72, 0x67, 0xb5, 0x8b, 0xf4, 0x17, 0x16, 0x3c, 0x58, 0x70, 0x3e, 0x18, 0xfb, 0x0f,
    0xc8, 0xd2, 0xab, 0x3d, 0x86, 0x01, 0x2c, 0xce, 0xc5, 0x43, 0x47, 0x14, 0x6d, 0x0c, 0xb4, 0xee,
    0x6a, 0x55, 0xa4, 0x1e, 0xe6, 0xca, 0x83, 0x04, 0x16, 0xc2, 0x6b, 0x5d, 0xef, 0x3c, 0x46, 0x1d,
    0x0d, 0xba, 0x4a, 0x87, 0x26, 0xa6, 0xa7, 0x55, 0x63, 0xab, 0x7f, 0x4b, 0x6b, 0x7f, 0xed, 0x38,
    0xe8, 0x4d, 0xde, 0xe0, 0xc0, 0x77, 0xc4, 0x8c, 0x89, 0x89, 0x38, 0xfd, 0xfb, 0x2c, 0xa8, 0x50,
    0xbc, 0x75, 0x81, 0xb0, 0x54, 0x5f, 0x81, 0x8c, 0xa7, 0x9e, 0xb9, 0x50, 0x89, 0xbe, 0x2f, 0x80,
    0xb0, 0x60, 0x9d, 0xd1, 0x12, 0xe7, 0xcc, 0x7c, 0x7a, 0xe9, 0xe4, 0xab, 0x30, 0xb0, 0xe4, 0x47,
    0x11, 0xa4, 0xe5, 0x23, 0x10, 0x35, 0xba, 0x2b, 0x9a, 0x94, 0x14, 0x0d, 0xe2, 0x43, 0x46, 0x18,
    0x9b, 0xfd, 0xb4, 0x49, 0xb6, 0x20, 0x3b, 0x31, 0xb7, 0x8b, 0x6f, 0xe8, 0x3e, 0x37, 0x80, 0x9f,
    0x63, 0x5d, 0x4f, 0xce, 0x06, 0xb0, 0x69, 0x55, 0xbb, 0xfa, 0xce, 0xad, 0x54, 0xf8, 0xec, 0x89,
    0x96, 0xc3, 0xb6, 0x00, 0x76, 0xf1, 0xfa, 0x3a, 0x61, 0xbc, 0x6f, 0x25, 0x17, 0x3c, 0x2f, 0x02,
    0x81, 0x81, 0x00, 0xe8, 0x7e, 0x77, 0x02, 0x03, 0x24, 0xf5, 0xc6, 0x40, 0x87, 0x00, 0xa1, 0xb4,
    0xd4, 0x24, 0x84, 0xb9, 0xea, 0x00, 0xea, 0x85, 0xbc, 0x68, 0x58, 0xc0, 0x89, 0xfb, 0x10, 0x64,
    0x5e, 0x1c, 0x02, 0x07, 0x20, 0xf3, 0xfd, 0xb0, 0x4a, 0x36, 0x4d, 0x09, 0x93, 0xb3, 0xff, 0x8a,
    0xe8, 0x8b, 0x3a, 0x3e, 0x6a, 0xf1, 0x7d, 0x75, 0xb7, 0x2f, 0xfd, 0xdf, 0xbb, 0x0d, 0xb7, 0x89,
    0xa8, 0x30, 0x47, 0xce, 0x3c, 0x6f, 0xea, 0xaf, 0x70, 0x45, 0x71, 0xbe, 0xa5, 0x91, 0x98, 0xc6,
    0xe0, 0x7f, 0x75, 0xd7, 0x31, 0xad, 0x8a, 0x88, 0xc5, 0xf3, 0xee, 0xfe, 0x06, 0x99, 0xec, 0x04,
    0x26, 0xe7, 0x10, 0x26, 0xa4, 0xc7, 0x86, 0x57, 0x1e, 0x12, 0xac, 0x1d, 0x94, 0x00, 0x5e, 0xf2,
    0xc8, 0x26, 0x0e, 0x11, 0x80, 0x1d, 0x04, 0x32, 0x20, 0xfe, 0xec, 0x22, 0xcd, 0x9c, 0x7d, 0xd9,
    0x11, 0x30, 0xaf, 0x02, 0x81, 0x81, 0x00, 0xbd, 0xec, 0x1d, 0x28, 0xe2, 0x24, 0x04, 0x8d, 0xb1,
    0xee, 0x7b, 0xbc, 0xc6, 0x62, 0x07, 0x08, 0xb9, 0x27, 0xa6, 0xff, 0xde, 0xdb, 0x11, 0x48, 0x5e,
    0x53, 0xe4, 0xab, 0xd3, 0xca, 0xff, 0x40, 0x86, 0xe2, 0xaa, 0xbf, 0x7b, 0x2c, 0xbf, 0x2c, 0x82,
    0x92, 0x69, 0xf2, 0x70, 0xca, 0x67, 0xb4, 0xfc, 0x7e, 0x85, 0x56, 0x3a, 0x65, 0x1b, 0x6a, 0xb1,
    0xc3, 0x73, 0xd5, 0xa7, 0xbe, 0x27, 0xb2, 0x11, 0x36, 0x6d, 0x9b, 0xbb, 0x12, 0x02, 0xc5, 0x17,
    0x11, 0x97, 0x53, 0x8a, 0x3e, 0x3d, 0x19, 0x31, 0x5c, 0xed, 0x03, 0xf8, 0x23, 0x9d, 0x13, 0xb3,
    0xf2, 0x0c, 0x32, 0xf3, 0x7e, 0x40, 0xb3, 0x31, 0xdf, 0x07, 0xff, 0xd5, 0xbd, 0x15, 0x05, 0x24,
    0xa1, 0x9d, 0xc0, 0x25, 0xa7, 0xf3, 0x31, 0x2e, 0xed, 0xc7, 0xd2, 0x54, 0x6a, 0xf0, 0xb1, 0x58,
    0x7c, 0x9b, 0x26, 0x72, 0x80, 0x68, 0x1f, 0x02, 0x81, 0x81, 0x00, 0xe5, 0xf6, 0xf1, 0x50, 0x14,
    0x3c, 0x22, 0xbe, 0x8e, 0x64, 0xfa, 0xc2, 0xf8, 0x52, 0x3e, 0x2c, 0xea, 0x98, 0x03, 0x7f, 0xf5,
    0xf8, 0x7e, 0x5e, 0x0b, 0x54, 0x6f, 0xf9, 0xae, 0xcd, 0x47, 0x76, 0xda, 0x06, 0x46, 0x50, 0xd0,
    0x67, 0x17, 0x7e, 0xeb, 0xd2, 0x25, 0x60, 0xc6, 0xcd, 0x6d, 0xa9, 0x96, 0xc3, 0xc1, 0x4a, 0x0f,
    0x7d, 0xbb, 0x02, 0xaa, 0xa2, 0x22, 0xd7, 0x40, 0x5a, 0x14, 0x27, 0x72, 0x5f, 0x65, 0x74, 0x05,
    0x44, 0x4f, 0xec, 0x4a, 0x5f, 0x0a, 0xbc, 0xcb, 0x3a, 0x93, 0xd8, 0xc3, 0x9a, 0x67, 0xc5, 0x77,
    0xb4, 0x15, 0xac, 0x77, 0xa7, 0x9f, 0xe8, 0x4b, 0xd3, 0x0f, 0x0a, 0x72, 0xae, 0xda, 0x8c, 0x8e,
    0xef, 0x38, 0x18, 0xf6, 0xc8, 0xc5, 0xf5, 0x24, 0xbf, 0xc4, 0xa4, 0x75, 0xba, 0xfa, 0xf8, 0x83,
    0x7d, 0x0f, 0xaa, 0x12, 0x62, 0xbe, 0x3f, 0xdc, 0xcd, 0x9c, 0x11, 0x02, 0x81, 0x80, 0x14, 0xc3,
    0x84, 0xa8, 0x9c, 0x98, 0xad, 0x7a, 0xc4, 0x52, 0x3b, 0x6a, 0xf7, 0x11, 0x6e, 0x8d, 0x70, 0x98,
    0xba, 0x34, 0x4d, 0x2c, 0x0c, 0x26, 0xaa, 0x51, 0x67, 0xb5, 0xb5, 0x71, 0x03, 0x19, 0x0d, 0xe6,
    0x28, 0x1e, 0xc9, 0x1b, 0xaa, 0x46, 0xf6, 0x7b, 0x85, 0x63, 0xc1, 0x1b, 0x0f, 0xdd, 0x84, 0xa1,
    0x5c, 0x78, 0x81, 0xe7, 0xdd, 0xe8, 0x7b, 0x48, 0xd0, 0x18, 0x32, 0xbf, 0xa2, 0x5d, 0x60, 0x6e,
    0x5f, 0xeb, 0x5f, 0xb7, 0x67, 0x60, 0x1e, 0xd6, 0x88, 0x81, 0xd4, 0xa2, 0x5b, 0x51, 0xae, 0xc8,
    0xe7, 0x0c, 0xc1, 0x0b, 0x3b, 0xb8, 0x14, 0xbb, 0x48, 0xc4, 0x25, 0x44, 0xcf, 0x54, 0x08, 0x06,
    0xc7, 0x3c, 0x1c, 0x25, 0x10, 0xf0, 0x40, 0x01, 0xff, 0x5a, 0x2b, 0x83, 0xc2, 0x1d, 0xc5, 0x70,
    0xaf, 0xa0, 0xfa, 0x23, 0xba, 0xee, 0xd8, 0xaa, 0xbe, 0xd7, 0xa4, 0x3b, 0x0f, 0xa7, 0x02, 0x81,
    0x81, 0x00, 0xdb, 0xce, 0xb8, 0x6e, 0xf8, 0xbc, 0x2d, 0x57, 0xae, 0xae, 0x6b, 0x81, 0xf1, 0x2a,
    0x5a, 0x91, 0xaa, 0x89, 0xe3, 0x92, 0x9d, 0x9d, 0xbd, 0x4c, 0x85, 0x4e, 0x3c, 0x2a, 0x46, 0xf1,
    0x2d, 0x60, 0x75, 0x38, 0xda, 0x1a, 0x48, 0xe6, 0x92, 0xb1, 0xa2, 0xd0, 0xa8, 0x89, 0xb0, 0x10,
    0xfe, 0x57, 0xa8, 0x7b, 0xaa, 0x48, 0x3d, 0xa7, 0x87, 0x0c, 0x5b, 0x0e, 0x4f, 0x66, 0x22, 0x63,
    0xf5, 0xcc, 0x24, 0x85, 0xb8, 0xb9, 0x39, 0x25, 0x73, 0x01, 0x19, 0xca, 0x65, 0x70, 0x68, 0x09,
    0x8c, 0xd6, 0x78, 0x94, 0xb2, 0xd8, 0xde, 0x8d, 0xae, 0x0f, 0x60, 0x43, 0xd3, 0xd1, 0xa1, 0x78,
    0xfe, 0xf4, 0x16, 0x6b, 0xd9, 0x22, 0xcb, 0xe2, 0x3b, 0x15, 0x9d, 0xca, 0x6e, 0x3c, 0x03, 0x2a,
    0x79, 0x82, 0xdd, 0x65, 0xf3, 0x2d, 0xeb, 0xbd, 0x4a, 0xff, 0xe9, 0xde, 0xd2, 0x56, 0xa2, 0xeb,
    0x4f, 0x16,
    ];

    fn server_config() -> Result<ServerConfig, Box<dyn std::error::Error + Send + Sync>> {
        let cert = CertificateDer::from(TEST_SERVER_CERT_DER.to_vec());
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(TEST_SERVER_KEY_DER.to_vec()));
        Ok(ServerConfig::with_single_cert(vec![cert], key)?)
    }

    fn client_config() -> Result<ClientConfig, Box<dyn std::error::Error + Send + Sync>> {
        let mut roots = RootCertStore::empty();
        // 根库装的是 **CA**，不是服务端叶（照抄探针 client_config）。
        roots.add(CertificateDer::from(TEST_CA_DER.to_vec()))?;
        let inner = quinn::rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let quic = QuicClientConfig::try_from(inner)?;
        Ok(ClientConfig::new(Arc::new(quic)))
    }

    /// 字节对账测试 A：`FramedStream` 走 QUIC 双向 256 KiB，双方 `set_key`，
    /// 载荷逐字节相等（核心是证明 2a 的落点在真实 `FramedStream` 上成立）。
    #[tokio::test]
    async fn quic_framed_duplex_256k_encrypted() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        install_ring_provider();
        let server_cfg = server_config()?;
        let client_cfg = client_config()?;

        let server = Endpoint::server(server_cfg, "127.0.0.1:0".parse()?)?;
        let addr = server.local_addr()?;
        let key = secretbox::gen_key();

        const N: usize = 256 * 1024;
        let payload_client = (0..N).map(|i| (i % 251) as u8).collect::<Vec<_>>();

        // 闭包会整体 move 捕获：每个 move 进闭包的量都先 clone 一份留在外层。
        let key_server = key.clone();
        let payload_check = payload_client.clone();

        let server_task = tokio::spawn(async move {
            let incoming = server.accept().await.ok_or("server accept ended")?;
            let conn = incoming.await?;
            let (send, recv) = conn.accept_bi().await?;
            let mut fs = quic_into_framed_stream(recv, send, addr);
            fs.set_key(key_server.clone());
            let got = fs.next().await.ok_or("server next ended")??;
            if got.len() != N || got[..] != payload_check[..] {
                return Err::<(), Box<dyn std::error::Error + Send + Sync>>(
                    format!("server saw wrong payload: got {} bytes (want {})", got.len(), N).into(),
                );
            }
            fs.send_raw(got.to_vec()).await?; // echo
            // 生命周期纪律：先 close 再让连接自行结束，不提前 drop（见 P1b 笔记）。
            conn.closed().await;
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        });

        let mut client = Endpoint::client("127.0.0.1:0".parse()?)?;
        client.set_default_client_config(client_cfg);
        let conn = client.connect(addr, "localhost")?.await?;
        let (send, recv) = conn.open_bi().await?;
        let mut fs = quic_into_framed_stream(recv, send, addr);
        fs.set_key(key.clone());
        fs.send_raw(payload_client.clone()).await?;
        let echoed = fs.next().await.ok_or("client next ended")??;
        client.close(0u32.into(), b"done");
        server_task.await??;

        if echoed.len() != N || echoed[..] != payload_client[..] {
            return Err(
                format!("client echoed mismatch: got {} bytes (want {})", echoed.len(), N).into(),
            );
        }
        Ok(())
    }

    /// 字节对账测试 B：不 `set_key` 时线上是明文（标记可见），`set_key` 后线上是
    /// secretbox 密文（标记不可见），且差量正好 `MACBYTES`。两端都走真实
    /// `FramedStream`：服务端故意**不** `set_key`，因此 `next()` 取到的是未经
    /// 解密的线上字节。
    #[tokio::test]
    async fn quic_wire_plaintext_vs_sealed() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        install_ring_provider();
        let server_cfg = server_config()?;
        let client_cfg = client_config()?;

        let marker = b"NERVDESK-QUIC-PLAINTEXT-MARKER".to_vec();

        // 一次回环：客户端 FramedStream（可选 set_key）发标记；服务端 FramedStream
        // 不 set_key，取回“线上字节”。返回 (线上载荷长度, 标记是否明文可见)。
        async fn one_roundtrip(
            marker: Vec<u8>,
            server_cfg: ServerConfig,
            client_cfg: ClientConfig,
            with_key: bool,
        ) -> Result<(usize, bool), Box<dyn std::error::Error + Send + Sync>> {
            let server = Endpoint::server(server_cfg, "127.0.0.1:0".parse()?)?;
            let addr = server.local_addr()?;
            let key = secretbox::gen_key();

            let server_task_key = key.clone();
            let server_task_use_key = with_key;
            let server_task = tokio::spawn(async move {
                let incoming = server.accept().await.ok_or("server accept ended")?;
                let conn = incoming.await?;
                let (send, recv) = conn.accept_bi().await?;
                let mut fs = quic_into_framed_stream(recv, send, addr);
                // 故意不 set_key：next() 返回的就是线路上未经解密的字节
                // （明文可见 or secretbox 密文，视 with_key 而定）。
                let wire = fs.next().await.ok_or("server next ended")??;
                // 回 ack：加密状态必须与 with_key 对称 —— 客户端带 key 时回包
                // 须为 secretbox 密文（否则客户端解明文报 decryption error）；
                // 客户端无 key 时回包必须明文（否则客户端拿到密文 ≠ "ack"）。
                if server_task_use_key {
                    fs.set_key(server_task_key.clone());
                }
                fs.send_raw(b"ack".to_vec()).await?;
                conn.closed().await;
                Ok::<Vec<u8>, Box<dyn std::error::Error + Send + Sync>>(wire.to_vec())
            });

            let mut client = Endpoint::client("127.0.0.1:0".parse()?)?;
            client.set_default_client_config(client_cfg);
            let conn = client.connect(addr, "localhost")?.await?;
            let (send, recv) = conn.open_bi().await?;
            let mut fs = quic_into_framed_stream(recv, send, addr);
            if with_key {
                fs.set_key(key.clone());
            }
            fs.send_raw(marker.clone()).await?;
            // 等服务端 ack，确认线上字节已被消费，再收尾（消除 close 竞态）。
            let ack = fs.next().await.ok_or("client ack ended")??;
            if ack[..] != b"ack"[..] {
                return Err::<(usize, bool), Box<dyn std::error::Error + Send + Sync>>(
                    "bad ack".into(),
                );
            }
            client.close(0u32.into(), b"done");
            let wire = server_task.await??;

            let marker_visible = wire.windows(marker.len()).any(|w| w == marker);
            println!(
                "[quic_wire] with_key={} wire={} bytes marker_visible={}",
                with_key,
                wire.len(),
                marker_visible
            );
            Ok((wire.len(), marker_visible))
        }

        let (plain_len, plain_visible) =
            one_roundtrip(marker.clone(), server_cfg.clone(), client_cfg.clone(), false).await?;
        let (sealed_len, sealed_visible) =
            one_roundtrip(marker.clone(), server_cfg, client_cfg, true).await?;

        if !plain_visible || sealed_visible {
            return Err("wire marker visibility wrong".into());
        }
        if sealed_len != plain_len + secretbox::MACBYTES {
            return Err(format!(
                "wire size delta wrong: plain {plain_len} vs sealed {sealed_len} (want +{})",
                secretbox::MACBYTES
            )
            .into());
        }
        Ok(())
    }

    /// 2a-3（task-27）：QUIC_MODE 行为矩阵（docs/11 §9 三档 × QUIC 可用/不可用）。
    ///
    /// 两层证据（诚实分层，绝不伪造「QUIC 生效」）：
    /// - 层 1 `prod-attempt`：直接调用产品入口 `quic_direct_attempt`（`connect()`
    ///   用的那个）。今天它**总是返回真实错误**（C1/C2 身份绑定未裁决 + 对端无
    ///   QUIC 端点）—— 因此当前生产环境的真实现状就是：`prefer` 记 warn 并回退、
    ///   `required` 上报真实错误。矩阵如实记录之。
    /// - 层 2 `primitive`：用测试 CA 验证拨号原语本身 —— 对回环 QUIC server 拨号
    ///   成功（QUIC 可用环境）、对必然失败的对端拨号失败（QUIC 不可用环境），再叠加
    ///   `connect()` 与矩阵共用的 `quic_failure_disposition` 得出最终处置。它回答
    ///   「C1/C2 落地后，各模式会选什么传输」。
    ///
    /// 生产环境真实 QUIC 对端 × prefer/required = NOT TESTED（无 QUIC 部署）。
    #[tokio::test]
    async fn quic_mode_matrix() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        install_ring_provider();
        use crate::common::QuicMode;

        // 拨号原语：QUIC 可用环境（回环 server + 测试 CA）。成功返回 true。
        // 带握手确认（客户端发 hello → 服务端读完回 ok → 客户端读 ack），
        // 先同步数据通路再 close，避免 close 帧与服务端 accept_bi 竞态
        // （ApplicationClosed(b"done") 教训：open_bi 后立即 close 会输掉竞态）。
        async fn dial_ok(
            server_cfg: ServerConfig,
            client_cfg: ClientConfig,
        ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let server = Endpoint::server(server_cfg, "127.0.0.1:0".parse()?)?;
            let addr = server.local_addr()?;
            let server_task = tokio::spawn(async move {
                let incoming = server.accept().await.ok_or("server accept ended")?;
                let conn = incoming.await?;
                let (mut send, mut recv) = conn.accept_bi().await?;
                let mut buf = [0u8; 5];
                recv.read_exact(&mut buf).await?; // 等客户端握手字节
                send.write_all(b"ok").await?;
                conn.closed().await;
                Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
            });
            let mut client = Endpoint::client("127.0.0.1:0".parse()?)?;
            client.set_default_client_config(client_cfg);
            let conn = client.connect(addr, "localhost")?.await?;
            let (mut send, mut recv) = conn.open_bi().await?;
            send.write_all(b"hello").await?;
            let mut ack = [0u8; 2];
            recv.read_exact(&mut ack).await?; // 等服务端确认
            if ack[..] != b"ok"[..] {
                return Ok(false);
            }
            client.close(0u32.into(), b"done");
            server_task.await??;
            Ok(true)
        }

        // 拨号原语：QUIC 不可用环境（必然失败的对端，带超时防挂起）。
        // 返回 true = 拨号确认失败（QUIC 不可用），false = 意外连通。
        async fn dial_fail() -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
            let mut client = Endpoint::client("127.0.0.1:0".parse()?)?;
            client.set_default_client_config(client_config()?);
            let conn = match client.connect("127.0.0.1:1".parse()?, "localhost") {
                Ok(conn) => conn,
                Err(_) => return Ok(true), // 本地即失败：QUIC 不可用
            };
            let res = tokio::time::timeout(std::time::Duration::from_secs(3), conn).await;
            client.close(0u32.into(), b"done");
            // 超时或握手失败 = QUIC 不可用；意外连上 = false。
            Ok(!matches!(res, Ok(Ok(_))))
        }

        let server_cfg = server_config()?;
        let client_cfg = client_config()?;

        // 行: (layer, mode, attempted, chosen, fallback)
        let mut rows: Vec<(&str, &str, bool, &str, bool)> = Vec::new();

        // 层 1：产品现状（真实入口 quic_direct_attempt）。
        for mode in [QuicMode::Disabled, QuicMode::Prefer, QuicMode::Required] {
            let attempted = mode != QuicMode::Disabled;
            let (chosen, fallback) = if !attempted {
                ("original", false)
            } else {
                // R4-3（task-15）：真实入口现在骑在调用方的已打洞 socket 上，测试用一个
                // 连到无人监听的 127.0.0.1:1 的 socket，语义与旧的固定失败一致。
                let probe_socket = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await?);
                probe_socket.connect("127.0.0.1:1").await?;
                match super::quic_direct_attempt(probe_socket, &[0u8; 32], 1000).await
                {
                    Ok(_) => ("quic", false),
                    Err(_) => match super::quic_failure_disposition(mode) {
                        super::QuicFailure::Fallback => ("original+fallback(warn)", true),
                        super::QuicFailure::RealError => ("none+real-error", false),
                    },
                }
            };
            rows.push(("prod-attempt", mode.as_str(), attempted, chosen, fallback));
        }

        // 层 2：拨号原语，QUIC 可用环境。
        let ok = dial_ok(server_cfg.clone(), client_cfg.clone()).await?;
        assert!(ok, "loopback QUIC dial must succeed (测试 CA)");
        for mode in [QuicMode::Disabled, QuicMode::Prefer, QuicMode::Required] {
            let attempted = mode != QuicMode::Disabled;
            let (chosen, fallback) = if !attempted {
                ("original", false)
            } else if ok {
                ("quic", false)
            } else {
                match super::quic_failure_disposition(mode) {
                    super::QuicFailure::Fallback => ("original+fallback", true),
                    super::QuicFailure::RealError => ("none+real-error", false),
                }
            };
            rows.push(("primitive-available", mode.as_str(), attempted, chosen, fallback));
        }

        // 层 2：拨号原语，QUIC 不可用环境。
        let unavailable = dial_fail().await?; // true = 拨号确认失败（QUIC 不可用）
        assert!(unavailable, "dial to dead addr must fail");
        for mode in [QuicMode::Disabled, QuicMode::Prefer, QuicMode::Required] {
            let attempted = mode != QuicMode::Disabled;
            let (chosen, fallback) = if !attempted {
                ("original", false)
            } else if !unavailable {
                ("quic", false)
            } else {
                match super::quic_failure_disposition(mode) {
                    super::QuicFailure::Fallback => ("original+fallback", true),
                    super::QuicFailure::RealError => ("none+real-error", false),
                }
            };
            rows.push(("primitive-unavailable", mode.as_str(), attempted, chosen, fallback));
        }

        for r in &rows {
            println!(
                "[quic-matrix] layer={} mode={} attempted={} chosen={} fallback={}",
                r.0, r.1, r.2, r.3, r.4
            );
        }

        // 关键不变量断言（docs/11 §9）：
        // 1) disabled 从不尝试 QUIC，总是原传输；
        for r in rows.iter().filter(|r| r.1 == "disabled") {
            assert!(!r.2, "disabled must never attempt QUIC");
            assert_eq!(r.3, "original", "disabled must choose original transport");
        }
        // 2) prefer 在 QUIC 可用时选 QUIC 且不回退；不可用/失败时回退；
        let prefer_available = rows
            .iter()
            .find(|r| r.1 == "prefer" && r.0 == "primitive-available")
            .unwrap();
        assert_eq!(prefer_available.3, "quic", "prefer+available must choose QUIC");
        assert!(!prefer_available.4);
        for r in rows
            .iter()
            .filter(|r| r.1 == "prefer" && r.0 != "primitive-available")
        {
            assert!(r.4, "prefer without QUIC must fall back (layer {})", r.0);
        }
        // 3) required 在 QUIC 可用时选 QUIC；不可用时真实错误、不回退；
        let required_available = rows
            .iter()
            .find(|r| r.1 == "required" && r.0 == "primitive-available")
            .unwrap();
        assert_eq!(required_available.3, "quic", "required+available must choose QUIC");
        assert!(!required_available.4);
        for r in rows
            .iter()
            .filter(|r| r.1 == "required" && r.0 != "primitive-available")
        {
            assert!(!r.4, "required without QUIC must NOT fall back (layer {})", r.0);
        }
        Ok(())
    }

    /// 2a-3（task-25 / C1）：RFC 7250 RPK 握手回环测试。
    ///
    /// 跑两轮：
    /// - **正路**：服务端持 32B raw ed25519 公钥 pk；客户端 NervRpkVerifier 配同 pk；
    ///   loopback handshake 必须成功，且能交换 5B 握手字节（hello/ok）。
    /// - **反路**：客户端配错 pk（[1u8; 32]）；handshake 必须失败（UnknownIssuer）。
    ///
    /// 与 `analysis/rpk-probe`（3/3 PASS）形态一致，但本测试用产品入口
    /// `make_quic_server_config` / `make_quic_client_config`，锁 C1 在产品代码
    /// 里的「真路径」可用。
    #[tokio::test]
    async fn quic_c1_rpk_handshake_loopback()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        install_ring_provider();
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // 用 ring 直接由 32B seed 导出匹配的 32B 公钥 —— seed 与 pk 必须配对，
        // 否则 server 的证书（用 seed 签的）会被 client 用错的 pk 验签拒绝。
        let seed: [u8; 32] = [0x42u8; 32];
        let keypair = Ed25519KeyPair::from_seed_unchecked(&seed)
            .map_err(|e| format!("Ed25519KeyPair::from_seed_unchecked: {e:?}"))?;
        let mut pk = [0u8; 32];
        pk.copy_from_slice(keypair.public_key().as_ref());

        let server_cfg = super::make_quic_server_config(&pk, &seed)?;
        let client_cfg = super::make_quic_client_config(&pk)?;

        let server = Endpoint::server(server_cfg, "127.0.0.1:0".parse()?)?;
        let addr = server.local_addr()?;
        let server_task = tokio::spawn(async move {
            let incoming = server.accept().await.ok_or("server accept ended")?;
            let conn = incoming.await?;
            let (mut send, mut recv) = conn.accept_bi().await?;
            let mut buf = [0u8; 5];
            recv.read_exact(&mut buf).await?;
            send.write_all(b"ok").await?;
            conn.closed().await;
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        });

        // --- 正路：客户端配对端 pk，handshake 应成功。
        let mut client = Endpoint::client("127.0.0.1:0".parse()?)?;
        client.set_default_client_config(client_cfg);
        let conn = client.connect(addr, "localhost")?.await?;
        let (mut send, mut recv) = conn.open_bi().await?;
        send.write_all(b"hello").await?;
        let mut ack = [0u8; 2];
        recv.read_exact(&mut ack).await?;
        assert_eq!(&ack[..], b"ok", "正路：C1 RPK 握手必须 ack=ok");
        client.close(0u32.into(), b"done");
        server_task.await??;

        // --- 反路：客户端配错 pk，重新握手必须被拒。
        let wrong_pk: [u8; 32] = [1u8; 32];
        let server_cfg2 = super::make_quic_server_config(&pk, &seed)?;
        let wrong_client_cfg = super::make_quic_client_config(&wrong_pk)?;
        let server2 = Endpoint::server(server_cfg2, "127.0.0.1:0".parse()?)?;
        let addr2 = server2.local_addr()?;
        // 拿一个连接，但客户端 trust 是错 pk ⇒ handshake 应 fail。
        let mut client2 = Endpoint::client("127.0.0.1:0".parse()?)?;
        client2.set_default_client_config(wrong_client_cfg);
        let res = client2.connect(addr2, "localhost")?.await;
        client2.close(0u32.into(), b"done");
        assert!(
            res.is_err(),
            "反路：错 pk 必须 handshake 失败（got Ok = false-negative）"
        );
        Ok(())
    }

    /// P1d-tail-2：`quic_dial_quinn` 原语在 C1 RPK loopback 上端到端跑通。
    ///
    /// 证 `quic_dial_quinn` 拿到真实 `(RecvStream, SendStream)`：拨号 → 握手 →
    /// `open_bi` → 5B hello/ok → `close`。本测试是为 RT-01 与未来的
    /// `rpk-probe ↔ quic_transport` 二进制铺路：它们可以直接复用这个原语，
    /// 不需要让 product 入口 `quic_direct_attempt` 改变「当前没 QUIC 部署」
    /// 的诚实状态。
    #[tokio::test]
    async fn quic_dial_quinn_loopback()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        install_ring_provider();
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // C1 配对（与 quic_c1_rpk_handshake_loopback 同款）
        let seed: [u8; 32] = [0x42u8; 32];
        let keypair = Ed25519KeyPair::from_seed_unchecked(&seed)
            .map_err(|e| format!("Ed25519KeyPair::from_seed_unchecked: {e:?}"))?;
        let mut pk = [0u8; 32];
        pk.copy_from_slice(keypair.public_key().as_ref());

        let server_cfg = super::make_quic_server_config(&pk, &seed)?;

        let server = Endpoint::server(server_cfg, "127.0.0.1:0".parse()?)?;
        let addr = server.local_addr()?;
        let server_task = tokio::spawn(async move {
            let incoming = server.accept().await.ok_or("server accept ended")?;
            let conn = incoming.await?;
            let (mut send, mut recv) = conn.accept_bi().await?;
            let mut buf = [0u8; 5];
            recv.read_exact(&mut buf).await?;
            send.write_all(b"ok").await?;
            conn.closed().await;
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        });

        // 走 P1d 新增原语。
        let (recv, send, _peer) = super::quic_dial_quinn(
            addr,
            "127.0.0.1:0".parse()?,
            3_000,
            &pk,
        )
        .await?;
        let mut recv = recv;
        let mut send = send;
        send.write_all(b"hello").await?;
        let mut ack = [0u8; 2];
        recv.read_exact(&mut ack).await?;
        assert_eq!(&ack[..], b"ok", "quic_dial_quinn 原语 ack 必须为 ok");
        // drop send 让对端 conn.closed() 自然结束。
        drop(send);
        drop(recv);
        server_task.await??;
        Ok(())
    }

    /// R1（task-73）：**产品入口** `quic_direct_attempt` / `quic_accept_attempt` 在同进程
    /// loopback 上完成一次真实握手 + 双向字节交换。
    ///
    /// 与既有 loopback 测试的区别（这是本测试存在的唯一理由）：`quic_c1_rpk_handshake_loopback`
    /// 与 `quic_dial_quinn_loopback` 走的都是 quinn 自带的 `Endpoint::server` /
    /// `Endpoint::client` —— 由 **quinn-udp** 自己建 socket、自己跑 `UdpSocketState`。
    /// 本测试驱动的是**产品入口**：它把调用方传入的 `Arc<tokio UdpSocket>` 包成
    /// `TokioAsyncUdpSocket`（`quic_transport.rs:167`），也就是被控端/主控端在真实
    /// `client.rs:udp_nat_connect` / `rendezvous_mediator.rs:udp_nat_loop` 里传进来的那个
    /// **已 `connect()` 的打洞 socket**。因此本测试真实覆盖：
    /// - `quic_direct_attempt` `:318` → `:357`（含 `socket.peer_addr()` 取对端、
    ///   `TokioAsyncUdpSocket::try_send` / `poll_recv`、`open_bi`）；
    /// - `quic_accept_attempt` `:370` → `:404`（含 `endpoint.accept()` → `incoming` →
    ///   `accept_bi`，即 `:404` 那条「QUIC 入站连接已建立…」路径）。
    ///
    /// 两侧 socket 都 `connect()` 对方，刻意复刻产品里「同一个 4 元组」的形态；
    /// 主控侧必须已连接，否则 `quic_direct_attempt` 的 `socket.peer_addr()?` 会返回
    /// ENOTCONN —— 这也是本测试对生产前置条件的钉死。
    #[tokio::test]
    async fn quic_product_entry_loopback_success()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        install_ring_provider();
        use tokio::net::UdpSocket;

        // C1 配对身份（与既有 loopback 测试同款：同一 seed ⇒ 同一 32B pk）。
        let seed: [u8; 32] = [0x42u8; 32];
        let (pk, server_cfg) = test_identity_and_server_cfg(&seed)?;

        // 两张 loopback UDP socket，各自 connect 到对方。
        let server_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let client_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let server_addr = server_sock.local_addr()?;
        let client_addr = client_sock.local_addr()?;
        client_sock.connect(server_addr).await?;
        server_sock.connect(client_addr).await?;

        const T: u64 = 5_000;

        // 被控端：产品入口 accept（覆盖 :404）。
        let server_task = tokio::spawn(async move {
            let mut s = match super::quic_accept_attempt(server_sock, client_addr, T, server_cfg).await
            {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[prod-loopback] 被控端 quic_accept_attempt 失败: {e}");
                    return Err::<(), Box<dyn std::error::Error + Send + Sync>>(
                        format!("被控端 accept_attempt: {e}").into(),
                    );
                }
            };
            eprintln!("[prod-loopback] 被控端 accept_attempt 成功");
            let got = match s.next().await {
                Some(Ok(g)) => g,
                other => {
                    eprintln!("[prod-loopback] 被控端 next() 失败: {other:?}");
                    return Err::<(), Box<dyn std::error::Error + Send + Sync>>(
                        format!("被控端 next(): {other:?}").into(),
                    );
                }
            };
            eprintln!("[prod-loopback] 被控端收到 {} 字节", got.len());
            if got[..] != b"hello"[..] {
                return Err::<(), Box<dyn std::error::Error + Send + Sync>>(
                    format!("被控端收到意外载荷：{:?}", &got[..]).into(),
                );
            }
            if let Err(e) = s.send_raw(b"ok".to_vec()).await {
                eprintln!("[prod-loopback] 被控端 send_raw 失败: {e}");
                return Err::<(), Box<dyn std::error::Error + Send + Sync>>(
                    format!("被控端 send_raw: {e}").into(),
                );
            }
            eprintln!("[prod-loopback] 被控端已回 ok");
            // **必须让 `s` 活到这一行之后**：本测试第一次跑就踩到了这个坑 —— 服务端任务
            // 一返回、`s`（最后一个持有该 QUIC connection 的句柄）被 drop，quinn 立刻以
            // `ApplicationClose { error_code: 0, reason: b"" }` 关闭连接，主控端的
            // `next()` 于是拿到 ConnectionLost 而不是刚发出去的 "ok"。
            // 所以这里再收一帧 "bye" 作为「主控端已读到 ok」的确认，用它把两端的存活
            // 顺序钉成确定性（不依赖 sleep）。
            let bye = match s.next().await {
                Some(Ok(b)) => b,
                other => {
                    eprintln!("[prod-loopback] 被控端第二帧失败: {other:?}");
                    return Err::<(), Box<dyn std::error::Error + Send + Sync>>(
                        format!("被控端第二帧: {other:?}").into(),
                    );
                }
            };
            if bye[..] != b"bye"[..] {
                return Err::<(), Box<dyn std::error::Error + Send + Sync>>(
                    format!("被控端第二帧载荷意外：{:?}", &bye[..]).into(),
                );
            }
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        });

        // 主控端：产品入口 dial（覆盖 :318 → :357）。
        let mut c = match super::quic_direct_attempt(client_sock, &pk, T).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[prod-loopback] 主控端 quic_direct_attempt 失败: {e}");
                return Err(format!("主控端 direct_attempt: {e}").into());
            }
        };
        eprintln!("[prod-loopback] 主控端 direct_attempt 成功（握手完成）");
        if let Err(e) = c.send_raw(b"hello".to_vec()).await {
            eprintln!("[prod-loopback] 主控端 send_raw 失败: {e}");
            return Err(format!("主控端 send_raw: {e}").into());
        }
        let ack = match c.next().await {
            Some(Ok(a)) => a,
            other => {
                eprintln!("[prod-loopback] 主控端 next() 失败: {other:?}");
                return Err(format!("主控端 next(): {other:?}").into());
            }
        };
        eprintln!("[prod-loopback] 主控端收到 {:?}", &ack[..]);
        assert_eq!(&ack[..], b"ok", "主控端应收到被控端的 2B ok");
        // 回一帧确认，让被控端可以安全地结束任务；此后主控端**仍持有 c**，因此连接不会
        // 在断言完成前被关闭（与上面那段注释对称）。
        c.send_raw(b"bye".to_vec()).await?;
        server_task.await??;
        drop(c);
        Ok(())
    }

    /// R1（task-73）负路：产品入口在**对端静默**（socket 已 bind 但无人响应 QUIC）时
    /// 必须**如实报超时**，而不是挂死或伪造成功。
    ///
    /// 对端故意选一张**只 bind、不 connect、不读**的 socket：UDP 无 ICMP 端口不可达，
    /// 因此 quinn 只能靠自身 PTO 重传，最终由 `quic_direct_attempt` 的
    /// `connect_timeout_ms` 兜底（`quic_transport.rs:345-352`）。
    #[tokio::test]
    async fn quic_product_entry_dial_timeout_is_reported()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        install_ring_provider();
        use tokio::net::UdpSocket;

        let seed: [u8; 32] = [0x42u8; 32];
        let (pk, _server_cfg) = test_identity_and_server_cfg(&seed)?;

        // 静默对端：只占住一个 127.0.0.1 端口，不读、不应答。
        let _silent = UdpSocket::bind("127.0.0.1:0").await?;
        let silent_addr = _silent.local_addr()?;

        let client_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        client_sock.connect(silent_addr).await?;

        const T_SHORT: u64 = 700;
        let err = super::quic_direct_attempt(client_sock, &pk, T_SHORT)
            .await
            .err()
            .ok_or("静默对端下 quic_direct_attempt 竟然返回 Ok —— 这是伪成功")?;
        let msg = format!("{err}");
        assert!(
            msg.contains("timeout"),
            "负路应如实报超时，实际错误：{msg}"
        );
        Ok(())
    }

    /// R1（task-73）负路：**产品入口**的 RPK 信任锚必须 fail-closed。
    ///
    /// 客户端 `NervRpkVerifier` 配错 pk（`[1u8; 32]`）⇒ 服务端证书验签失败 ⇒
    /// `quic_direct_attempt` 必须返回 `Err`（而不是握手成功或挂到超时）。
    /// `quic_c1_rpk_handshake_loopback` 只在 quinn 自带 Endpoint 上验过这一点，
    /// 本测试把它钉到产品入口 + `TokioAsyncUdpSocket` 这条真实路径上。
    #[tokio::test]
    async fn quic_product_entry_wrong_pk_fails_closed()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        install_ring_provider();
        use tokio::net::UdpSocket;

        let seed: [u8; 32] = [0x42u8; 32];
        let (_pk, server_cfg) = test_identity_and_server_cfg(&seed)?;

        let server_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let client_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let server_addr = server_sock.local_addr()?;
        let client_addr = client_sock.local_addr()?;
        client_sock.connect(server_addr).await?;
        server_sock.connect(client_addr).await?;

        const T: u64 = 5_000;

        // 被控端照常提供 RPK 服务端；它自己的握手会随之失败，这里不关心其结果。
        let server_task = tokio::spawn(async move {
            let _ = super::quic_accept_attempt(server_sock, client_addr, 1_500, server_cfg).await;
        });

        let wrong_pk: [u8; 32] = [1u8; 32];
        let res = super::quic_direct_attempt(client_sock, &wrong_pk, T).await;
        assert!(
            res.is_err(),
            "错 pk 下产品入口必须 fail-closed，实际却握手成功"
        );
        let _ = server_task.await;
        Ok(())
    }

    /// 由 32B seed 导出配对的 32B raw pk，并用产品入口 `make_quic_server_config`
    /// 构造服务端 RPK 配置（`quic_c1_rpk_handshake_loopback:1400-1409` 的提取版）。
    fn test_identity_and_server_cfg(
        seed: &[u8; 32],
    ) -> Result<([u8; 32], ServerConfig), Box<dyn std::error::Error + Send + Sync>> {
        let keypair = Ed25519KeyPair::from_seed_unchecked(seed)
            .map_err(|e| format!("Ed25519KeyPair::from_seed_unchecked: {e:?}"))?;
        let mut pk = [0u8; 32];
        pk.copy_from_slice(keypair.public_key().as_ref());
        let cfg = super::make_quic_server_config(&pk, seed)?;
        Ok((pk, cfg))
    }

    /// 两张互为对端的 loopback UDP socket。返回 `(发起端, 接受端)`。
    async fn loopback_pair() -> Result<
        (
            Arc<tokio::net::UdpSocket>,
            Arc<tokio::net::UdpSocket>,
        ),
        Box<dyn std::error::Error + Send + Sync>,
    > {
        let client_sock = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await?);
        let server_sock = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await?);
        client_sock.connect(server_sock.local_addr()?).await?;
        server_sock.connect(client_sock.local_addr()?).await?;
        Ok((client_sock, server_sock))
    }

    /// 复现接受侧 `punch_udp`：先**消费掉对端的第一个数据报**（并断言它确实是 QUIC 长首部
    /// Initial），再把**同一张** socket 交给产品 accept 入口 —— 顺序与
    /// `rendezvous_mediator.rs:1487` 一致（`punch_udp` 返回后 `quic_accept_attempt` 才建 endpoint）。
    /// 因此被消费的那个 Initial 永远不会到达 quinn，只能靠发起侧 PTO 重传。
    fn spawn_punch_eats_first_initial(
        server_sock: Arc<tokio::net::UdpSocket>,
        client_addr: std::net::SocketAddr,
        server_cfg: ServerConfig,
    ) -> tokio::task::JoinHandle<Result<(), Box<dyn std::error::Error + Send + Sync>>> {
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let n = server_sock.recv(&mut buf).await?;
            if !super::looks_like_quic_long_header(&buf[..n]) {
                return Err(format!("被消费的首包不是 QUIC 长首部：{n} 字节").into());
            }
            // 接受侧握手本身是否成功与本测试无关（发起侧可能已按预算超时），只要它不挂死。
            let _ = super::quic_accept_attempt(server_sock, client_addr, 3_000, server_cfg).await;
            Ok(())
        })
    }

    /// 产品 client 配置，但把 `initial_rtt` 换成给定值 —— 用于在同一套 socket/endpoint 栈上
    /// 对比 quinn 默认值与修复值。
    fn client_config_with_initial_rtt(
        pk: &[u8; 32],
        initial_rtt: std::time::Duration,
    ) -> Result<ClientConfig, Box<dyn std::error::Error + Send + Sync>> {
        let mut cfg = super::make_quic_client_config(pk)?;
        let mut transport = quinn::TransportConfig::default();
        transport.initial_rtt(initial_rtt);
        cfg.transport_config(Arc::new(transport));
        Ok(cfg)
    }

    /// 复刻 `quic_direct_attempt:327-358` 的拨号序列，唯一差别是允许注入 client 配置。
    async fn dial_with_config(
        socket: Arc<tokio::net::UdpSocket>,
        client_cfg: ClientConfig,
        budget_ms: u64,
    ) -> Result<hbb_common::tcp::FramedStream, String> {
        let peer = socket.peer_addr().map_err(|e| format!("peer_addr: {e}"))?;
        let mut endpoint = quinn::Endpoint::new_with_abstract_socket(
            quinn::EndpointConfig::default(),
            None,
            Arc::new(super::TokioAsyncUdpSocket {
                socket: socket.clone(),
            }),
            quinn::default_runtime().ok_or("no async runtime available for QUIC")?,
        )
        .map_err(|e| format!("Endpoint::new_with_abstract_socket: {e}"))?;
        endpoint.set_default_client_config(client_cfg);
        let sn = quinn::rustls::pki_types::ServerName::IpAddress(peer.ip().into());
        let connecting = endpoint
            .connect(peer, &sn.to_str())
            .map_err(|e| format!("Endpoint::connect: {e}"))?;
        let conn = tokio::time::timeout(std::time::Duration::from_millis(budget_ms), connecting)
            .await
            .map_err(|_| format!("dial timeout after {budget_ms}ms"))?
            .map_err(|e| format!("handshake: {e}"))?;
        let (send, recv) = conn.open_bi().await.map_err(|e| format!("open_bi: {e}"))?;
        Ok(super::quic_into_framed_stream(recv, send, peer))
    }

    /// 跑一整轮「接受侧第一个 Initial 被吃掉」的场景，返回 `(是否在预算内握手成功, 实测毫秒)`。
    async fn eaten_initial_arm(
        pk: &[u8; 32],
        initial_rtt: std::time::Duration,
        budget_ms: u64,
    ) -> Result<(bool, u128), Box<dyn std::error::Error + Send + Sync>> {
        let seed: [u8; 32] = [0x42u8; 32];
        let (_, server_cfg) = test_identity_and_server_cfg(&seed)?;
        let (client_sock, server_sock) = loopback_pair().await?;
        let client_addr = client_sock.local_addr()?;
        let server_task = spawn_punch_eats_first_initial(server_sock, client_addr, server_cfg);

        let cfg = client_config_with_initial_rtt(pk, initial_rtt)?;
        let t0 = std::time::Instant::now();
        let res = dial_with_config(client_sock, cfg, budget_ms).await;
        let elapsed_ms = t0.elapsed().as_millis();
        let ok = res.is_ok();
        drop(res);
        server_task.await??;
        Ok((ok, elapsed_ms))
    }

    /// task-6：接受侧第一个 QUIC Initial 被 `punch_udp` 消费后，发起侧必须等一次 PTO 重传。
    /// quinn 默认 `initial_rtt = 333ms` ⇒ 首次 PTO = 999ms，已经吃满 `client.rs` 给局域网
    /// 路径的全部预算（`connect_timeout` = `const MIN` = 1000ms）。两条断言中的数字都是实测：
    /// 默认值在 1000ms 预算内必须失败、在放宽到 5000ms 后必须成功（其耗时就是被 PTO 推后的量），
    /// 修复值 `QUIC_INITIAL_RTT` 在 1000ms 预算内必须成功。
    #[tokio::test]
    async fn quic_eaten_first_initial_rtt_budget() -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    {
        install_ring_provider();
        let seed: [u8; 32] = [0x42u8; 32];
        let (pk, _) = test_identity_and_server_cfg(&seed)?;

        /// `client.rs` 的 `const MIN`：局域网 / 双 SYMMETRIC 路径给 QUIC 的全部预算。
        const PRODUCT_MIN_MS: u64 = 1_000;
        /// quinn 的默认 `initial_rtt`（`quinn-proto-0.11.14/src/config/transport.rs:373`）。
        const QUINN_DEFAULT_INITIAL_RTT_MS: u64 = 333;

        let quinn_default = std::time::Duration::from_millis(QUINN_DEFAULT_INITIAL_RTT_MS);

        let (ok_default, ms_default) =
            eaten_initial_arm(&pk, quinn_default, PRODUCT_MIN_MS).await?;
        assert!(
            !ok_default,
            "默认 initial_rtt={QUINN_DEFAULT_INITIAL_RTT_MS}ms 竟在 {PRODUCT_MIN_MS}ms 预算内握手成功（耗时 {ms_default}ms）——本测试复现缺陷的前提不成立"
        );

        let (ok_default_generous, ms_default_generous) =
            eaten_initial_arm(&pk, quinn_default, 5_000).await?;
        assert!(
            ok_default_generous,
            "默认 initial_rtt 在 5000ms 预算内仍未握手成功，缺陷模型不成立"
        );

        let (ok_fixed, ms_fixed) =
            eaten_initial_arm(&pk, super::QUIC_INITIAL_RTT, PRODUCT_MIN_MS).await?;
        assert!(
            ok_fixed,
            "修复值 initial_rtt={:?} 在 {PRODUCT_MIN_MS}ms 预算内仍未握手成功（耗时 {ms_fixed}ms）",
            super::QUIC_INITIAL_RTT
        );

        eprintln!(
            "[rtt-fix] initial_rtt={QUINN_DEFAULT_INITIAL_RTT_MS}ms 预算={PRODUCT_MIN_MS}ms: 结果={} 耗时={ms_default}ms",
            if ok_default { "成功" } else { "超时" }
        );
        eprintln!(
            "[rtt-fix] initial_rtt={QUINN_DEFAULT_INITIAL_RTT_MS}ms 预算=5000ms: 结果={} 耗时={ms_default_generous}ms（被首个 PTO 推后）",
            if ok_default_generous { "成功" } else { "超时" }
        );
        eprintln!(
            "[rtt-fix] initial_rtt={:?} 预算={PRODUCT_MIN_MS}ms: 结果={} 耗时={ms_fixed}ms",
            super::QUIC_INITIAL_RTT,
            if ok_fixed { "成功" } else { "超时" }
        );
        assert!(
            ms_fixed < ms_default_generous,
            "修复后的握手耗时（{ms_fixed}ms）必须小于修复前（{ms_default_generous}ms）"
        );
        Ok(())
    }

    /// task-6 产品入口回归：`quic_direct_attempt(..., 1000)`（= `client.rs` 的 `const MIN`）
    /// 在接受侧吃掉第一个 Initial 后仍必须**真实成功**（含 `open_bi`）。字节层对账由既有的
    /// `quic_product_entry_loopback_success` 覆盖，这里只钉「预算内能否拿到可用连接」。
    #[tokio::test]
    async fn quic_product_entry_survives_eaten_first_initial()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        install_ring_provider();
        let seed: [u8; 32] = [0x42u8; 32];
        let (pk, server_cfg) = test_identity_and_server_cfg(&seed)?;

        let (client_sock, server_sock) = loopback_pair().await?;
        let client_addr = client_sock.local_addr()?;
        let server_task = spawn_punch_eats_first_initial(server_sock, client_addr, server_cfg);

        let t0 = std::time::Instant::now();
        let stream = super::quic_direct_attempt(client_sock, &pk, 1_000)
            .await
            .map_err(|e| {
                format!(
                    "修复后产品入口仍在 1000ms 预算内失败（耗时 {}ms）：{e}",
                    t0.elapsed().as_millis()
                )
            })?;
        let ms = t0.elapsed().as_millis();
        eprintln!("[rtt-fix] 产品入口 quic_direct_attempt(1000ms) 握手成功，实测耗时={ms}ms");
        assert!(ms < 1_000);
        drop(stream);
        server_task.await??;
        Ok(())
    }

    /// R4-3（task-15）F-R1：监听侧判别器必须对 KCP SYN 零假阳性。
    ///
    /// 旧判据（`bit7 != 0 && len > 8 && datagram[1..5] != [0;4]`）对 KCP SYN 有约 50% 假阳性，
    /// 会把 KCP 直连误导入 QUIC 并杀掉连接。本测试把收紧后的判据钉死。
    #[test]
    fn quic_discriminator_rejects_kcp_syn() {
        // `kcp-sys` 的线格式（kcp-sys/src/packet_def.rs:23-30，无前缀）：
        // conv(LE u32) ‖ src_session_id(LE u32) ‖ dst_session_id(LE u32) ‖ flag ‖ rsv = 14 字节。
        // `KcpStream::connect`（src/kcp_stream.rs:117）把 src_session_id 硬编码为 0。
        fn kcp_syn(conv: u32, src_session_id: u32) -> [u8; 14] {
            let mut b = [0u8; 14];
            b[..4].copy_from_slice(&conv.to_le_bytes());
            b[4..8].copy_from_slice(&src_session_id.to_le_bytes());
            b
        }
        // Lead 复核给出的反例：conv = 0x0000_0180 ⇒ 首字节 0x80、datagram[1..5] = [1,0,0,0]，
        // 旧判据判为 QUIC，新判据必须否。
        assert!(
            !super::looks_like_quic_long_header(&kcp_syn(0x0000_0180, 0)),
            "conv = 0x00000180 的 KCP SYN 不得被判为 QUIC"
        );
        // 覆盖 bit7/bit6 的四种组合与 conv 边界（1/2 的 bit7 概率由此穷举式钉住）。
        for conv in [
            0x0000_0000,
            0x0000_0001,
            0x0000_0040,
            0x0000_0080,
            0x0000_00c0,
            0x0000_00ff,
            0x0000_0100,
            0x0000_0101,
            0x0000_0180,
            0x0000_01c0,
            0x0000_ffff,
            0x0001_0000,
            0x8000_0000,
            0xffff_ffff,
        ] {
            let p = kcp_syn(conv, 0);
            assert!(
                !super::looks_like_quic_long_header(&p),
                "conv = {conv:#010x} 的 KCP SYN 被误判为 QUIC：{p:?}"
            );
        }
        // 收紧后唯一的假阳性形状 = 「conv < 256（版本字段前 3 字节为 0）」**且**
        // 「src_session_id 低字节 == 1」。后半条本仓不可达（src/kcp_stream.rs:117 恒传 0）——
        // 下面三条把这个「结构上不可能」钉成可回归的断言。
        assert!(super::looks_like_quic_long_header(&kcp_syn(0x0000_00c0, 1)));
        assert!(!super::looks_like_quic_long_header(&kcp_syn(0x0000_00c0, 0)));
        assert!(!super::looks_like_quic_long_header(&kcp_syn(0x0000_01c0, 1)));
    }

    /// R4-3（task-15）F-R1：真 QUIC v1 Initial 必须命中。
    #[test]
    fn quic_discriminator_accepts_quic_v1_initial() {
        // 0xC3 = LONG_HEADER_FORM | FIXED_BIT | (pn_len − 1 = 3)；版本 = v1；
        // 随后 DCID 长度 8 / DCID / SCID 长度 0 / token 长度 0 / length / packet number。
        // 客户端首包的 fixed bit 不会被 grease 掉（quinn 只在 peer_params.grease_quic_bit
        // 为真时翻转，而首包构造时 peer 参数还是默认的 false）。
        fn initial(b0: u8, version: [u8; 4]) -> Vec<u8> {
            let mut p = vec![b0, version[0], version[1], version[2], version[3], 0x08];
            p.extend_from_slice(&[0xAA; 8]); // DCID
            p.push(0x00); // SCID 长度
            p.push(0x00); // token 长度
            p.extend_from_slice(&[0x44, 0x9E]); // length
            p.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // packet number
            p.resize(1200, 0); // RFC 9000：客户端 Initial 必须填充到 >= 1200 字节
            p
        }
        // 首字节的四种 packet-number 长度都应命中。
        for b0 in [0xC0u8, 0xC1, 0xC2, 0xC3] {
            let p = initial(b0, [0x00, 0x00, 0x00, 0x01]);
            assert!(
                super::looks_like_quic_long_header(&p),
                "首字节 {b0:#04x} 的 QUIC v1 Initial 必须命中"
            );
        }
    }

    /// R4-3（task-15）F-R1：打洞探针、空包、过短的包、非 v1 版本都不得命中。
    #[test]
    fn quic_discriminator_rejects_non_quic_datagrams() {
        // src/common.rs:2785-2787：PUNCH_PROBE = *b"RDP?"、PUNCH_ACK = *b"RDP!"，共 12 字节，
        // 其余 8 字节是随机 tid。首字节 'R' = 0x52，bit7 = 0 ⇒ 恒不命中。
        for tag in [b"RDP?", b"RDP!"] {
            let mut p = [0u8; 12];
            p[..4].copy_from_slice(tag);
            p[4..].copy_from_slice(&0x0000_0180u64.to_le_bytes());
            assert!(
                !super::looks_like_quic_long_header(&p),
                "打洞探针 {:?} 不得被判为 QUIC",
                std::str::from_utf8(tag).unwrap_or("?")
            );
        }
        // 空包与长度 < 5 的包：版本字段读不出来 ⇒ 恒不命中。
        assert!(!super::looks_like_quic_long_header(&[]));
        for n in 1..5usize {
            assert!(
                !super::looks_like_quic_long_header(&vec![0xC0u8; n]),
                "{n} 字节的包不得被判为 QUIC（版本字段缺失）"
            );
        }
        // 版本 0（Version Negotiation，RFC 9000 §17.2.1）与其它版本：本模块客户端恒发 v1，
        // 且首包不会是 Version Negotiation，故一律不命中。
        fn long_header(version: [u8; 4]) -> Vec<u8> {
            let mut p = vec![0xC0u8, version[0], version[1], version[2], version[3], 0x08];
            p.extend_from_slice(&[0xBB; 8]);
            p.resize(1200, 0);
            p
        }
        assert!(!super::looks_like_quic_long_header(&long_header([
            0x00, 0x00, 0x00, 0x00
        ])));
        for v in [0xff00_001du32, 0xff00_001eu32, 0x0000_0002, 0xffff_ffff] {
            let p = long_header(v.to_be_bytes());
            assert!(
                !super::looks_like_quic_long_header(&p),
                "版本 {v:#010x} 不得被判为 QUIC v1"
            );
        }
    }

    #[test]
    fn quic_discriminator_known_residual_shape_is_pinned() {
        // 已知残余形状（本仓不可达，但行为必须被钉住 —— 见 analysis/network/quic-server-wiring.md
        // §4.1.1「残余形状」）：判据只看首字节的两个高位与版本字段，**不要求 QUIC Initial 的最小长度**。
        // 因此一个恰好 5 字节的 `C0 00 00 00 01` 会被判为 QUIC。这是刻意保留的：
        //   - 监听 socket（src/rendezvous_mediator.rs:1486）已 connect 到对端，对端只会发
        //     12 字节打洞探针（src/common.rs:2785-2787）、14 字节 KCP SYN（src/kcp_stream.rs:117）
        //     或 >= 1200 字节 QUIC Initial ⇒ 该形状不可达。
        //   - 若加 `datagram.len() >= 1200`（RFC 9000 §14.1 的填充是**客户端义务**），会引入一个
        //     **新的误判类**：漏判 ⇒ 退回 KCP ⇒ 连接失败，比「多等一个 PTO」更差的失败模式。
        //     故记为 round-5 候选，不在 v1 实施（Lead 裁定）。
        let residual = [0xC0u8, 0x00, 0x00, 0x00, 0x01];
        assert!(
            super::looks_like_quic_long_header(&residual),
            "已知残余形状：5 字节 `C0 00 00 00 01` 当前判为 QUIC。若此处变红，说明判据被改动，\
             必须同步 analysis/network/quic-server-wiring.md §4.1.1 的残余形状条目"
        );
    }
}
