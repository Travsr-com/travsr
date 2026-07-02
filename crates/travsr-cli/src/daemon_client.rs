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

/// Outcome of an attempt to bring up the background daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpawnOutcome {
    /// We spawned it (and confirmed it took the lock) — or a concurrent starter did.
    Started,
    /// A daemon already holds the lock; nothing was spawned.
    AlreadyRunning,
    /// We spawned a child but no daemon took the lock within the timeout.
    Failed,
}

/// True iff a **live** daemon currently holds this repo's exclusive lock.
///
/// This is the race-free singleton authority. It tries the very same
/// `.travsr/daemon.lock` flock the daemon holds for its lifetime, rather than
/// probing the control socket (unbound during the 10–30 s initial watcher scan)
/// or reading the lock-file PID (momentarily empty during the daemon's own
/// open→write window). Because the OS releases an flock when its holder dies,
/// "held" inherently means "a live process holds it" — no PID-liveness check
/// needed. If we can take the lock, no daemon holds it and we release immediately.
pub(crate) fn daemon_lock_held(repo_root: &Path) -> bool {
    use fs2::FileExt as _;
    let lock_path = repo_root.join(".travsr").join("daemon.lock");
    // NB: no `.truncate(true)` — never clobber a running daemon's PID content.
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path)
    else {
        return false; // .travsr missing / unopenable → no daemon possible
    };
    match file.try_lock_exclusive() {
        Ok(()) => {
            let _ = fs2::FileExt::unlock(&file);
            false
        }
        Err(_) => true,
    }
}

/// Spawn the background daemon **only if** none currently holds the repo lock,
/// then confirm the outcome. Race-free front door used by `daemon start`,
/// `daemon restart`, and `travsr init` so no entry point ever spawns a doomed
/// child against a running daemon.
pub(crate) fn spawn_background_daemon(repo_root: &Path, exe: &Path) -> SpawnOutcome {
    if daemon_lock_held(repo_root) {
        return SpawnOutcome::AlreadyRunning;
    }
    // Re-exec ourselves as the long-lived foreground worker (which re-acquires the
    // lock — the last-line-of-defense guard for the tight spawn race).
    let spawned = std::process::Command::new(exe)
        .args(["daemon", "start", "--foreground"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    if spawned.is_err() {
        return SpawnOutcome::Failed;
    }
    // Poll ≤2 s for the lock to become held (by our child, or by whoever won a
    // concurrent race — either way a single daemon is now running).
    for _ in 0..40 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        if daemon_lock_held(repo_root) {
            return SpawnOutcome::Started;
        }
    }
    SpawnOutcome::Failed
}

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

#[cfg(test)]
mod lock_tests {
    use super::*;

    #[test]
    fn lock_not_held_on_empty_repo() {
        let tmp = tempfile::tempdir().unwrap();
        // No .travsr dir → cannot open lock → not held (no daemon possible).
        assert!(!daemon_lock_held(tmp.path()));
        // With .travsr present but nobody holding the flock → still not held.
        std::fs::create_dir_all(tmp.path().join(".travsr")).unwrap();
        assert!(!daemon_lock_held(tmp.path()));
    }

    #[test]
    fn lock_held_when_another_fd_holds_flock() {
        use fs2::FileExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let travsr = tmp.path().join(".travsr");
        std::fs::create_dir_all(&travsr).unwrap();
        // Simulate a running daemon: hold an exclusive flock on daemon.lock.
        let held = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(travsr.join("daemon.lock"))
            .unwrap();
        held.lock_exclusive().unwrap();
        assert!(
            daemon_lock_held(tmp.path()),
            "must report held while flock is taken"
        );
        fs2::FileExt::unlock(&held).unwrap();
        drop(held);
        assert!(
            !daemon_lock_held(tmp.path()),
            "must report free once released"
        );
    }
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
