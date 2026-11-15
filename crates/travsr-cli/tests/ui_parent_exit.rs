//! `travsr ui` stops when the process that started it is gone (#1060).
//!
//! The VS Code extension starts `travsr ui` with a piped stdin it never closes
//! and sets `TRAVSR_UI_EXIT_ON_STDIN_EOF=1`. If the editor crashes or is
//! force-killed, its extension host never runs `deactivate`, but the operating
//! system still closes the pipe, and the server must exit on that end of file
//! rather than keep serving with nobody left to stop it. Closing stdin here is
//! exactly what the OS does when the parent dies.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// Start `travsr ui --port 0` in an empty folder and wait for its address.
fn start(exit_on_eof: bool) -> (Child, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_travsr"));
    cmd.args(["ui", "--port", "0"])
        .current_dir(dir.path())
        .env("TRAVSR_DISABLE_REGISTRY", "1")
        .env("HOME", dir.path())
        .env("USERPROFILE", dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if exit_on_eof {
        cmd.env("TRAVSR_UI_EXIT_ON_STDIN_EOF", "1");
    } else {
        cmd.env_remove("TRAVSR_UI_EXIT_ON_STDIN_EOF");
    }
    let mut child = cmd.spawn().unwrap();

    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    loop {
        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(line) if line.starts_with("travsr ui: serving http") => break,
            Ok(_) => {}
            Err(e) => {
                let _ = child.kill();
                panic!("travsr ui never printed its address: {e}");
            }
        }
    }
    (child, dir)
}

/// Wait for `child` to exit on a helper thread (`Child::try_wait` is banned in
/// this repo), giving up after `limit`.
fn exits_within(mut child: Child, limit: Duration) -> Option<ExitStatus> {
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait());
    });
    match rx.recv_timeout(limit) {
        Ok(status) => Some(status.unwrap()),
        Err(_) => {
            eprintln!("travsr ui (pid {pid}) still running after {limit:?}");
            None
        }
    }
}

#[test]
fn closing_stdin_stops_the_server_when_asked_to() {
    let (mut child, _dir) = start(true);
    // The parent dying: its end of the pipe is closed.
    drop(child.stdin.take());
    let status = exits_within(child, Duration::from_secs(15))
        .expect("travsr ui must exit once its stdin closes");
    assert!(status.success(), "a clean stop, not a crash: {status:?}");
}

#[test]
fn closing_stdin_does_nothing_without_the_variable() {
    // A terminal user's `travsr ui` must not stop because stdin hit end of
    // file (stdin redirected from /dev/null, a CI runner): only Ctrl-C stops it.
    let (mut child, _dir) = start(false);
    drop(child.stdin.take());
    std::thread::sleep(Duration::from_secs(2));
    // Still running, so the kill is what ends it: not a clean exit.
    let _ = child.kill();
    let status = child.wait().unwrap();
    assert!(
        !status.success(),
        "travsr ui exited on its own after stdin closed: {status:?}"
    );
}
