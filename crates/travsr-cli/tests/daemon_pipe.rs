//! #572: piping the daemonizing commands must not hang the pipeline.
//!
//! On Windows, `std::process::Command` spawns with `bInheritHandles=TRUE`
//! whenever stdio is configured, so every inheritable handle in the CLI —
//! including the write end of a pipe the shell attached to its stdout — used
//! to leak into the long-lived detached daemon. The shell-side reader then
//! never saw EOF: `travsr daemon start | tail -2` blocked until the daemon
//! exited. These tests capture stdout/stderr through real pipes and assert
//! EOF arrives promptly after the CLI exits, while the daemon keeps running.
#![cfg(windows)]

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Generous per-stage ceiling: a tiny-repo init plus daemon spawn completes in
/// seconds; only the #572 leak makes EOF wait for the daemon's exit.
const STAGE_DEADLINE: Duration = Duration::from_secs(180);

fn travsr_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_travsr"))
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap_or_else(|e| panic!("git {args:?} failed to spawn: {e}"));
    assert!(status.success(), "git {args:?} exited with {status}");
}

fn make_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    git(dir, &["-c", "init.defaultBranch=main", "init", "-q"]);
    git(dir, &["config", "user.email", "test@test.com"]);
    git(dir, &["config", "user.name", "Test"]);
    std::fs::write(dir.join("lib.rs"), "pub fn hello() {}\n").expect("write source file");
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
    tmp
}

/// Run `travsr <args>` with stdout and stderr captured through real pipes and
/// assert both reach EOF within [`STAGE_DEADLINE`]. This is the #572 repro
/// shape: a reader that only finishes when the last write handle closes. On
/// the buggy build, EOF waits for the daemon to exit and `recv_timeout` fires.
fn run_piped_expecting_prompt_eof(
    repo: &Path,
    args: &[&str],
) -> (std::process::ExitStatus, String) {
    let mut child = Command::new(travsr_exe())
        .args(args)
        .current_dir(repo)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn travsr");

    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");
    let (out_tx, out_rx) = std::sync::mpsc::channel();
    let (err_tx, err_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = out_tx.send(buf);
    });
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        let _ = err_tx.send(buf);
    });

    let out = out_rx.recv_timeout(STAGE_DEADLINE).unwrap_or_else(|_| {
        panic!(
            "travsr {args:?}: stdout never reached EOF within {STAGE_DEADLINE:?} — \
             the detached daemon inherited this pipe's write handle (#572)"
        )
    });
    let err = err_rx.recv_timeout(STAGE_DEADLINE).unwrap_or_else(|_| {
        panic!(
            "travsr {args:?}: stderr never reached EOF within {STAGE_DEADLINE:?} — \
             the detached daemon inherited this pipe's write handle (#572)"
        )
    });

    // EOF implies the CLI-side handles are closed; the process itself exits
    // with them, so this wait is immediate.
    let status = child.wait().expect("wait for travsr");
    let mut combined = String::from_utf8_lossy(&out).into_owned();
    combined.push_str(&String::from_utf8_lossy(&err));
    (status, combined)
}

fn daemon_status(repo: &Path) -> String {
    let output = Command::new(travsr_exe())
        .args(["daemon", "status"])
        .current_dir(repo)
        .output()
        .expect("daemon status");
    let mut s = String::from_utf8_lossy(&output.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&output.stderr));
    s
}

/// Wait until every daemon spawned from the test binary has actually exited.
///
/// `daemon stop` is acknowledged before the process is gone, and a daemon
/// that outlives its (deleted) temp repo can linger past any status poll —
/// which then keeps `target\debug\travsr.exe` locked and fails the next
/// `cargo` relink with "Access is denied". An executing image denies write
/// access, so an append-open succeeding is proof no process is running this
/// binary anymore. Best-effort: gives up after `deadline` (a developer's own
/// daemon running the same debug binary would legitimately hold it).
fn wait_for_daemon_exit(deadline: Duration) {
    let cutoff = std::time::Instant::now() + deadline;
    while std::time::Instant::now() < cutoff {
        if std::fs::OpenOptions::new()
            .append(true)
            .open(travsr_exe())
            .is_ok()
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Stops the repo's daemon on drop, so a mid-test panic never strands a
/// detached daemon on the runner (or a lock on the just-built binary).
struct DaemonGuard(PathBuf);
impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = Command::new(travsr_exe())
            .args(["daemon", "stop"])
            .current_dir(&self.0)
            .status();
        wait_for_daemon_exit(Duration::from_secs(30));
    }
}

#[test]
fn piped_daemon_start_reaches_eof_while_daemon_keeps_running() {
    let repo = make_repo();
    let _guard = DaemonGuard(repo.path().to_path_buf());

    // `travsr init` may itself auto-spawn the daemon, so it goes through the
    // same prompt-EOF assertion — it hung identically before the fix.
    let (status, output) = run_piped_expecting_prompt_eof(repo.path(), &["init"]);
    assert!(status.success(), "travsr init failed:\n{output}");

    // Force the explicit `daemon start` spawn path: stop whatever init
    // started and wait for it to actually exit (not merely acknowledge the
    // stop), or the start below sees the repo lock still held and reports
    // AlreadyRunning without ever spawning.
    let _ = Command::new(travsr_exe())
        .args(["daemon", "stop"])
        .current_dir(repo.path())
        .status();
    wait_for_daemon_exit(Duration::from_secs(60));

    // The issue's exact repro: `travsr daemon start | <reader>` must complete
    // promptly...
    let (status, output) = run_piped_expecting_prompt_eof(repo.path(), &["daemon", "start"]);
    assert!(status.success(), "travsr daemon start failed:\n{output}");

    // ...while the daemon it spawned keeps running.
    let status_out = daemon_status(repo.path());
    assert!(
        status_out.contains("running ["),
        "daemon must still be running after the piped start completed:\n{status_out}"
    );
}
