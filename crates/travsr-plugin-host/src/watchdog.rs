//! Shared I/O watchdog for subprocess transports.
//!
//! # Why this exists
//! Both the language-plugin `Sidecar` (`transport.rs`) and the `EmbedSidecar`
//! (`embed_sidecar.rs`) speak a request/response protocol over a child's
//! stdin/stdout pipes. A blocking `decode_message` read on that pipe hangs
//! **forever** if the child never writes a response — e.g. a plugin that
//! deadlocks on startup (issue #388: `travsr-lang-php` wedged on its handshake,
//! freezing `invoke_phase_b_all`'s scoped join and, with it, `travsr init` and
//! `cargo test --workspace`).
//!
//! [`with_io_watchdog`] bounds any such blocking I/O step: a watchdog thread
//! waits on a `Condvar` for up to `timeout`; if the guarded closure has not
//! finished by then it SIGKILLs the child, which closes the child's stdout and
//! unblocks the pending `decode` read (it returns EOF/error, which the caller
//! maps to a per-language failure). On the happy path the `Condvar` is notified
//! the instant the closure returns, so a healthy call is never penalised the
//! full `timeout` (a bare `sleep(timeout)` + `join()` would stall every call).

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Best-effort kill of a subprocess by OS PID. Uses a shell command so no
/// `libc`/`winapi` dependency is required. A no-op if the process already exited.
///
/// Platform note: `kill` (Unix) and the MSYS `kill` shim do NOT terminate a
/// native Windows process by its OS PID (different PID namespace — the shim
/// reports "No such process"). `child.id()` returns the *native* Windows PID, so
/// on Windows we must use `taskkill /PID <native> /F` (force) `/T` (tree) to
/// actually terminate the sidecar — otherwise the watchdog kill is a silent
/// no-op and a wedged plugin never gets killed.
pub(crate) fn kill_pid(pid: u32) {
    #[cfg(windows)]
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F", "/T"])
        .status();

    #[cfg(not(windows))]
    let _ = std::process::Command::new("kill")
        .arg(pid.to_string())
        .status();
}

/// Run a blocking sidecar I/O step guarded by a watchdog that kills the
/// subprocess (`pid`) if `io` does not complete within `timeout`.
///
/// Returns as soon as `io` completes — the watchdog waits on a `Condvar` and is
/// woken the instant we finish, so `join()` returns immediately. A bare
/// `thread::sleep(timeout)` + `join()` is NOT interruptible: it would block the
/// full `timeout` on *every* call even when the response arrived in
/// milliseconds.
pub(crate) fn with_io_watchdog<T>(pid: u32, timeout: Duration, io: impl FnOnce() -> T) -> T {
    let state = Arc::new((Mutex::new(false), Condvar::new()));
    let state_wg = Arc::clone(&state);
    let watchdog = std::thread::spawn(move || {
        let (lock, cv) = &*state_wg;
        let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let (done, res) = cv
            .wait_timeout_while(guard, timeout, |d| !*d)
            .unwrap_or_else(|e| e.into_inner());
        if res.timed_out() && !*done {
            kill_pid(pid);
        }
    });

    let out = io();

    let (lock, cv) = &*state;
    *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
    cv.notify_all();
    drop(watchdog.join());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Happy path: the closure returns well within `timeout`, so the watchdog is
    /// notified immediately and we return without waiting the full window.
    #[test]
    fn returns_immediately_when_io_completes() {
        let start = Instant::now();
        let out = with_io_watchdog(std::process::id(), Duration::from_secs(30), || 42);
        assert_eq!(out, 42);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "healthy call must not wait the full timeout"
        );
    }

    /// Regression for #388: a blocking read on a child that never responds must
    /// be unblocked by the watchdog killing the child, not hang forever. We
    /// spawn a child that sleeps far longer than the timeout and never writes to
    /// stdout; the guarded `read` blocks until the watchdog kills it, closing
    /// stdout (EOF) so `read` returns.
    ///
    /// Unix-only: relies on the `sleep` binary and `kill`-style termination.
    /// The Windows kill path (`taskkill`) is exercised in production, not here —
    /// the cross-platform mechanism (Condvar wakeup) is covered by the test above.
    #[cfg(unix)]
    #[test]
    fn watchdog_kills_hung_child_and_unblocks_read() {
        use std::io::Read;
        use std::process::{Command, Stdio};

        let mut child = Command::new("sleep")
            .arg("120")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        let mut stdout = child.stdout.take().expect("piped stdout");

        let start = Instant::now();
        // The child never writes, so this read only returns once the watchdog
        // kills the child and its stdout write end closes (EOF -> Ok(0)).
        let n = with_io_watchdog(pid, Duration::from_millis(500), || {
            let mut buf = [0u8; 16];
            stdout.read(&mut buf)
        });
        let elapsed = start.elapsed();

        assert!(matches!(n, Ok(0) | Err(_)), "read should unblock on kill");
        assert!(
            elapsed < Duration::from_secs(30),
            "watchdog must unblock the read promptly, took {elapsed:?}"
        );
        let _ = child.wait();
    }
}
