//! A daemon runs the binary image it was spawned with for its whole life, so
//! after a binary upgrade the old daemon keeps serving old code until something
//! restarts it. The CLI now closes that gap automatically: the first time a
//! command talks to a daemon whose build version differs from this binary's, it
//! restarts the daemon so answers come from current code.
//!
//! Real-process tests on purpose: the behaviour is one process detecting that
//! another process is the wrong build and replacing it, which nothing short of
//! running it can show. `TRAVSR_BUILD_VERSION_OVERRIDE` lets the first daemon
//! claim an old version without a second, separately versioned build.
//!
//! The observable is `.travsr/daemon-restart.lock`: the restart path creates it
//! (and only it) when, and only when, it detects skew, so its presence after a
//! command distinguishes "restarted" from "left alone" on every platform. The
//! daemon's own PID file (`daemon.lock`) is deliberately not read here: the
//! daemon holds it exclusively locked for its whole life, so reading it fails on
//! Windows by design (see `daemon_lock_pid`).

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn travsr(repo: &Path, lang_toml: &Path) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin("travsr"));
    cmd.current_dir(repo)
        .env("TRAVSR_DISABLE_REGISTRY", "1")
        .env("TRAVSR_SKIP_DOWNLOAD", "1")
        .env("TRAVSR_LANG_TOML", lang_toml)
        .env_remove("CI");
    cmd
}

/// Stops the daemon the test started, however the test ends.
struct StopDaemon<'a>(&'a Path, &'a Path);

impl Drop for StopDaemon<'_> {
    fn drop(&mut self) {
        let _ = travsr(self.0, self.1)
            .args(["daemon", "stop"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// The breadcrumb the version-skew restart leaves: `try_lock_restart` creates it
/// the moment skew is detected, and nothing else touches it.
fn restart_lock(repo: &Path) -> std::path::PathBuf {
    repo.join(".travsr").join("daemon-restart.lock")
}

/// Poll `daemon status` until it reports the daemon running, or time out.
fn wait_running(repo: &Path, lang_toml: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let out = travsr(repo, lang_toml)
            .args(["daemon", "status"])
            .output()
            .unwrap();
        if String::from_utf8_lossy(&out.stdout).contains("running") {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// A committed repo with a graph index and NO daemon running.
fn indexed_repo(tmp: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let repo = tmp.join("repo");
    let lang_toml = tmp.join("lang.toml");
    std::fs::create_dir(&repo).unwrap();
    for args in [
        &["init", "-q"][..],
        &["config", "user.email", "t@t"][..],
        &["config", "user.name", "t"][..],
    ] {
        Command::new("git")
            .args(args)
            .current_dir(&repo)
            .status()
            .unwrap();
    }
    std::fs::write(repo.join("a.ts"), "export function a() { return 1; }\n").unwrap();
    Command::new("git")
        .args(["add", "-A"])
        .current_dir(&repo)
        .status()
        .unwrap();
    Command::new("git")
        .args(["commit", "-qm", "init"])
        .current_dir(&repo)
        .status()
        .unwrap();
    // Index with no daemon, so the only daemon in the test is one it starts.
    let init = travsr(&repo, &lang_toml)
        .env("CI", "1")
        .args(["init", "--no-connect"])
        .output()
        .unwrap();
    assert!(init.status.success(), "{init:?}");
    (repo, lang_toml)
}

#[test]
fn a_skewed_daemon_is_restarted_on_the_next_command() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, lang_toml) = indexed_repo(tmp.path());
    let _stop = StopDaemon(&repo, &lang_toml);

    // Start a daemon that claims an older build than this binary.
    let start = travsr(&repo, &lang_toml)
        .env("TRAVSR_BUILD_VERSION_OVERRIDE", "0.0.0-skew-old")
        .args(["daemon", "start"])
        .output()
        .unwrap();
    assert!(start.status.success(), "{start:?}");
    assert!(wait_running(&repo, &lang_toml), "the stale daemon must come up");
    assert!(
        !restart_lock(&repo).exists(),
        "nothing should have restarted anything yet"
    );

    // Any daemon-routed command, run by THIS binary (no override), detects the
    // skew and restarts the daemon before answering.
    let status = travsr(&repo, &lang_toml)
        .args(["status"])
        .output()
        .unwrap();
    assert!(status.status.success(), "{status:?}");
    assert!(
        restart_lock(&repo).exists(),
        "a skewed daemon must trigger the restart path"
    );
    assert!(
        wait_running(&repo, &lang_toml),
        "a daemon must be running again after the restart"
    );

    // The replacement is this binary's version, so a second command leaves it
    // alone: clear the breadcrumb, run again, and it must not come back.
    std::fs::remove_file(restart_lock(&repo)).unwrap();
    let status2 = travsr(&repo, &lang_toml)
        .args(["status"])
        .output()
        .unwrap();
    assert!(status2.status.success(), "{status2:?}");
    assert!(
        !restart_lock(&repo).exists(),
        "a current-version daemon must not be restarted again"
    );
}

#[test]
fn a_current_daemon_is_left_running() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, lang_toml) = indexed_repo(tmp.path());
    let _stop = StopDaemon(&repo, &lang_toml);

    // A daemon of this binary's own version (no override).
    let start = travsr(&repo, &lang_toml)
        .args(["daemon", "start"])
        .output()
        .unwrap();
    assert!(start.status.success(), "{start:?}");
    assert!(wait_running(&repo, &lang_toml), "the daemon must come up");

    // A routed command must not disturb a matching daemon.
    let status = travsr(&repo, &lang_toml)
        .args(["status"])
        .output()
        .unwrap();
    assert!(status.status.success(), "{status:?}");
    assert!(
        !restart_lock(&repo).exists(),
        "a matching daemon must be reused, not restarted"
    );
    assert!(
        wait_running(&repo, &lang_toml),
        "the daemon must still be running"
    );
}
