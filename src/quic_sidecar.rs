//! QUIC-sidecar integration (feature `quic`).
//!
//! P3 (path 1): the QUIC data plane is carried by a separate `nervdesk-quic-sidecar`
//! binary (built out of tree, iroh 1.2.0 on a newer toolchain). This module is the thin
//! client side: discover/spawn the sidecar, speak a minimal control protocol over the
//! existing per-uid IPC transport, and borrow the sidecar's relayed QUIC session as a
//! plain byte stream. The borrowed stream is wrapped upstream into
//! `Stream::Tcp(FramedStream::from(conn, addr))` exactly like KCP and the in-process
//! QUIC transport, so the connection layer cannot tell it from TCP.
//!
//! Design: `docs/P3-CLIENT-DESIGN.md`. Security model (unchanged):
//! - the sidecar serves only the same uid (per-uid socket path + `peer_uid` check);
//! - the sidecar has no identity authority: `endpoint_id == remote_id` is all it can
//!   assert; RustDesk's own `SignedId`/secretbox checks run above this transport;
//! - the relay token is read by the sidecar from `IROH_RELAY_TOKEN` at spawn time; it
//!   is never written into this crate, configuration, logs or docs.
//!
//! This module is feature-gated: with `quic` off none of it compiles and every legacy
//! path is byte-for-byte unchanged.

use std::{
    fmt, io,
    net::SocketAddr,
    path::PathBuf,
    time::{Duration, Instant},
};

use hbb_common::{anyhow, log, ResultType, Stream};
use hbb_common::anyhow::{anyhow, Context as _};

/// The sidecar binary name, searched next to the current executable and on PATH.
const SIDECAR_NAME: &str = "nervdesk-quic-sidecar";
/// Per-uid IPC socket postfix, distinct from the main app IPC.
const SIDECAR_IPC_POSTFIX: &str = "-sidecar";
/// How long to wait for the sidecar's control socket to appear after spawn.
const SIDECAR_SPAWN_TIMEOUT: Duration = Duration::from_secs(10);
/// How long one control request may take before we treat the sidecar as stuck.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(15);

/// Control messages between RustDesk and the sidecar. Kept minimal for the MVP
/// (see P3-CLIENT-DESIGN §3.2); the data plane is a separate stream connection.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
enum Control {
    /// Idempotent; asks the sidecar to make sure its endpoint is online on the relay.
    EnsureOnline { relay_url: String, token_present: bool },
    /// Ask the sidecar to dial a peer and, on success, open a fresh data connection.
    Dial {
        remote_endpoint_id: [u8; 32],
        alpn: Vec<u8>,
    },
    /// Positive result carries the chosen path; negative carries a reason.
    DialResult {
        ok: bool,
        reason: Option<String>,
        path: Option<String>,
    },
    /// Inbound connection notification (responder side).
    Incoming {
        remote_endpoint_id: [u8; 32],
        path: Option<String>,
    },
    /// Responder's decision on an `Incoming`.
    Accept { decision: bool, reason: Option<String> },
    /// Graceful shutdown.
    Shutdown {},
}

impl fmt::Display for Control {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Control::EnsureOnline { relay_url, .. } => write!(f, "EnsureOnline({relay_url})"),
            Control::Dial { remote_endpoint_id, .. } => write!(
                f,
                "Dial({}..)",
                hex::encode(&remote_endpoint_id[..4])
            ),
            Control::DialResult { ok, path, .. } => write!(f, "DialResult(ok={ok}, path={path:?})"),
            Control::Incoming { remote_endpoint_id, .. } => write!(
                f,
                "Incoming({}..)",
                hex::encode(&remote_endpoint_id[..4])
            ),
            Control::Accept { decision, .. } => write!(f, "Accept({decision})"),
            Control::Shutdown {} => write!(f, "Shutdown"),
        }
    }
}

/// Where to find the sidecar binary.
fn sidecar_path() -> Option<PathBuf> {
    // Configuration first (absolute path or name), then next to the current exe, then PATH.
    let cfg = hbb_common::config::LocalConfig::get_option("sidecar-path");
    let cand = if !cfg.is_empty() {
        vec![PathBuf::from(cfg)]
    } else {
        let mut v = Vec::new();
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                v.push(dir.join(SIDECAR_NAME));
                v.push(dir.join(format!("{SIDECAR_NAME}.exe")));
            }
        }
        v.push(PathBuf::from(SIDECAR_NAME));
        v
    };
    cand.into_iter().find(|p| p.is_file())
}

/// Spawn the sidecar for the current uid if it is not already listening.
pub fn ensure_sidecar() -> ResultType<()> {
    if control_socket_path().exists() {
        return Ok(()); // already running (or stale socket; timeout below handles it)
    }
    let bin = sidecar_path().ok_or_else(|| {
        anyhow!("{SIDECAR_NAME} not found (set `sidecar-path` local option or place it next to the executable)")
    })?;
    // At this point the caller is the user process (the same process that will talk to
    // the sidecar). Spawn detached; the sidecar inherits IROH_RELAY_TOKEN from our env.
    let mut cmd = std::process::Command::new(&bin);
    cmd.arg("serve"); // sidecar "serve" role: online + accept control + accept dials
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let child = cmd.spawn().map_err(|e| anyhow!("failed to spawn {SIDECAR_NAME}: {e}"))?;
    // Keep a handle so the OS does not reap it silently; we never block on it here.
    std::mem::forget(child);
    log::info!("[QUIC] sidecar spawned from {}", bin.display());
    Ok(())
}

/// Make sure a live sidecar of ours is listening, self-healing a crash:
/// a socket file without a listener behind it is stale (killed -9 leaves one
/// behind) and would otherwise wedge `ensure_sidecar`'s exists() early-return
/// for good. Probe first; only reclaim when nothing answers.
pub async fn ensure_sidecar_online() -> ResultType<()> {
    if control_socket_path().exists() {
        if let Ok(conn) = connect_control().await {
            drop(conn);
            return Ok(());
        }
        log::info!("[QUIC] stale sidecar socket (no listener); removing and respawning");
        let _ = std::fs::remove_file(control_socket_path());
    }
    ensure_sidecar()?;
    // The freshly spawned sidecar needs a moment to bind its listener. Retry
    // the probe briefly instead of letting the caller race it.
    for _ in 0..3 {
        if let Ok(conn) = connect_control().await {
            drop(conn);
            return Ok(());
        }
        hbb_common::tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
    Err(anyhow!("sidecar did not come up on {}", control_socket_path().display()))
}

/// Per-uid IPC socket path for the sidecar. Same shape as the app's own IPC
/// (`Config::ipc_path`), with its own postfix so it never collides.
fn control_socket_path() -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        let uid = unsafe { hbb_common::libc::geteuid() as u32 };
        PathBuf::from(hbb_common::config::Config::ipc_path_for_uid(
            uid,
            SIDECAR_IPC_POSTFIX,
        ))
    }
    #[cfg(not(target_os = "linux"))]
    {
        PathBuf::from(hbb_common::config::Config::ipc_path(SIDECAR_IPC_POSTFIX))
    }
}

/// Open the control connection to the sidecar (same uid only).
async fn connect_control() -> ResultType<parity_tokio_ipc::Connection> {
    let path = control_socket_path();
    let conn = parity_tokio_ipc::Endpoint::connect(&path)
        .await
        .with_context(|| format!("sidecar control connect to {}", path.display()))?;
    // The os-level socket path is per-uid; additionally verify the peer is ours.
    verify_peer_uid(&conn)?;
    Ok(conn)
}

#[cfg(unix)]
fn verify_peer_uid(conn: &parity_tokio_ipc::Connection) -> ResultType<()> {
    use std::os::unix::io::AsRawFd;
    let uid = unsafe { hbb_common::libc::geteuid() as u32 };
    let peer = crate::ipc::peer_uid_from_fd(conn.as_raw_fd())
        .context("sidecar peer uid unavailable")?;
    if peer != uid {
        return Err(anyhow!(
            "sidecar owned by uid {peer}, refusing (we are uid {uid}); possible privilege boundary crossing"
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_peer_uid(_conn: &parity_tokio_ipc::Connection) -> ResultType<()> {
    // Windows named-pipe security attributes are enforced at connect time by the OS.
    Ok(())
}

/// Send one control message and await its reply.
async fn control_roundtrip(msg: &Control) -> ResultType<Control> {
    use hbb_common::tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut conn = connect_control().await?;
    let line = serde_json::to_string(msg).context("serialize control message")?;
    hbb_common::tokio::time::timeout(CONTROL_TIMEOUT, async {
        conn.write_all(line.as_bytes()).await?;
        conn.write_all(b"\n").await?;
        conn.flush().await?;
        let mut buf = Vec::new();
        conn.read_to_end(&mut buf).await?;
        let reply = String::from_utf8_lossy(&buf);
        let reply = reply.trim_end_matches('\n');
        serde_json::from_str(reply).context("parse control reply")
    })
    .await
    .context("sidecar control timeout")?
}

/// Make sure the sidecar's endpoint is online on the configured relay.
pub async fn ensure_online(relay_url: &str) -> ResultType<()> {
    let msg = Control::EnsureOnline {
        relay_url: relay_url.to_owned(),
        token_present: std::env::var_os("IROH_RELAY_TOKEN").is_some(),
    };
    let reply = control_roundtrip(&msg).await?;
    match reply {
        Control::DialResult { ok: true, .. } => Ok(()),
        Control::DialResult { ok: false, reason, .. } => Err(anyhow!(
            "sidecar could not reach relay: {}",
            reason.unwrap_or_else(|| "unknown".into())
        )),
        other => Err(anyhow!("unexpected control reply: {other}")),
    }
}

/// Borrow the sidecar's relayed QUIC session to `remote_endpoint_id` as a byte stream.
///
/// Returns the sidecar's data connection; the caller wraps it with
/// `FramedStream::from(conn, addr)` exactly like KCP/the in-process QUIC path.
pub async fn dial(
    remote_endpoint_id: [u8; 32],
    alpn: &[u8],
) -> ResultType<parity_tokio_ipc::Connection> {
    let msg = Control::Dial {
        remote_endpoint_id,
        alpn: alpn.to_vec(),
    };
    let reply = control_roundtrip(&msg).await?;
    match reply {
        Control::DialResult { ok: true, path, .. } => {
            log::info!(
                "[QUIC] outcome=relay: sidecar relay established with {}.. (path={})",
                hex::encode(&remote_endpoint_id[..4]),
                path.as_deref().unwrap_or("relay")
            );
            // After the successful Dial the sidecar accepts a fresh data connection.
            let path = control_socket_path();
            let data = parity_tokio_ipc::Endpoint::connect(&path)
                .await
                .with_context(|| format!("sidecar data connect to {}", path.display()))?;
            verify_peer_uid(&data)?;
            Ok(data)
        }
        Control::DialResult { ok: false, reason, .. } => Err(anyhow!(
            "sidecar dial failed: {}",
            reason.unwrap_or_else(|| "unknown".into())
        )),
        other => Err(anyhow!("unexpected control reply: {other}")),
    }
}

/// Extra DNS-free fallback: report the sidecar feature state for the debug panel.
pub fn available() -> bool {
    sidecar_path().is_some()
}

/// An inbound-session notice the sidecar pushed to this responder (P3-2K).
/// Mirrors the wire `Control::Incoming`; see docs/P3-CLIENT-DESIGN.md §3.2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingSession {
    pub remote_endpoint_id: [u8; 32],
    pub path: Option<String>,
}

/// Parse one control line into an `IncomingSession`. Non-`Incoming` controls
/// yield `None`: they are not expected on the subscribe connection, but the
/// server may send them and they must not derail the long-running loop.
fn parse_incoming_line(line: &str) -> Option<IncomingSession> {
    let msg: Control = serde_json::from_str(line).ok()?;
    match msg {
        Control::Incoming {
            remote_endpoint_id,
            path,
        } => Some(IncomingSession {
            remote_endpoint_id,
            path,
        }),
        _ => None,
    }
}

/// The `Accept` reply line the serve-side notify loop waits for after pushing an
/// `Incoming` (advisory, bounded by its own timeout; docs/P3-MVP.md §6.3->task-38).
fn accept_reply_line(decision: bool, reason: &str) -> ResultType<String> {
    serde_json::to_string(&Control::Accept {
        decision,
        reason: Some(reason.to_owned()),
    })
    .context("serialize Accept reply")
}

/// Read one LF-terminated JSON line. The subscribe connection is long-lived and
/// the server never closes it on its own, so the `read_to_end` that
/// `control_roundtrip` relies on cannot be used here.
async fn read_control_line(conn: &mut parity_tokio_ipc::Connection) -> ResultType<String> {
    use hbb_common::tokio::io::AsyncReadExt;
    let mut line = String::new();
    let mut buf = [0u8; 1];
    loop {
        let n = conn.read(&mut buf).await?;
        if n == 0 {
            return Err(anyhow!("sidecar closed the subscribe connection"));
        }
        if buf[0] == b'\n' {
            break;
        }
        line.push(buf[0] as char);
    }
    Ok(line)
}

/// Keep a long-lived subscription to the sidecar's inbound-session notices:
/// handshake with the subscribe marker (`Accept{true,"subscribe"}`, the P3-2J
/// wire), then read `Incoming` lines forever, hand each to `on_incoming` (its
/// return value becomes the advisory `Accept` decision) and reply so the
/// serve-side notify loop never waits out its reply timeout.
///
/// Long-running by design; a connection error returns and the caller decides
/// whether to resubscribe. The responder wiring point that should spawn this
/// task and then open the parked data connection is a TODO in `client.rs` (see
/// docs/P3-MVP.md §6.3); with the manual-pairing MVP the responder is reached
/// through the Dial path instead, so this subscription is not yet driven.
pub async fn subscribe_incoming<F>(mut on_incoming: F) -> ResultType<()>
where
    F: FnMut(IncomingSession) -> bool,
{
    use hbb_common::tokio::io::AsyncWriteExt;
    let mut conn = connect_control().await?;
    let handshake = Control::Accept {
        decision: true,
        reason: Some("subscribe".to_owned()),
    };
    let line = serde_json::to_string(&handshake).context("serialize subscribe handshake")?;
    conn.write_all(line.as_bytes()).await?;
    conn.write_all(b"\n").await?;
    conn.flush().await?;
    loop {
        let line = read_control_line(&mut conn).await?;
        let Some(session) = parse_incoming_line(&line) else {
            log::debug!("[QUIC] non-Incoming control on subscribe connection");
            continue;
        };
        log::info!(
            "[QUIC] inbound sidecar session from {}.. (path={})",
            hex::encode(&session.remote_endpoint_id[..4]),
            session.path.as_deref().unwrap_or("-")
        );
        let decision = on_incoming(session);
        let reply = accept_reply_line(decision, if decision { "accept" } else { "reject" })?;
        conn.write_all(reply.as_bytes()).await?;
        conn.write_all(b"\n").await?;
        conn.flush().await?;
    }
}

/// F1 deterministic endpoint derivation (docs/RENDEZVOUS-DISCOVERY.md §1.1):
/// `EndpointId = ed25519_public(HKDF-SHA256(ikm = pk, salt = "nervdesk-quic-sidecar",
/// info = "nervdesk/quic-sidecar/endpoint/v1", okm = 32))`. A pure function over the
/// peer's device ed25519 public key: the caller and the responder's sidecar derive the
/// same EndpointId from the same bytes, with no server cooperation.
///
/// The ed25519 public derivation uses `from_seed_unchecked`: the seed is our own KDF
/// output, so the endpoint public key is simply computed from it. (`from_seed_and_public_key`
/// would require pub(seed) == pk, which never holds -- the endpoint is a *derived* identity,
/// not the device key itself.) KDF parameter drift is caught by the golden-vector test below.
pub fn derive_endpoint_id(pk: &[u8; 32]) -> ResultType<[u8; 32]> {
    use ring::hkdf::{KeyType, Salt, HKDF_SHA256};
    use ring::signature::{Ed25519KeyPair, KeyPair as _}; // `public_key()` from the trait
    const SALT: &[u8] = b"nervdesk-quic-sidecar";
    const INFO: &[&[u8]] = &[b"nervdesk/quic-sidecar/endpoint/v1"];
    // ring's KeyType is implemented for its own Algorithm only; a length struct is the
    // pattern its own hkdf tests use for a fixed-length Okm.
    struct OkmLen(usize);
    impl KeyType for OkmLen {
        fn len(&self) -> usize {
            self.0
        }
    }

    let prk = Salt::new(HKDF_SHA256, SALT).extract(pk);
    let mut seed = [0u8; 32];
    prk.expand(INFO, OkmLen(32))
        .map_err(|e| anyhow!("HKDF expand failed: {e}"))?
        .fill(&mut seed)
        .map_err(|e| anyhow!("HKDF output fill failed: {e}"))?;
    let pair = Ed25519KeyPair::from_seed_unchecked(&seed)
        .map_err(|e| anyhow!("ed25519 pair from derived seed failed: {e}"))?;
    let endpoint_id: [u8; 32] = pair
        .public_key()
        .as_ref()
        .try_into()
        .map_err(|_| anyhow!("ed25519 public key is not 32 bytes"))?;
    Ok(endpoint_id)
}

/// Parse the manual-pairing form of an EndpointId: hex, 64 chars (fallback path D,
/// docs/RENDEZVOUS-DISCOVERY.md §6).
fn parse_manual_endpoint_id(raw: &str) -> Option<[u8; 32]> {
    let raw = raw.trim();
    match hex::decode(raw) {
        Ok(b) if b.len() == 32 => {
            let mut id = [0u8; 32];
            id.copy_from_slice(&b);
            Some(id)
        }
        Ok(b) => {
            log::warn!(
                "[QUIC] sidecar-peer-endpoint-id is {} bytes, expected 32; skipping sidecar relay",
                b.len()
            );
            None
        }
        Err(e) => {
            log::warn!(
                "[QUIC] sidecar-peer-endpoint-id not a valid hex peer id: {e}; skipping"
            );
            None
        }
    }
}

/// Resolve the peer EndpointId, F1 first: deterministically derived from the peer's
/// device ed25519 public key (RENDEZVOUS-DISCOVERY §1), else the manual-pairing option
/// (fallback D). `None` sends the caller down the legacy path.
fn resolve_endpoint_id(peer_pk: Option<&[u8; 32]>, manual_raw: &str) -> Option<[u8; 32]> {
    if let Some(pk) = peer_pk {
        match derive_endpoint_id(pk) {
            Ok(e) => return Some(e),
            Err(e) => log::warn!("[QUIC] F1 endpoint derivation failed ({e}); trying manual pairing"),
        }
    }
    parse_manual_endpoint_id(manual_raw)
}

/// MVP entry point used by the connect race: try the sidecar QUIC relay to `peer_id`,
/// returning the session as a `Stream` on success, or `None` to fall back to the legacy
/// path (hbbr relay). Never fails the caller: any error is logged and mapped to `None`.
///
/// The peer EndpointId is resolved F1-first from the peer's device key (`peer_pk`,
/// extracted from the rendezvous-verified `PunchHoleResponse.pk`), falling back to the
/// manual-pairing option `sidecar-peer-endpoint-id` (P3-CLIENT-DESIGN §5.3, degraded).
pub async fn try_relay(peer_id: &str, peer_pk: Option<&[u8; 32]>) -> Option<Stream> {
    let manual_raw = hbb_common::config::LocalConfig::get_option("sidecar-peer-endpoint-id");
    let Some(endpoint_id) = resolve_endpoint_id(peer_pk, &manual_raw) else {
        return None;
    };
    match run_relay(peer_id, endpoint_id).await {
        Ok(stream) => Some(stream),
        Err(e) => {
            log::info!("[QUIC] sidecar relay unavailable, falling back to legacy: {e}");
            None
        }
    }
}

async fn run_relay(peer_id: &str, endpoint_id: [u8; 32]) -> ResultType<Stream> {
    ensure_sidecar_online().await?;
    // The relay URL is already RustDesk's configured rendezvous/relay host; the sidecar
    // speaks to the iroh relay (path-1) which today fronts `iroh.nervcode.eu.org`. The
    // sidecar reads its own relay config; here we only make sure it is online.
    ensure_online("").await?;
    let conn = dial(endpoint_id, b"/nervdesk/p3/1").await?;
    let addr = hbb_common::config::Config::get_any_listen_addr(false);
    log::info!("[QUIC] sidecar relay session wrapping IPC stream for peer {peer_id}");
    Ok(Stream::Tcp(hbb_common::tcp::FramedStream::from(conn, addr)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctl(msg: &Control) -> String {
        serde_json::to_string(msg).expect("control serializes")
    }

    #[test]
    fn parse_incoming_line_extracts_endpoint() {
        let mut id = [0u8; 32];
        id[0] = 1;
        id[31] = 2;
        // Exact wire shape the serve notify loop writes (serde_json on [u8; 32]):
        // {"Incoming":{"remote_endpoint_id":[...],"path":"relay"}}
        let line = ctl(&Control::Incoming {
            remote_endpoint_id: id,
            path: Some("relay".to_owned()),
        });
        let parsed = parse_incoming_line(&line).expect("Incoming line must parse");
        assert_eq!(parsed.remote_endpoint_id, id);
        assert_eq!(parsed.path.as_deref(), Some("relay"));
    }

    #[test]
    fn parse_incoming_line_ignores_other_controls() {
        let line = ctl(&Control::DialResult {
            ok: true,
            reason: None,
            path: Some("relay".to_owned()),
        });
        assert!(parse_incoming_line(&line).is_none());
        assert!(parse_incoming_line("not json at all").is_none());
    }

    #[test]
    fn accept_reply_line_round_trips() {
        let line = accept_reply_line(true, "accept").expect("Accept serializes");
        match serde_json::from_str::<Control>(&line).expect("Accept deserializes") {
            Control::Accept { decision, reason } => {
                assert!(decision);
                assert_eq!(reason.as_deref(), Some("accept"));
            }
            other => panic!("expected Accept, got {other}"),
        }
        let line = accept_reply_line(false, "reject").expect("Accept serializes");
        match serde_json::from_str::<Control>(&line).expect("Accept deserializes") {
            Control::Accept { decision, reason } => {
                assert!(!decision);
                assert_eq!(reason.as_deref(), Some("reject"));
            }
            other => panic!("expected Accept, got {other}"),
        }
    }

    /// Fixed pk -> fixed seed -> fixed EndpointId. Guards the KDF parameters (salt/info/
    /// version, docs/RENDEZVOUS-DISCOVERY.md §1.1): any drift changes this vector and
    /// every F1 pair would stop matching. Established once from the derivation itself.
    #[test]
    fn derive_endpoint_id_golden_vector() {
        let pk: [u8; 32] = [
            0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42,
            0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42,
            0x42, 0x42, 0x42, 0x42,
        ];
        // GOLDEN: first derived from this exact pk (HKDF-SHA256, salt "nervdesk-quic-sidecar",
        // info "nervdesk/quic-sidecar/endpoint/v1"); freezes the KDF parameters.
        let expected: [u8; 32] = [
            167, 145, 24, 83, 54, 229, 103, 62, 50, 97, 101, 185, 135, 9, 27, 51, 193, 172, 59,
            43, 222, 241, 70, 29, 157, 113, 115, 227, 237, 26, 206, 27,
        ];
        let got = derive_endpoint_id(&pk).expect("derives");
        assert_eq!(got, expected, "KDF parameters drifted: golden vector mismatch");
    }

    /// Deterministic: the same pk derives the same endpoint every call, and distinct pks
    /// derive distinct endpoints (the endpoint is a fresh derived identity, never equal to
    /// the device key it is derived from).
    #[test]
    fn derive_endpoint_id_is_deterministic_and_distinct() {
        let pk = [0x07u8; 32];
        let a = derive_endpoint_id(&pk).expect("derives");
        let b = derive_endpoint_id(&pk).expect("derives");
        assert_eq!(a, b);
        let other = derive_endpoint_id(&[0x08u8; 32]).expect("derives");
        assert_ne!(a, other);
        assert_ne!(a, pk, "endpoint must not equal the device key");
    }

    #[test]
    fn parse_manual_endpoint_id_accepts_64_hex_rejects_rest() {
        let id = [0x11u8; 32];
        let hexed: String = id.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(parse_manual_endpoint_id(&hexed), Some(id));
        assert_eq!(parse_manual_endpoint_id(""), None);
        assert_eq!(parse_manual_endpoint_id("zz"), None);
        assert_eq!(parse_manual_endpoint_id(&hexed[..62]), None);
    }

    /// F1 wins when the peer pk is available; manual pairing covers pk-less peers; both
    /// missing sends the caller down the legacy path.
    #[test]
    fn resolve_endpoint_id_f1_first_then_manual() {
        let pk = [0x2Au8; 32];
        let from_f1 = derive_endpoint_id(&pk).unwrap();
        assert_eq!(resolve_endpoint_id(Some(&pk), ""), Some(from_f1));
        assert_eq!(resolve_endpoint_id(None, ""), None);

        let manual = [0x5Au8; 32];
        let hexed: String = manual.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(resolve_endpoint_id(None, &hexed), Some(manual));
        // F1 still wins over manual when both are present.
        assert_eq!(resolve_endpoint_id(Some(&pk), &hexed), Some(from_f1));
        assert_eq!(resolve_endpoint_id(None, "garbage"), None);
    }
}