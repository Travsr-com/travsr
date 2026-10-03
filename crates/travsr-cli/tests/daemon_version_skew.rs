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

/// The PID the running daemon recorded in its lock file. `None` during the brief
/// open→write window, so callers poll.
fn read_pid(repo: &Path) -> Option<u32> {
    std::fs::read_to_string(repo.join(".travsr").join("daemon.lock"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn wait_for_pid(repo: &Path, deadline: Instant) -> Option<u32> {
    loop {
        if let Some(p) = read_pid(repo) {
            return Some(p);
        }
        if Instant::now() > deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
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
    let deadline = Instant::now() + Duration::from_secs(20);
    let pid_old = wait_for_pid(&repo, deadline).expect("the stale daemon must record a PID");

    // Any daemon-routed command, run by THIS binary (no override), detects the
    // skew and restarts the daemon before answering.
    let status = travsr(&repo, &lang_toml).args(["status"]).output().unwrap();
    assert!(status.status.success(), "{status:?}");

    let deadline = Instant::now() + Duration::from_secs(20);
    let pid_new = loop {
        if let Some(p) = read_pid(&repo) {
            if p != pid_old {
                break p;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the stale daemon (pid {pid_old}) was never replaced"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_ne!(pid_new, pid_old, "a skewed daemon must be restarted");

    // The replacement is this binary's version, so a second command leaves it
    // alone — no restart loop.
    let status2 = travsr(&repo, &lang_toml).args(["status"]).output().unwrap();
    assert!(status2.status.success(), "{status2:?}");
    assert_eq!(
        read_pid(&repo),
        Some(pid_new),
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
    let deadline = Instant::now() + Duration::from_secs(20);
    let pid = wait_for_pid(&repo, deadline).expect("the daemon must record a PID");

    // A routed command must not disturb a matching daemon.
    let status = travsr(&repo, &lang_toml).args(["status"]).output().unwrap();
    assert!(status.status.success(), "{status:?}");
    assert_eq!(
        read_pid(&repo),
        Some(pid),
        "a matching daemon must be reused, not restarted"
    );
}
