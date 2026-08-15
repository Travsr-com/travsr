//! `travsr synonym` CLI end-to-end coverage (RFC-012 A2 F1).
//!
//! Store-level unit tests cover the SQL; these exercise the actual binary's
//! argument parsing and the add / set / remove / list command wiring against a
//! real `travsr init`-ed repo.

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

/// A git repo with one TS file, indexed by `travsr init`.
fn init_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    git_init(tmp.path());
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/ts-small/a.ts"),
        tmp.path().join("a.ts"),
    )
    .unwrap();
    Command::cargo_bin("travsr")
        .unwrap()
        .env("TRAVSR_DISABLE_REGISTRY", "1") // UX-017: don't pollute the real registry
        .current_dir(tmp.path())
        .arg("init")
        .assert()
        .success();
    tmp
}

fn synonym(dir: &Path, args: &[&str]) -> assert_cmd::assert::Assert {
    Command::cargo_bin("travsr")
        .unwrap()
        .env("TRAVSR_DISABLE_REGISTRY", "1") // UX-017: don't pollute the real registry
        .current_dir(dir)
        .arg("synonym")
        .args(args)
        .assert()
}

fn stdout(assert: assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&assert.success().get_output().stdout).into_owned()
}

#[test]
fn add_accepts_multiple_aliases() {
    let repo = init_repo();
    let out = stdout(synonym(
        repo.path(),
        &["add", "payment", "billing", "invoice"],
    ));
    assert!(
        out.contains("added: payment → billing, invoice"),
        "got: {out}"
    );

    let listed = stdout(synonym(repo.path(), &["list", "payment"]));
    assert!(listed.contains("payment → billing"), "got: {listed}");
    assert!(listed.contains("payment → invoice"), "got: {listed}");
}

#[test]
fn set_replaces_existing_aliases() {
    let repo = init_repo();
    synonym(repo.path(), &["add", "payment", "billing"]).success();
    let out = stdout(synonym(
        repo.path(),
        &["set", "payment", "charge", "transaction"],
    ));
    assert!(
        out.contains("set: payment → charge, transaction"),
        "got: {out}"
    );

    let listed = stdout(synonym(repo.path(), &["list", "payment"]));
    assert!(listed.contains("charge"), "got: {listed}");
    assert!(listed.contains("transaction"), "got: {listed}");
    assert!(
        !listed.contains("billing"),
        "set must drop the old alias: {listed}"
    );
}

#[test]
fn remove_single_alias_leaves_siblings() {
    let repo = init_repo();
    synonym(repo.path(), &["add", "payment", "billing", "invoice"]).success();
    let out = stdout(synonym(repo.path(), &["remove", "payment", "billing"]));
    assert!(out.contains("removed: payment → billing"), "got: {out}");

    let listed = stdout(synonym(repo.path(), &["list", "payment"]));
    assert!(!listed.contains("billing"), "got: {listed}");
    assert!(
        listed.contains("invoice"),
        "sibling alias must survive: {listed}"
    );
}

#[test]
fn remove_without_alias_clears_term() {
    let repo = init_repo();
    synonym(repo.path(), &["add", "payment", "billing", "invoice"]).success();
    let out = stdout(synonym(repo.path(), &["remove", "payment"]));
    assert!(
        out.contains("removed all aliases for: payment"),
        "got: {out}"
    );

    let listed = stdout(synonym(repo.path(), &["list", "payment"]));
    assert!(
        listed.contains("(no synonyms for payment)"),
        "got: {listed}"
    );
}

#[test]
fn list_filter_reports_empty_for_unknown_term() {
    let repo = init_repo();
    let listed = stdout(synonym(repo.path(), &["list", "nonexistent"]));
    assert!(
        listed.contains("(no synonyms for nonexistent)"),
        "got: {listed}"
    );
}

#[test]
fn synonym_outside_initialized_repo_errors() {
    let tmp = tempfile::tempdir().unwrap();
    git_init(tmp.path()); // git repo, but no `travsr init`
    let output = Command::cargo_bin("travsr")
        .unwrap()
        .env("TRAVSR_DISABLE_REGISTRY", "1") // UX-017: don't pollute the real registry
        .current_dir(tmp.path())
        .args(["synonym", "list"])
        .output()
        .unwrap();
    assert!(!output.status.success(), "synonym must fail before init");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("not initialized"),
        "error must mention 'not initialized', got: {combined}"
    );
}
