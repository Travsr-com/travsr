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
    /// The daemon is up and answering on its control socket.
    Started,
    /// The daemon is alive and holding the lock but has not finished its
    /// initial scan / socket bind yet (large repo). Not a failure.
    Starting,
    /// A daemon already holds the lock; nothing was spawned.
    AlreadyRunning,
    /// We spawned a child but it never came up (spawn error, or it died during
    /// startup — e.g. a control-socket bind failure, see [`crate::daemon_start_error`]).
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
    #[allow(clippy::suspicious_open_options)]
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
    // #592: clear any breadcrumb from a previous failed start so we only observe
    // THIS start's outcome.
    let err_path = repo_root.join(".travsr").join("daemon-start.err");
    let _ = std::fs::remove_file(&err_path);

    // Re-exec ourselves as the long-lived foreground worker (which re-acquires the
    // lock — the last-line-of-defense guard for the tight spawn race).
    #[cfg(unix)]
    let spawned: std::io::Result<()> = std::process::Command::new(exe)
        .args(["daemon", "start", "--foreground"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ());
    // #503/#572: on Windows the daemon must be fully detached from the
    // launching console (DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP, or
    // Ctrl+C / closing the terminal kills it and the Task Scheduler autostart
    // pops a console window) AND spawned with an explicit handle-inheritance
    // allowlist. `std::process::Command` forces bInheritHandles=TRUE whenever
    // stdio is configured, so EVERY inheritable handle in this process leaked
    // into the long-lived daemon even though its own stdio is null — the
    // write end of a pipe the shell attached to our stdout, or one a
    // grandparent (cargo → test harness → travsr) created inheritably and
    // passed down. Either pins the pipe open so its reader never sees EOF
    // until the daemon exits. The raw spawn below lists exactly the daemon's
    // three NUL stdio handles as inheritable and nothing else.
    #[cfg(windows)]
    let spawned = travsr_plugin_host::sandbox::windows::spawn_detached_with_inherit_allowlist(
        exe,
        &["daemon", "start", "--foreground"],
    );

    if spawned.is_err() {
        return SpawnOutcome::Failed;
    }
    // Confirm the child actually came up. Polling only the lock reports a false
    // "Started" (travsr #592): the daemon takes the lock early, then opens its
    // stores, scans the tree, and only then binds its control socket — a bind
    // failure (a path over SUN_LEN, a hostile fallback dir) happens well after
    // the lock is held. So we wait for a *terminal* state instead of a timer:
    //   • the daemon answers on its socket            → Started (truly up)
    //   • it wrote a startup-error breadcrumb          → Failed
    //   • the lock was held then released (child died) → Failed
    //   • still holding the lock at the deadline       → Starting (big-repo scan)
    let mut saw_lock = false;
    for _ in 0..50 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        // Positive terminal state: a round-trip to the control socket succeeds.
        if send_daemon_command(repo_root, &travsr_ipc::ControlMessage::Status).is_ok() {
            return SpawnOutcome::Started;
        }
        if err_path.exists() {
            return SpawnOutcome::Failed;
        }
        if daemon_lock_held(repo_root) {
            saw_lock = true;
        } else if saw_lock {
            // Lock was held then released → the child died during startup.
            return SpawnOutcome::Failed;
        }
    }
    // Deadline reached: alive and scanning is fine; never having taken the lock
    // means the child never really started.
    if saw_lock {
        SpawnOutcome::Starting
    } else {
        SpawnOutcome::Failed
    }
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

/// Direct-open fallback: read-only fast open (skips migrations and FTS/vocab/
/// synonym backfills), degrading to a full writable open when the store has
/// pending migrations or the read-only open is not possible.
pub fn open_read_store(db_path: &Path) -> anyhow::Result<SqliteStore> {
    match SqliteStore::open_read_only(db_path) {
        Ok(s) => Ok(s),
        Err(_) => Ok(SqliteStore::open(db_path)?),
    }
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
