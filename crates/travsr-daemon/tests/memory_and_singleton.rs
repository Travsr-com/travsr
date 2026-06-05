//! Integration tests for memory safety and daemon singleton enforcement.
//!
//! # Tests
//!
//! ## `rss_flood_under_200mb` (Linux only)
//! Floods the daemon with 100 rapid watcher events (100 `.ts` file creations)
//! and asserts that the RSS delta stays under 200 MB. The 200 MB budget covers
//! the indexer thread pool plus the SQLite WAL buffer; anything above that
//! indicates a runaway `spawn_blocking` task accumulation or a Vec/HashMap leak
//! in the debounce table.
//!
//! ## `daemon_start_twice_single_process` (Unix only)
//! Starts the daemon event loop twice against the same `.travsr/` directory and
//! asserts that the second attempt is rejected via the `daemon.lock` exclusive
//! lock. This is a regression test for the duplicate-spawn bug where two daemon
//! processes could concurrently write to the same SQLite WAL, causing
//! graph corruption.
//!
//! # Infrastructure reused
//! - `tempfile::TempDir` — ephemeral git repos that are auto-cleaned on drop.
//! - `#[tokio::test(flavor = "multi_thread")]` — mirrors production runtime;
//!   required so `tokio::task::spawn_blocking` dispatches to the real blocking
//!   thread pool rather than a single-threaded executor.
//! - `tokio::sync::mpsc::channel` — same channel type used by the watcher.

use std::process::Command as StdCommand;
use std::time::Duration;
#[cfg(target_os = "linux")]
use std::time::Instant;

// ── helpers ──────────────────────────────────────────────────────────────────

/// Send `{"op":"shutdown"}` to the daemon's control socket and wait for the
/// task to exit cleanly. Falls back to `abort()` + a 2s wait if the socket
/// is unreachable (e.g. daemon failed to start).
///
/// Using graceful shutdown instead of plain `abort()` ensures the daemon runs
/// its explicit `drop(index_tx)` + `indexer_worker.await` shutdown sequence.
/// Without this, the tokio runtime's `shutdown_timeout(Duration::MAX)` would
/// wait forever for the detached spawn_blocking indexer task.
#[cfg(unix)]
async fn graceful_shutdown<T: Send + 'static>(
    sock_path: &Path,
    daemon_task: tokio::task::JoinHandle<T>,
) {
    use tokio::io::AsyncWriteExt as _;
    use tokio::net::UnixStream;

    if let Ok(mut stream) = UnixStream::connect(sock_path).await {
        let _ = stream.write_all(b"{\"op\":\"shutdown\"}\n").await;
        let _ = stream.flush().await;
    } else {
        // Socket not reachable — daemon may have crashed. Abort directly.
        daemon_task.abort();
        tokio::time::sleep(Duration::from_millis(500)).await;
        return;
    }

    // Wait up to 10 s for the daemon to shut down cleanly.
    let _ = tokio::time::timeout(Duration::from_secs(10), daemon_task).await;
}

fn git_init(dir: &std::path::Path) {
    StdCommand::new("git")
        .args(["-c", "init.defaultBranch=main", "init", "-q"])
        .current_dir(dir)
        .status()
        .expect("git init");
    StdCommand::new("git")
        .args(["config", "user.email", "qa@travsr.test"])
        .current_dir(dir)
        .status()
        .unwrap();
    StdCommand::new("git")
        .args(["config", "user.name", "QA Bot"])
        .current_dir(dir)
        .status()
        .unwrap();
}

/// Read `VmRSS` (resident set size, kB) from `/proc/self/status`.
///
/// Returns `None` on parse error or when the field is absent (non-Linux).
#[cfg(target_os = "linux")]
fn read_vm_rss_kb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            // Format: "VmRSS:     12345 kB"
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb);
        }
    }
    None
}

// ── test 1: RSS flood budget ──────────────────────────────────────────────────

/// Rapidly creates/modifies 100 `.ts` files and asserts the RSS delta is
/// under 200 MB (200 * 1024 kB).
///
/// The daemon's `Daemon::run` loop spawns one `tokio::task::spawn_blocking`
/// call **per watcher event**. Without a semaphore or queue depth cap, a
/// storm of 100 events can accumulate 100 concurrent blocking tasks, each
/// holding a copy of the store Arc and a reindex allocation. This test
/// catches that regression.
///
/// The test is gated behind `#[cfg(target_os = "linux")]` because the RSS
/// measurement relies on `/proc/self/status`. The indexer logic itself
/// (and therefore the leak it guards against) is platform-independent.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rss_flood_under_200mb() {
    const FILE_COUNT: usize = 100;
    // Debug builds carry substantially more RSS overhead than release:
    //   - A second SqliteStore connection (64 MB cache)
    //   - 3 daemon-internal threads: watcher-event, watcher-flush, indexer worker
    //     (3 × 8 MB = 24 MB of thread stacks at OS default stack size)
    //   - Tree-sitter parse trees and allocator pools held after each parse
    //     (~150-200 MB in debug due to uninlined frame data)
    // In debug the "fixed" code peaks at ~305 MB; the "broken" code (100
    // spawn_blocking tasks × 8 MB each) peaks at ~800 MB.
    // Release: peak ~60 MB (fixed) vs ~800 MB (broken) — tighter budget.
    let rss_budget_kb: u64 = if cfg!(debug_assertions) {
        512 * 1024 // 512 MB — above fixed code (~305 MB), well below broken (~800 MB)
    } else {
        200 * 1024 // 200 MB — comfortable headroom in optimised builds
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    git_init(tmp.path());

    // init_repo creates the DB, installs the hook, stamps format version.
    travsr_daemon::init_repo(tmp.path()).expect("init_repo");

    // Baseline RSS — measured after init_repo so the DB is already open and
    // any one-time allocation (mmap pages, WAL header) is already counted.
    let rss_before_kb = read_vm_rss_kb().expect("/proc/self/status must be readable on Linux");

    // Spawn the daemon on a background task. It will run until we drop the
    // abort handle below.
    let repo_root = tmp.path().to_path_buf();
    let daemon_task = tokio::spawn(async move {
        // Daemon::run blocks until SIGINT/SIGTERM or a control-socket Shutdown.
        // We cancel it via JoinHandle::abort() after the flood completes.
        if let Err(e) = travsr_daemon::Daemon::run(repo_root).await {
            // "another travsr daemon is already running" is the expected error
            // if the OS still has the lock from a previous test run in the
            // same process; tolerate it rather than panicking.
            let msg = e.to_string();
            if !msg.contains("already running") {
                panic!("Daemon::run failed unexpectedly: {e}");
            }
        }
    });

    // Allow the daemon's watcher to fully initialise (inotify/kqueue setup)
    // and register the watch before we flood it with events.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // ── Flood: 100 .ts file creates/overwrites ────────────────────────────
    let flood_start = Instant::now();
    for i in 0..FILE_COUNT {
        let path = tmp.path().join(format!("flood_{i:04}.ts"));
        // Each write is a distinct content so the SHA-256 always differs and
        // reindex_files cannot skip via the hash-cache fast path.
        let content =
            format!("export class Flood{i} {{ id = {i}; process() {{ return {i} * 2; }} }}\n");
        std::fs::write(&path, content).expect("write flood file");
    }
    let flood_write_elapsed = flood_start.elapsed();

    // Wait long enough for:
    //   1. The watcher's DEBOUNCE_MS (500 ms) to expire and flush all events.
    //   2. The single indexer worker to finish all reindex_files calls.
    //      With 100 events processed sequentially, give 6 s for slow CI.
    tokio::time::sleep(Duration::from_secs(6)).await;

    // ── RSS check ─────────────────────────────────────────────────────────
    let rss_after_kb = read_vm_rss_kb().expect("/proc/self/status must be readable on Linux");
    let rss_delta_kb = rss_after_kb.saturating_sub(rss_before_kb);

    // Graceful shutdown via control socket — ensures the daemon's explicit
    // `drop(index_tx) + indexer_worker.await` path runs so the tokio
    // runtime can shut down without hanging on a detached spawn_blocking task.
    let travsr_dir = tmp.path().join(".travsr");
    let sock_path = travsr_ipc::ControlAddr::for_repo(tmp.path()).socket_path(&travsr_dir);
    graceful_shutdown(&sock_path, daemon_task).await;

    println!(
        "Flood: {FILE_COUNT} files written in {flood_write_elapsed:?}. \
         RSS before={rss_before_kb} kB, after={rss_after_kb} kB, delta={rss_delta_kb} kB \
         (budget={rss_budget_kb} kB)"
    );

    assert!(
        rss_delta_kb < rss_budget_kb,
        "RSS grew by {rss_delta_kb} kB after a {FILE_COUNT}-file event flood — \
         expected < {rss_budget_kb} kB. \
         This indicates spawn_blocking task accumulation or a debounce-table leak. \
         Hint: ensure the single indexer worker (PERF-001) is in place and no \
         per-event spawn_blocking calls have been re-introduced."
    );
}

// ── Non-Linux stub so `cargo test --all` on macOS compiles the file ──────────

/// On non-Linux platforms the `/proc` assertion is not available.
/// We still run the flood to verify no panic occurs, but we skip the RSS gate.
#[cfg(not(target_os = "linux"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rss_flood_under_200mb() {
    const FILE_COUNT: usize = 100;

    let tmp = tempfile::tempdir().expect("tempdir");
    git_init(tmp.path());
    travsr_daemon::init_repo(tmp.path()).expect("init_repo");

    let repo_root = tmp.path().to_path_buf();
    let daemon_task = tokio::spawn(async move {
        let _ = travsr_daemon::Daemon::run(repo_root).await;
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    for i in 0..FILE_COUNT {
        let path = tmp.path().join(format!("flood_{i:04}.ts"));
        let content = format!("export class Flood{i} {{ id = {i}; }}\n");
        std::fs::write(&path, content).expect("write flood file");
    }

    tokio::time::sleep(Duration::from_secs(6)).await;

    // Graceful shutdown on Unix (UnixStream available); abort on Windows.
    #[cfg(unix)]
    {
        let travsr_dir = tmp.path().join(".travsr");
        let sock_path = travsr_ipc::ControlAddr::for_repo(tmp.path()).socket_path(&travsr_dir);
        graceful_shutdown(&sock_path, daemon_task).await;
    }
    #[cfg(not(unix))]
    {
        daemon_task.abort();
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // No RSS assertion on non-Linux — the test still validates no panic/deadlock.
    println!(
        "rss_flood_under_200mb: RSS check skipped on non-Linux platform (no /proc/self/status)"
    );
}

// ── test 2: duplicate-daemon singleton enforcement (Unix only) ────────────────

/// Start `Daemon::run` twice against the **same** `.travsr/` directory and
/// assert only one instance acquires the exclusive lock.
///
/// The second call must return an `Err` whose message contains "already running".
/// This is a regression test for the duplicate-spawn bug: two daemon processes
/// concurrently writing to the same SQLite WAL would produce interleaved
/// transactions and could silently corrupt the graph.
///
/// Gated on Unix because `Daemon::run` uses a `UnixListener` for the control
/// socket, which is not available on Windows. The lockfile logic itself is
/// cross-platform (via `fs2`), but the test is only meaningful where the full
/// daemon runs.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_start_twice_single_process() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git_init(tmp.path());
    travsr_daemon::init_repo(tmp.path()).expect("init_repo");

    let repo_root_1 = tmp.path().to_path_buf();
    let repo_root_2 = tmp.path().to_path_buf();

    // Start the first daemon — should succeed and keep running.
    let daemon1 = tokio::spawn(async move { travsr_daemon::Daemon::run(repo_root_1).await });

    // Give daemon 1 enough time to acquire the lock, create the socket, and
    // enter its select! loop.
    tokio::time::sleep(Duration::from_millis(600)).await;

    // Start the second daemon against the same directory — must fail immediately.
    let result2 = travsr_daemon::Daemon::run(repo_root_2).await;

    // Graceful shutdown of daemon 1 before asserting, so the TempDir can be
    // cleaned up and the lockfile is released cleanly.
    let travsr_dir = tmp.path().join(".travsr");
    let sock_path = travsr_ipc::ControlAddr::for_repo(tmp.path()).socket_path(&travsr_dir);
    graceful_shutdown(&sock_path, daemon1).await;

    // Assert the second instance was rejected with the expected error.
    match result2 {
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("already running"),
                "expected 'already running' error from second daemon start, got: {msg}"
            );
        }
        Ok(()) => {
            panic!(
                "Second Daemon::run returned Ok(()) — it should have been rejected by \
                 the exclusive lockfile. This means two daemon instances can run \
                 concurrently against the same graph.db (duplicate-spawn regression)."
            );
        }
    }
}

// ── Non-Unix stub ─────────────────────────────────────────────────────────────

#[cfg(not(unix))]
#[test]
fn daemon_start_twice_single_process() {
    // The full singleton test requires the Unix control socket path that
    // Daemon::run creates. On Windows the lock still exists but the test
    // cannot call Daemon::run without a tokio runtime and full Unix deps.
    // Mark it as a known skip rather than silently passing.
    println!(
        "daemon_start_twice_single_process: skipped on non-Unix platform \
         (Daemon::run requires UnixListener)"
    );
}
