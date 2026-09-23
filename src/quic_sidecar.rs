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

/// MVP entry point used by the connect race: try the sidecar QUIC relay to `peer_id`,
/// returning the session as a `Stream` on success, or `None` to fall back to the legacy
/// path (hbbr relay). Never fails the caller: any error is logged and mapped to `None`.
///
/// The peer's EndpointId is taken from the local option `sidecar-peer-endpoint-id`
/// (hex, 64 chars). This is the manual-pairing MVP form (P3-CLIENT-DESIGN §5.3); the
/// second step replaces it with rendezvous-carried binding.
pub async fn try_relay(peer_id: &str) -> Option<Stream> {
    let raw = hbb_common::config::LocalConfig::get_option("sidecar-peer-endpoint-id");
    let raw = raw.trim();
    let endpoint_id = match hex::decode(raw) {
        Ok(b) if b.len() == 32 => {
            let mut id = [0u8; 32];
            id.copy_from_slice(&b);
            id
        }
        Ok(b) => {
            log::warn!(
                "[QUIC] sidecar-peer-endpoint-id is {} bytes, expected 32; skipping sidecar relay",
                b.len()
            );
            return None;
        }
        Err(e) => {
            log::warn!(
                "[QUIC] sidecar-peer-endpoint-id not a valid hex peer id: {e}; skipping"
            );
            return None;
        }
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
    ensure_sidecar()?;
    // The relay URL is already RustDesk's configured rendezvous/relay host; the sidecar
    // speaks to the iroh relay (path-1) which today fronts `iroh.nervcode.eu.org`. The
    // sidecar reads its own relay config; here we only make sure it is online.
    ensure_online("").await?;
    let conn = dial(endpoint_id, b"/nervdesk/p3/1").await?;
    let addr = hbb_common::config::Config::get_any_listen_addr(false);
    log::info!("[QUIC] sidecar relay session wrapping IPC stream for peer {peer_id}");
    Ok(Stream::Tcp(hbb_common::tcp::FramedStream::from(conn, addr)))
}