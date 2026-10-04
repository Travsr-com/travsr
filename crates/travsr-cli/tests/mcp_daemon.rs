//! Plan 4.10 / S8: `travsr mcp` starts the background daemon when none is
//! running, so the index keeps up after a reboot or a killed `init`, without
//! putting a byte of its own on the JSON-RPC stdout.
//!
//! One real-process test on purpose: the behaviour is a process starting
//! another process, which nothing short of running it can show.

use std::io::{BufRead as _, BufReader, Write as _};
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

#[test]
fn mcp_starts_the_daemon_and_keeps_stdout_clean() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let lang_toml = tmp.path().join("lang.toml");
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
    // Index with no daemon, so the one this test sees is the one `mcp` starts.
    let init = travsr(&repo, &lang_toml)
        .env("CI", "1")
        .args(["init", "--no-connect"])
        .output()
        .unwrap();
    assert!(init.status.success(), "{init:?}");
    let _stop = StopDaemon(&repo, &lang_toml);

    let mut child = travsr(&repo, &lang_toml)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2024-11-05","capabilities":{{}},"clientInfo":{{"name":"t","version":"0"}}}}}}"#
    )
    .unwrap();
    let mut first = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut first)
        .unwrap();
    let reply: serde_json::Value = serde_json::from_str(first.trim())
        .unwrap_or_else(|e| panic!("stdout must be JSON-RPC from the first byte ({e}): {first:?}"));
    assert_eq!(reply["id"], 1, "{reply}");
    assert!(reply["result"]["serverInfo"].is_object(), "{reply}");

    let deadline = Instant::now() + Duration::from_secs(15);
    let running = loop {
        let status = travsr(&repo, &lang_toml)
            .args(["daemon", "status"])
            .output()
            .unwrap();
        if String::from_utf8_lossy(&status.stdout).contains("running") {
            break true;
        }
        if Instant::now() > deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(250));
    };
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        running,
        "`travsr mcp` must start the daemon when none is running"
    );
}
