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
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Serialize the tests in this file. Each deliberately makes a handle
/// inheritable in THIS process (or relies on none being so); a concurrent
/// sibling test spawning a CLI child with piped stdio would inherit it too
/// and hold the other test's pipe open past its EOF assertion.
static SERIAL: Mutex<()> = Mutex::new(());

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
            "travsr {args:?}: stdout never reached EOF within {STAGE_DEADLINE:?}, \
             the detached daemon inherited this pipe's write handle (#572)"
        )
    });
    let err = err_rx.recv_timeout(STAGE_DEADLINE).unwrap_or_else(|_| {
        panic!(
            "travsr {args:?}: stderr never reached EOF within {STAGE_DEADLINE:?}, \
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

/// Poll `daemon status` until it reports the daemon running, up to `deadline`.
///
/// `daemon start` confirms the spawn for only ~5 s and then legitimately
/// returns with the daemon still "starting": the child holds the repo lock
/// but has not bound its control pipe yet (store open, embed sidecar loading
/// its model, initial watcher scan — or a concurrent reindex holding the
/// embed lock). Status only flips to "running [" once the pipe binds, so a
/// single immediate snapshot races startup and fails on any slow machine.
fn wait_for_daemon_running(repo: &Path, deadline: Duration) -> String {
    let cutoff = Instant::now() + deadline;
    loop {
        let out = daemon_status(repo);
        if out.contains("running [") {
            return out;
        }
        assert!(
            Instant::now() < cutoff,
            "daemon must be running after the piped start completed, but it never \
             reported so within {deadline:?}; last status:\n{out}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// True iff a live daemon holds this repo's `.travsr/daemon.lock` flock —
/// the same race-free singleton probe the CLI uses. The OS drops the flock
/// when its holder dies, so "held" inherently means a live process.
fn repo_daemon_alive(repo: &Path) -> bool {
    use fs2::FileExt as _;
    // NB: no `.truncate(true)` — never clobber a running daemon's PID content.
    #[allow(clippy::suspicious_open_options)]
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(repo.join(".travsr").join("daemon.lock"))
    else {
        return false; // no .travsr → no daemon possible
    };
    match file.try_lock_exclusive() {
        Ok(()) => {
            let _ = fs2::FileExt::unlock(&file);
            false
        }
        Err(_) => true,
    }
}

/// Wait until every daemon spawned from the test binary has actually exited.
/// Returns `true` once no process runs this binary anymore.
///
/// `daemon stop` is acknowledged before the process is gone, and a daemon
/// that outlives its (deleted) temp repo can linger past any status poll —
/// which then keeps `target\debug\travsr.exe` locked and fails the next
/// `cargo` relink with "Access is denied". An executing image denies write
/// access, so an append-open succeeding is proof no process is running this
/// binary anymore. Best-effort: gives up after `deadline` (a developer's own
/// daemon running the same debug binary would legitimately hold it).
fn wait_for_daemon_exit(deadline: Duration) -> bool {
    let cutoff = Instant::now() + deadline;
    while Instant::now() < cutoff {
        if std::fs::OpenOptions::new()
            .append(true)
            .open(travsr_exe())
            .is_ok()
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Stop this repo's daemon and only return once it is actually gone.
///
/// One fire-and-forget `daemon stop` is not enough: sent while the daemon is
/// still starting (repo lock held, control pipe not bound yet) the stop used
/// to come back as "not running" and was silently lost — the daemon then
/// outlived the deleted temp repo forever, serving stale answers and keeping
/// `target\debug\travsr.exe` locked. The CLI now waits out that window
/// itself, but the test's cleanup must not depend on the code under test:
/// retry the stop for as long as a daemon holds this repo's lock, and
/// force-kill daemons of this binary as a last resort so one wedged stop
/// path cannot poison every later `cargo` relink on the machine.
fn stop_daemon_and_wait(repo: &Path) {
    let cutoff = Instant::now() + STAGE_DEADLINE;
    loop {
        let _ = Command::new(travsr_exe())
            .args(["daemon", "stop"])
            .current_dir(repo)
            .status();
        if !repo_daemon_alive(repo) {
            // This repo's daemon released its lock; give the process (and any
            // unrelated sibling daemon a developer may run off this binary —
            // hence best-effort) time to release the executable image too.
            wait_for_daemon_exit(Duration::from_secs(30));
            return;
        }
        if Instant::now() >= cutoff {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    kill_daemons_of_this_binary();
    wait_for_daemon_exit(Duration::from_secs(10));
}

/// Last resort: force-kill every `daemon start --foreground` process running
/// THIS test build's travsr executable. Only reached after a daemon held the
/// repo lock through the whole [`STAGE_DEADLINE`] of stop retries, i.e. the
/// stop path itself is wedged. (This may also take down a developer's own
/// daemon started from the same debug binary; at this point that beats
/// leaking a daemon that locks the binary and serves a deleted temp repo.)
fn kill_daemons_of_this_binary() {
    let exe = travsr_exe();
    let name = exe
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "travsr.exe".to_string());
    let exe = exe.to_string_lossy().replace('\'', "''");
    let script = format!(
        "Get-CimInstance Win32_Process -Filter \"Name='{name}'\" | \
         Where-Object {{ $_.ExecutablePath -eq '{exe}' -and \
                         $_.CommandLine -match 'daemon start --foreground' }} | \
         ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force }}"
    );
    let _ = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .status();
}

/// Stops the repo's daemon on drop, so a mid-test panic never strands a
/// detached daemon on the runner (or a lock on the just-built binary).
struct DaemonGuard(PathBuf);
impl Drop for DaemonGuard {
    fn drop(&mut self) {
        stop_daemon_and_wait(&self.0);
    }
}

#[test]
fn piped_daemon_start_reaches_eof_while_daemon_keeps_running() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let repo = make_repo();
    let _guard = DaemonGuard(repo.path().to_path_buf());

    // `travsr init` may itself auto-spawn the daemon, so it goes through the
    // same prompt-EOF assertion — it hung identically before the fix.
    let (status, output) = run_piped_expecting_prompt_eof(repo.path(), &["init"]);
    assert!(status.success(), "travsr init failed:\n{output}");

    // Force the explicit `daemon start` spawn path: stop whatever init
    // started and wait for it to actually exit (not merely acknowledge the
    // stop), or the start below sees the repo lock still held and reports
    // AlreadyRunning without ever spawning. This stop can race init's daemon
    // still being mid-startup, so it must retry rather than fire once.
    stop_daemon_and_wait(repo.path());

    // The issue's exact repro: `travsr daemon start | <reader>` must complete
    // promptly...
    let (status, output) = run_piped_expecting_prompt_eof(repo.path(), &["daemon", "start"]);
    assert!(status.success(), "travsr daemon start failed:\n{output}");

    // ...while the daemon it spawned keeps running. Poll rather than assert a
    // single snapshot: on a slow startup the CLI's spawn-confirmation window
    // expires with the control pipe still unbound, and status only reports
    // "running [" once the pipe binds.
    wait_for_daemon_running(repo.path(), STAGE_DEADLINE);
}

/// Anonymous pipe whose WRITE end carries `HANDLE_FLAG_INHERIT`, standing in
/// for a handle a grandparent (cargo, a test harness, CI) created inheritably
/// and passed down the spawn chain without installing it as anyone's stdout.
fn inheritable_pipe() -> (std::fs::File, std::fs::File) {
    use std::os::windows::io::FromRawHandle as _;
    use windows_sys::Win32::Foundation::{SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT};
    use windows_sys::Win32::System::Pipes::CreatePipe;

    let mut read_h: HANDLE = std::ptr::null_mut();
    let mut write_h: HANDLE = std::ptr::null_mut();
    // SAFETY: out-pointers to two local HANDLEs; null security attributes and
    // default buffer size are the documented defaults.
    let ok = unsafe { CreatePipe(&mut read_h, &mut write_h, std::ptr::null(), 0) };
    assert_ne!(
        ok,
        0,
        "CreatePipe failed: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: write_h is a live handle we just created; setting the inherit
    // flag only changes inheritance metadata.
    let ok = unsafe { SetHandleInformation(write_h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
    assert_ne!(
        ok,
        0,
        "SetHandleInformation failed: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: both handles are freshly created, owned, and unaliased; File
    // takes ownership and closes them on drop.
    let read = unsafe { std::fs::File::from_raw_handle(read_h as _) };
    let write = unsafe { std::fs::File::from_raw_handle(write_h as _) };
    (read, write)
}

/// #572 residual: an inheritable handle that is NOT one of the CLI's std
/// handles must not leak into the daemon either. Clearing the inherit flag on
/// the CLI's own stdio (the first fix) cannot see such a handle; only the
/// spawn-side `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` allowlist closes it.
#[test]
fn daemon_does_not_inherit_grandparent_pipe_handle() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let repo = make_repo();
    let _guard = DaemonGuard(repo.path().to_path_buf());

    // Set up the repo with no extra handle in play, then stop init's daemon so
    // the spawn under test below is the explicit `daemon start` path.
    let (status, output) = run_piped_expecting_prompt_eof(repo.path(), &["init"]);
    assert!(status.success(), "travsr init failed:\n{output}");
    stop_daemon_and_wait(repo.path());

    // The grandparent-chain shape: our spawn below configures stdio, so std
    // passes bInheritHandles=TRUE and the CLI inherits this write end even
    // though it is not the CLI's stdout. On the buggy build the CLI's own
    // daemon spawn forwarded every inheritable handle again, so the detached
    // daemon ended up holding it too.
    let (mut read_end, write_end) = inheritable_pipe();

    let mut child = Command::new(travsr_exe())
        .args(["daemon", "start"])
        .current_dir(repo.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn travsr daemon start");
    // Our copy is now surplus; the CLI (and, on a buggy build, the daemon it
    // spawns) holds the only other copies.
    drop(write_end);

    // Drain the CLI's own stdio so it cannot block on a full pipe buffer.
    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
    });
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
    });
    let status = child.wait().expect("wait for travsr");
    assert!(status.success(), "travsr daemon start failed");

    // The CLI has exited, closing its inherited copy. EOF on the side pipe
    // must now arrive promptly — it can only still be open if the detached
    // daemon inherited it.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = read_end.read_to_end(&mut buf);
        let _ = tx.send(());
    });
    rx.recv_timeout(Duration::from_secs(30))
        .unwrap_or_else(|_| {
            panic!(
                "side pipe never reached EOF after the CLI exited, the detached \
             daemon inherited a non-stdio handle (#572 residual)"
            )
        });

    // ...while the daemon itself keeps running.
    wait_for_daemon_running(repo.path(), STAGE_DEADLINE);
}
