//! Daemon-routed CLI queries (#318 O1).
//!
//! `ask` / `graph` / `status` first try the daemon's control socket — the
//! daemon holds the store open and warm, so a routed query skips the
//! per-command SQLite open that dominates CLI latency. Any failure (no daemon,
//! protocol version skew, transport error, malformed payload) falls back to
//! [`open_read_store`], which itself prefers the read-only fast path.

use std::path::Path;

use serde::de::DeserializeOwned;
use travsr_store::SqliteStore;

/// Send one control message to the daemon for `repo_root`.
///
/// Dispatches to the platform-appropriate transport:
/// Unix → `UnixTransport` (domain socket), Windows → `NamedPipeTransport`.
pub fn send_daemon_command(
    repo_root: &Path,
    msg: &travsr_ipc::ControlMessage,
) -> anyhow::Result<travsr_ipc::ControlResponse> {
    let addr = travsr_ipc::ControlAddr::for_repo(repo_root);

    #[cfg(unix)]
    {
        let travsr_dir = repo_root.join(".travsr");
        let mut t = travsr_ipc::unix::UnixTransport::connect(&addr, &travsr_dir)?;
        travsr_ipc::ControlTransport::send_request(&mut t, msg)
    }

    #[cfg(windows)]
    {
        let mut t = travsr_ipc::windows::NamedPipeTransport::connect(&addr)?;
        travsr_ipc::ControlTransport::send_request(&mut t, msg)
    }

    #[cfg(all(not(unix), not(windows)))]
    {
        let _ = (addr, msg);
        anyhow::bail!("daemon control socket not supported on this platform")
    }
}

/// Route one query to a running daemon. `None` means "use the direct path" —
/// no daemon listening, version skew (old/new daemon), or a payload the CLI
/// cannot parse. Never errors: daemon routing is best-effort by design.
pub fn try_query<T: DeserializeOwned>(
    repo_root: &Path,
    tool: &str,
    args: serde_json::Value,
) -> Option<T> {
    let msg = travsr_ipc::ControlMessage::Query {
        protocol: travsr_ipc::QUERY_PROTOCOL_VERSION,
        tool: tool.to_string(),
        args,
    };
    let resp = send_daemon_command(repo_root, &msg).ok()?;
    // Handshake: the daemon must speak exactly our query protocol version.
    // Older daemons answer `ok:false` (parse error) with no protocol field.
    if !resp.ok || resp.protocol != Some(travsr_ipc::QUERY_PROTOCOL_VERSION) {
        return None;
    }
    serde_json::from_value(resp.result?).ok()
}

/// Direct-open fallback: read-only fast open (skips migrations and FTS/vocab/
/// synonym backfills), degrading to a full writable open when the store has
/// pending migrations or the read-only open is not possible.
pub fn open_read_store(db_path: &Path) -> anyhow::Result<SqliteStore> {
    match SqliteStore::open_read_only(db_path) {
        Ok(s) => Ok(s),
        Err(_) => Ok(SqliteStore::open(db_path)?),
    }
}
