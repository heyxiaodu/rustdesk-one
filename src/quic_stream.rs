//! QUIC transport, mirroring `kcp_stream.rs`.
//!
//! `punch_udp` has already opened the NAT mapping on a *connected* UDP socket by the time
//! this module takes it over. quinn drives that socket directly, and the single
//! bidirectional stream carrying the session is wrapped into
//! `Stream::Tcp(FramedStream(..))`. The connection layer above therefore cannot tell QUIC
//! from TCP or KCP, which is what keeps this feature out of `hbb_common`.
//!
//! TLS is present because QUIC requires it, not because it carries RustDesk's identity.
//! The certificate is self-signed and accepted unconditionally; peer authentication stays
//! where it is for every other transport -- the `SignedId` signature check and the
//! secretbox exchange that `Stream::Tcp`'s `set_key` path performs above this layer. A QUIC
//! session is therefore exactly as authenticated as a plain TCP one, and encrypted at least
//! as well.

use hbb_common::{
    // Imported anonymously: `Context` is also `std::task::Context`, which every poll method
    // below names.
    anyhow::{anyhow, Context as _},
    bytes_codec::BytesCodec,
    log,
    tcp::{DynTcpStream, FramedStream},
    tokio::{
        self,
        io::{AsyncRead, AsyncWrite, ReadBuf},
        net::UdpSocket,
    },
    tokio_util, ResultType, Stream,
};
use quinn::{
    crypto::rustls::{QuicClientConfig, QuicServerConfig},
    rustls,
    udp::{RecvMeta, Transmit},
    AsyncUdpSocket, ClientConfig, Endpoint, EndpointConfig, IdleTimeout, ServerConfig,
    TransportConfig, UdpPoller, VarInt,
};
use std::{
    fmt, io,
    io::IoSliceMut,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{ready, Context, Poll},
    time::Duration,
};

/// ALPN, so a QUIC endpoint that is not this transport declines the handshake instead of
/// reaching the session layer.
const ALPN: &[u8] = b"rustdesk-quic";

/// SNI for the same handshake. No CA and no name are checked (see `AcceptAnyServerCert`),
/// so its only job is to be a syntactically valid server name.
const SERVER_NAME: &str = "quic.rustdesk.local";

/// Largest UDP payload quinn may put on the wire, and the largest it accepts.
///
/// **A deployment-environment constant, not a universal one.** It is measured against the
/// fixed, homogeneous target this ships to: two endpoints behind the same path where 1447 B
/// passes unfragmented and 1469 B is dropped outright. Changing either endpoint, or the path
/// between them, invalidates it.
///
/// A packet of this size is 1447 + 8 (UDP) + 20 (IPv4) = 1475 B, inside a 1500 B Ethernet
/// MTU, so it does not rely on IP fragmentation -- which this path drops rather than
/// delivers. QUIC's 1200 B floor is the only size safe on *any* path and costs about a fifth
/// of the payload, which is why this is not simply set to it.
///
/// **Before this transport is used over paths this project does not control** -- the planned
/// QUIC relay reaching arbitrary public clients is the case that matters -- re-evaluate this
/// and expect to come down to 1200, or to re-enable MTU discovery, rather than carrying the
/// current number over.
const MAX_UDP_PAYLOAD_SIZE: u16 = 1447;

/// Idle timeout advertised to the peer. A session that goes silent is reaped by QUIC on
/// both ends rather than left to the connection layer's own 30s read timeout.
const IDLE_TIMEOUT_SECS: u64 = 30;

/// Keeps NAT mappings warm between messages, well inside the idle timeout.
const KEEP_ALIVE_SECS: u64 = 5;

/// How long the QUIC handshake gets when the legacy KCP attempt may still follow on the
/// same socket.
///
/// The punch has already proved the path, so a peer that speaks QUIC answers in about one
/// round trip, while a peer that does not never answers at all and must not consume the
/// caller's whole connect budget.
///
/// **Known trade-off, decided deliberately (see `docs/QUIC-ARCHITECTURE.md` §14).** This
/// project prefers QUIC, so `Mode::Auto` -- the default -- probes for it first. A peer that
/// does not speak QUIC therefore reaches its direct KCP connection about 2000 ms later than
/// it would have without this module. Which transport is used does not change, and the race
/// is not lost to the relay: 2000 ms is kept below `Client::RELAY_FALLBACK_DELAY_MS`
/// (2500 ms), the window a relayed result is held for, so the slower direct attempt still
/// wins. `transport-mode = "tcp"` restores the original timing exactly.
const PROBE_TIMEOUT_MS: u64 = 2_000;

/// Application close code. QUIC reserves none of these, so any value works; the reason
/// string is what a peer logs.
const CLOSE_CODE: u32 = 0;

/// The transport label, reported the same way "TCP" / "UDP" / "Relay" / "WebRTC" are, so the
/// UI and the connect log name what a session actually used.
pub const TYP: &str = "QUIC";

/// Which transport the direct UDP punch path may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// QUIC when the peer answers, otherwise the legacy KCP attempt on the same socket.
    Auto,
    /// QUIC only. A failure is reported instead of being masked by a fallback, so an
    /// operator can prove which transport a session actually used.
    Quic,
    /// Never QUIC. Byte-for-byte the behaviour before this module existed.
    Tcp,
}

/// Read `transport-mode`. Unset, empty or unrecognised means `Auto`, so a build with the
/// `quic` feature on behaves as documented without any configuration.
pub fn mode() -> Mode {
    parse_mode(&hbb_common::config::LocalConfig::get_option(
        base::config::keys::OPTION_TRANSPORT_MODE,
    ))
}

fn parse_mode(raw: &str) -> Mode {
    match raw.trim().to_ascii_lowercase().as_str() {
        "quic" => Mode::Quic,
        "tcp" => Mode::Tcp,
        "auto" | "" => Mode::Auto,
        other => {
            log::warn!("[QUIC] unknown transport-mode {other:?}, using auto");
            Mode::Auto
        }
    }
}

/// Whether the direct UDP punch path should attempt QUIC at all.
pub fn enabled() -> bool {
    mode() != Mode::Tcp
}

/// The window one QUIC attempt gets.
///
/// In `Auto` the legacy KCP attempt may still follow on the same socket, so the handshake
/// is bounded by `PROBE_TIMEOUT_MS`: the punch has already proved the path, so a peer that
/// speaks QUIC answers in about one round trip, while a peer that does not never answers at
/// all and must not consume the caller's whole connect budget. In `Quic` there is nothing
/// to fall back to, so the caller's full budget applies.
fn budget_ms(mode: Mode, timeout_ms: u64) -> u64 {
    if mode == Mode::Auto {
        timeout_ms.min(PROBE_TIMEOUT_MS)
    } else {
        timeout_ms
    }
}

/// Reject a transport that `Mode::Quic` did not ask for.
///
/// A no-op in every other mode, and for QUIC itself, so both race sites stay a single
/// guarded line and the legacy paths keep their exact behaviour. An empty label means the
/// attempt already failed, and its own error is the useful one to report.
pub fn require(typ: &str) -> ResultType<()> {
    if mode() == Mode::Quic && !typ.is_empty() && typ != TYP {
        return Err(anyhow!(
            "transport-mode is \"quic\" but the connection fell back to {typ:?}"
        ));
    }
    Ok(())
}

/// The peer address of a socket `punch_udp` has already connected.
fn peer_addr_of(socket: &UdpSocket) -> ResultType<SocketAddr> {
    socket
        .peer_addr()
        .context("the punched UDP socket is not connected to the peer")
}

/// A UDP socket RustDesk already punched, presented to quinn via `AsyncUdpSocket`.
///
/// `Endpoint::new` would want to own a `std::net::UdpSocket`, which this socket cannot be:
/// the NAT-test task, the IPv6 cache and the punch itself all hold `Arc` clones of it, and
/// taking it over is what preserves the mapping the punch just opened. quinn's abstract
/// socket exists for exactly this case.
#[derive(Debug)]
struct PunchedSocket {
    socket: Arc<UdpSocket>,
}

/// Write-readiness for `PunchedSocket`. tokio keeps one waker per socket for send
/// readiness, and quinn asks for a fresh poller per interested task, so each gets its own
/// handle onto the same registration.
#[derive(Debug)]
struct SendReady {
    socket: Arc<UdpSocket>,
}

impl UdpPoller for SendReady {
    fn poll_writable(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.socket.poll_send_ready(cx)
    }
}

impl AsyncUdpSocket for PunchedSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(SendReady {
            socket: self.socket.clone(),
        })
    }

    fn try_send(&self, transmit: &Transmit<'_>) -> io::Result<()> {
        // The socket is connected, so `destination` is implied and is not honoured here.
        // One datagram per call: `max_transmit_segments` is left at its default of 1, so
        // quinn has no reason to coalesce -- but a `segment_size` shorter than the buffer
        // would mean exactly that, and sending it whole would put a multi-datagram batch on
        // the wire as one.
        if let Some(segment_size) = transmit.segment_size {
            if segment_size != transmit.contents.len() {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "coalesced UDP transmits are not supported",
                ));
            }
        }
        match self.socket.try_send(transmit.contents) {
            Ok(n) if n == transmit.contents.len() => Ok(()),
            Ok(n) => Err(io::Error::new(
                io::ErrorKind::WriteZero,
                format!("short UDP send: {n} of {}", transmit.contents.len()),
            )),
            Err(e) => Err(e),
        }
    }

    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let (Some(buf), Some(meta)) = (bufs.first_mut(), meta.first_mut()) else {
            return Poll::Ready(Ok(0));
        };
        let mut read = ReadBuf::new(&mut buf[..]);
        ready!(self.socket.poll_recv(cx, &mut read))?;
        let len = read.filled().len();
        if len == 0 {
            // An empty datagram carries nothing QUIC can parse; report "no datagrams"
            // rather than a zero-length one, which the endpoint would treat as a frame.
            return Poll::Ready(Ok(0));
        }
        // A connected socket has exactly one peer, and QUIC only ever talks to it.
        let addr = match self.socket.peer_addr() {
            Ok(addr) => addr,
            Err(e) => return Poll::Ready(Err(e)),
        };
        *meta = RecvMeta {
            addr,
            len,
            stride: len,
            ecn: None,
            dst_ip: None,
        };
        Poll::Ready(Ok(1))
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}

/// Treats any certificate as valid.
///
/// This is not the session's authentication, and nothing here claims it is: RustDesk proves
/// the peer's identity above this transport, by checking a signature made with the peer's
/// private key against the public key the rendezvous server signed, and by sealing the
/// session key to the ephemeral key inside that signed payload. A machine in the middle
/// that terminated this TLS could not forge either, so it would still fail the handshake in
/// `secure_connection` / `identity_handshake`. There is no CA and no hostname to check in
/// the first place: both endpoints are already keyed to each other by ID.
#[derive(Debug)]
struct AcceptAnyServerCert(Arc<rustls::crypto::CryptoProvider>);

impl AcceptAnyServerCert {
    fn new() -> Arc<Self> {
        Arc::new(Self(Arc::new(rustls::crypto::ring::default_provider())))
    }
}

impl rustls::client::danger::ServerCertVerifier for AcceptAnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Transport parameters shared by both ends.
///
/// The payload clamp lives on `EndpointConfig` (what we accept) and `initial_mtu` (what we
/// send); `TransportConfig` has no `max_udp_payload_size` of its own.
fn transport_config() -> ResultType<TransportConfig> {
    let mut config = TransportConfig::default();
    // What we send. `min_mtu` is left at QUIC's 1200 B floor: it is the size the path is
    // assumed to guarantee, and only MTU discovery reads it, which is off below.
    config.initial_mtu(MAX_UDP_PAYLOAD_SIZE);
    // The measured path drops oversized datagrams silently rather than signalling, so a
    // probe learns nothing it can trust; a fixed, never-fragmenting packet size is worth
    // more here than the last few percent of payload.
    config.mtu_discovery_config(None);
    config.keep_alive_interval(Some(Duration::from_secs(KEEP_ALIVE_SECS)));
    config.max_idle_timeout(Some(
        IdleTimeout::try_from(Duration::from_secs(IDLE_TIMEOUT_SECS))
            .map_err(|e| anyhow!("QUIC idle timeout rejected: {e}"))?,
    ));
    Ok(config)
}

/// What this endpoint accepts from a peer. quinn's default is 1472, above what the path
/// carries, so an oversized datagram would be taken in and then dropped by the network on
/// the way out of the peer instead.
fn endpoint_config() -> ResultType<EndpointConfig> {
    let mut config = EndpointConfig::default();
    config
        .max_udp_payload_size(MAX_UDP_PAYLOAD_SIZE)
        .map_err(|e| anyhow!("QUIC max_udp_payload_size rejected: {e}"))?;
    Ok(config)
}

fn runtime() -> ResultType<Arc<dyn quinn::Runtime>> {
    quinn::default_runtime().ok_or_else(|| anyhow!("no QUIC-compatible async runtime available"))
}

fn client_config(transport: Arc<TransportConfig>) -> ResultType<ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| anyhow!("QUIC client TLS setup failed: {e}"))?
        .dangerous()
        .with_custom_certificate_verifier(AcceptAnyServerCert::new())
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let mut config = ClientConfig::new(Arc::new(
        QuicClientConfig::try_from(tls).map_err(|e| anyhow!("QUIC client crypto rejected: {e}"))?,
    ));
    config.transport_config(transport);
    Ok(config)
}

fn server_config(transport: Arc<TransportConfig>) -> ResultType<ServerConfig> {
    let cert = rcgen::generate_simple_self_signed(vec![SERVER_NAME.to_owned()])
        .map_err(|e| anyhow!("failed to generate the QUIC certificate: {e}"))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| anyhow!("QUIC server TLS setup failed: {e}"))?
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.cert.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()).into(),
        )
        .map_err(|e| anyhow!("QUIC server certificate rejected: {e}"))?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let mut config = ServerConfig::with_crypto(Arc::new(
        QuicServerConfig::try_from(tls).map_err(|e| anyhow!("QUIC server crypto rejected: {e}"))?,
    ));
    config.transport_config(transport);
    Ok(config)
}

/// Owns everything the session's QUIC transport needs to stay alive.
///
/// The endpoint is held here, not left to the caller, so the connection's driver task --
/// which owns the socket -- lives exactly as long as the `Stream` that carries it. This is
/// the same shape as the KCP path, where the `KcpStream` handle is kept by the tuple that
/// owns the `Stream`; here it is one object, so no caller can forget it.
struct QuicBiStream {
    endpoint: Endpoint,
    conn: quinn::Connection,
    send: quinn::SendStream,
    recv: quinn::RecvStream,
}

impl fmt::Debug for QuicBiStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QuicBiStream")
            .field("remote", &self.conn.remote_address())
            .finish()
    }
}

impl AsyncRead for QuicBiStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        AsyncRead::poll_read(Pin::new(&mut self.recv), cx, buf)
    }
}

impl AsyncWrite for QuicBiStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        AsyncWrite::poll_write(Pin::new(&mut self.send), cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_flush(Pin::new(&mut self.send), cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_shutdown(Pin::new(&mut self.send), cx)
    }
}

impl Drop for QuicBiStream {
    fn drop(&mut self) {
        // Owning the transport means ending it here, as `Stream`'s own Drop does for WebRTC.
        // Both calls only enqueue: the endpoint's driver task holds its own reference and
        // keeps draining the socket after this handle goes, so the close frames still reach
        // the peer. Nothing may be awaited in Drop -- callers reach it from a `select!` arm
        // or from a future the UI abandons.
        self.conn.close(VarInt::from(CLOSE_CODE), b"session ended");
        self.endpoint.close(VarInt::from(CLOSE_CODE), b"");
    }
}

/// Wrap the session's QUIC stream for the connection layer.
fn create_framed(stream: QuicBiStream, local_addr: SocketAddr) -> Stream {
    Stream::Tcp(FramedStream(
        tokio_util::codec::Framed::new(DynTcpStream(Box::new(stream)), BytesCodec::new()),
        local_addr,
        None,
        0,
    ))
}

fn build_endpoint(
    socket: Arc<UdpSocket>,
    server_config: Option<ServerConfig>,
) -> ResultType<Endpoint> {
    let local_addr = socket
        .local_addr()
        .context("failed to read the punched socket's address")?;
    let endpoint = Endpoint::new_with_abstract_socket(
        endpoint_config()?,
        server_config,
        Arc::new(PunchedSocket { socket }),
        runtime()?,
    )
    .with_context(|| format!("failed to start a QUIC endpoint on {local_addr}"))?;
    log::debug!("[QUIC] endpoint initialized on {local_addr}");
    Ok(endpoint)
}

/// Finish a handshake and hand back the transport as a `Stream`.
async fn adopt(conn: quinn::Connection, endpoint: Endpoint, budget: u64) -> ResultType<Stream> {
    let local_addr = endpoint
        .local_addr()
        .context("failed to read the QUIC endpoint's address")?;
    let remote = conn.remote_address();
    let (send, recv) =
        match tokio::time::timeout(Duration::from_millis(budget), conn.open_bi()).await {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => {
                return Err(anyhow!("failed to open the QUIC session stream: {e}"));
            }
            Err(_) => {
                return Err(anyhow!(
                    "the QUIC session stream did not open within {budget} ms"
                ));
            }
        };
    log::info!("[QUIC] connection established with {remote}");
    Ok(create_framed(
        QuicBiStream {
            endpoint,
            conn,
            send,
            recv,
        },
        local_addr,
    ))
}

/// Dial the punched peer over QUIC.
///
/// `timeout_ms` is the caller's whole connect budget; see `budget_ms` for how much of it a
/// handshake that may still be followed by a legacy attempt is allowed to spend.
pub async fn connect(socket: Arc<UdpSocket>, timeout_ms: u64) -> ResultType<Stream> {
    let peer = peer_addr_of(&socket)?;
    let budget = budget_ms(mode(), timeout_ms);
    let mut endpoint = build_endpoint(socket, None)?;
    endpoint.set_default_client_config(client_config(Arc::new(transport_config()?))?);
    log::debug!("[QUIC] direct connection attempt to {peer}");

    let connecting = endpoint
        .connect(peer, SERVER_NAME)
        .with_context(|| format!("failed to start a QUIC handshake with {peer}"))?;
    let conn = match tokio::time::timeout(Duration::from_millis(budget), connecting).await {
        Ok(Ok(conn)) => conn,
        Ok(Err(e)) => {
            return Err(anyhow!("QUIC handshake with {peer} failed: {e}"));
        }
        Err(_) => {
            return Err(anyhow!(
                "QUIC handshake with {peer} timed out after {budget} ms"
            ));
        }
    };
    adopt(conn, endpoint, budget).await
}

/// Accept the peer's QUIC connection on the punched socket.
pub async fn accept(socket: Arc<UdpSocket>, timeout_ms: u64) -> ResultType<Stream> {
    let budget = budget_ms(mode(), timeout_ms);
    let endpoint = build_endpoint(socket, Some(server_config(Arc::new(transport_config()?))?))?;
    let incoming = match tokio::time::timeout(Duration::from_millis(budget), endpoint.accept()).await
    {
        Ok(Some(incoming)) => incoming,
        Ok(None) => return Err(anyhow!("the QUIC endpoint closed before a peer connected")),
        Err(_) => {
            return Err(anyhow!("no QUIC connection arrived within {budget} ms"));
        }
    };
    let conn = match tokio::time::timeout(Duration::from_millis(budget), incoming).await {
        Ok(Ok(conn)) => conn,
        Ok(Err(e)) => {
            return Err(anyhow!("the QUIC handshake with the controller failed: {e}"));
        }
        Err(_) => {
            return Err(anyhow!(
                "the QUIC handshake did not complete within {budget} ms"
            ));
        }
    };
    let local_addr = endpoint
        .local_addr()
        .context("failed to read the QUIC endpoint's address")?;
    let remote = conn.remote_address();
    // The controller opens exactly one bidirectional stream, and that stream is the session.
    let (send, recv) = match tokio::time::timeout(Duration::from_millis(budget), conn.accept_bi())
        .await
    {
        Ok(Ok(pair)) => pair,
        Ok(Err(e)) => {
            return Err(anyhow!("failed to accept the QUIC session stream: {e}"));
        }
        Err(_) => {
            return Err(anyhow!(
                "the controller did not open a QUIC session stream within {budget} ms"
            ));
        }
    };
    log::info!("[QUIC] connection accepted from {remote}");
    Ok(create_framed(
        QuicBiStream {
            endpoint,
            conn,
            send,
            recv,
        },
        local_addr,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_receive_clamp_is_exactly_the_measured_path_limit() {
        let config = endpoint_config().expect("endpoint config");
        assert_eq!(
            config.get_max_udp_payload_size(),
            MAX_UDP_PAYLOAD_SIZE as u64
        );
    }

    #[test]
    fn the_clamp_is_below_the_payload_the_path_drops() {
        // The measured path drops >= 1469 B. The clamp must also leave room for the UDP and
        // IP headers inside a 1500 B MTU, which is what keeps it off IP fragmentation.
        assert!(MAX_UDP_PAYLOAD_SIZE < 1469);
        assert!(MAX_UDP_PAYLOAD_SIZE as u32 + 28 <= 1500);
    }

    #[test]
    fn both_tls_configs_build_with_a_generated_certificate() {
        // Exercises the whole TLS setup: the ring provider, TLS 1.3 only, ALPN, and rcgen's
        // self-signed certificate for the server half.
        let transport = Arc::new(transport_config().expect("transport config"));
        client_config(transport.clone()).expect("client config");
        server_config(transport).expect("server config");
    }

    #[test]
    fn only_the_documented_transport_modes_are_recognised() {
        assert_eq!(parse_mode("quic"), Mode::Quic);
        assert_eq!(parse_mode("  QUIC "), Mode::Quic);
        assert_eq!(parse_mode("tcp"), Mode::Tcp);
        assert_eq!(parse_mode("Tcp"), Mode::Tcp);
        assert_eq!(parse_mode("auto"), Mode::Auto);
        // Unset, and anything unrecognised, must not disable the transport or fail a
        // connection: both mean the documented default.
        assert_eq!(parse_mode(""), Mode::Auto);
        assert_eq!(parse_mode("   "), Mode::Auto);
        assert_eq!(parse_mode("udp"), Mode::Auto);
    }

    #[test]
    fn auto_falls_back_but_quic_does_not() {
        // In Auto a legacy peer must still get its direct KCP connection, so the QUIC probe
        // is bounded by `PROBE_TIMEOUT_MS`, and that bound has to sit inside the window a
        // relayed result is held for -- otherwise the direct path loses the race outright.
        assert_eq!(budget_ms(Mode::Auto, u64::MAX), PROBE_TIMEOUT_MS);
        assert_eq!(budget_ms(Mode::Auto, 500), 500);
        assert_eq!(budget_ms(Mode::Quic, u64::MAX), u64::MAX);
        // The window must fit inside the relay preference window, or a legacy peer loses the
        // direct path to the relay while this probe runs.
        assert!(PROBE_TIMEOUT_MS < 2500);
    }
}
