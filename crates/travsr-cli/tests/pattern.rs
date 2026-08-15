//! `travsr pattern` CLI end-to-end coverage (#517).
//!
//! Unit tests in `travsr-mcp::tools` cover `find_pattern`'s logic directly;
//! these exercise the actual binary's argument parsing and the no-match /
//! error-surfacing wiring in `travsr-cli::pattern::run` against a real
//! `travsr init`-ed repo.

use std::path::Path;
use std::process::Command as StdCommand;

use assert_cmd::Command;

fn git_init(dir: &Path) {
    for args in [
        &["-c", "init.defaultBranch=main", "init", "-q"][..],
        &["config", "user.email", "test@test.com"][..],
        &["config", "user.name", "Test"][..],
    ] {
        StdCommand::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("git command failed");
    }
}

/// A git repo with one TS file (containing a `hello()` call site), indexed by
/// `travsr init`.
fn init_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    git_init(tmp.path());
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/ts-small/a.ts"),
        tmp.path().join("a.ts"),
    )
    .unwrap();
    StdCommand::new("git")
        .args(["add", "-A"])
        .current_dir(tmp.path())
        .status()
        .expect("git add");
    StdCommand::new("git")
        .args(["commit", "-qm", "init"])
        .current_dir(tmp.path())
        .status()
        .expect("git commit");
    Command::cargo_bin("travsr")
        .unwrap()
        .env("TRAVSR_DISABLE_REGISTRY", "1") // UX-017: don't pollute the real registry
        .current_dir(tmp.path())
        .arg("init")
        .assert()
        .success();
    tmp
}

fn pattern_stdout(dir: &Path, args: &[&str]) -> String {
    let out = Command::cargo_bin("travsr")
        .unwrap()
        .env("TRAVSR_DISABLE_REGISTRY", "1") // UX-017: don't pollute the real registry
        .current_dir(dir)
        .arg("pattern")
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8_lossy(&out).into_owned()
}

/// D4: a zero-match search must print the friendly message, not silently
/// nothing (the message was unreachable before #517 — the envelope wrapper
/// made `output.trim().is_empty()` always false).
#[test]
fn no_match_prints_friendly_message() {
    let repo = init_repo();
    let out = pattern_stdout(repo.path(), &["zzznosuchsymbolxyz"]);
    assert!(
        out.contains("no matches for 'zzznosuchsymbolxyz'"),
        "got: {out}"
    );
}

/// D5: an unescaped `(` compiles fine under BRE (today) but is a fatal
/// unbalanced group under `-E` (POSIX ERE, #517's fix) — the CLI must exit 0
/// with an error naming `--fixed`, not silently return nothing.
#[test]
fn unescaped_paren_errors_and_names_fixed_flag() {
    let repo = init_repo();
    let out = pattern_stdout(repo.path(), &["hello("]);
    assert!(out.contains("pattern error"), "got: {out}");
    assert!(out.contains("--fixed"), "got: {out}");
}

/// DD-6: `--fixed` is the escape hatch for exactly the pattern that broke in
/// the test above.
#[test]
fn fixed_flag_finds_the_literal() {
    let repo = init_repo();
    let out = pattern_stdout(repo.path(), &["--fixed", "hello("]);
    assert!(!out.contains("pattern error"), "got: {out}");
    assert!(out.contains("hello()"), "got: {out}");
}
