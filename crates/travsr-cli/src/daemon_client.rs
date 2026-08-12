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
    // Re-exec ourselves as the long-lived foreground worker (which re-acquires the
    // lock — the last-line-of-defense guard for the tight spawn race).
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["daemon", "start", "--foreground"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // #503: fully detach from the launching console. Without these flags the
    // daemon stays attached to the parent's console, so Ctrl+C in that
    // terminal reaches its ctrl_c handler and shuts it down, closing the
    // terminal kills it via CTRL_CLOSE_EVENT, and the Task Scheduler
    // autostart path pops a visible console window. DETACHED_PROCESS gives
    // the child no console at all; CREATE_NEW_PROCESS_GROUP takes it out of
    // the parent's Ctrl+C signal group.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    // #592: clear any breadcrumb from a previous failed start so we only observe
    // THIS start's outcome.
    let err_path = repo_root.join(".travsr").join("daemon-start.err");
    let _ = std::fs::remove_file(&err_path);

    let spawned = cmd.spawn();
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

/// Warn on stderr when the call-graph index cannot answer authoritatively.
///
/// The MCP tools have carried this note since #617; the CLI never did. So a
/// terminal user running `travsr references X` while Phase B was still building
/// got an empty result and no indication that empty meant "not indexed yet"
/// rather than "no callers". The agent was told; the human was not.
///
/// Deliberately a warning rather than terminal progress output. Once `daemon
/// start` returns, the daemon is detached and owns no terminal, so nothing can
/// be pushed while indexing runs. What a user actually needs is not "work is
/// happening" but "the answer you are reading may be incomplete" — which is
/// only worth saying at the moment they ask.
///
/// Opens its own read-only handle because `travsr ask` answers from the daemon
/// on the warm path and never opens a store locally at all. Three metadata
/// reads, so the cost does not justify threading a store through every caller.
///
/// Silent on any error: a freshness note is not worth failing a query over.
pub fn warn_if_call_graph_degraded(db_path: &Path) {
    if let Ok(store) = open_read_store(db_path) {
        if let Some(note) = travsr_mcp::phase_b_degraded_note(&store) {
            eprintln!("warning: {note}");
        }
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

    /// The note is shared with the MCP tools rather than reimplemented, so what
    /// is worth pinning here is that the CLI classifies the same three states
    /// the same way — and, just as importantly, stays silent on the fourth.
    #[test]
    fn the_cli_and_the_mcp_tools_agree_on_call_graph_completeness() {
        use travsr_store::SqliteStore;

        let set = |s: &mut SqliteStore, pairs: &[(&str, &str)]| {
            for (k, v) in pairs {
                s.set_meta(k, v).unwrap();
            }
        };

        // Phase B never ran: init happened, no phase_b marker.
        let mut store = SqliteStore::open_in_memory().unwrap();
        set(&mut store, &[("last_commit", "abc1234")]);
        assert!(travsr_mcp::phase_b_degraded_note(&store).is_some());

        // Phase B ran, but HEAD has moved past it.
        let mut store = SqliteStore::open_in_memory().unwrap();
        set(
            &mut store,
            &[("last_commit", "def5678"), ("phase_b_commit", "abc1234")],
        );
        assert!(travsr_mcp::phase_b_degraded_note(&store).is_some());

        // Markers agree, but a watcher reindex dropped call edges (#583).
        let mut store = SqliteStore::open_in_memory().unwrap();
        set(
            &mut store,
            &[
                ("last_commit", "abc1234"),
                ("phase_b_commit", "abc1234"),
                ("phase_b_dirty", "1"),
            ],
        );
        assert!(travsr_mcp::phase_b_degraded_note(&store).is_some());

        // Complete and clean: silent. A note that always fires is noise, and
        // noise is how the ADR-017 warning made the daemon log unreadable.
        let mut store = SqliteStore::open_in_memory().unwrap();
        set(
            &mut store,
            &[("last_commit", "abc1234"), ("phase_b_commit", "abc1234")],
        );
        assert_eq!(travsr_mcp::phase_b_degraded_note(&store), None);
    }
}
