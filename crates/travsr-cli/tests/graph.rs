//! `travsr graph` CLI end-to-end coverage for ambiguity and path resolution (issue #565 / RFC-002).

use assert_cmd::Command;
use std::path::Path;
use std::process::Command as StdCommand;

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

/// A git repo with duplicate definitions for a function name across multiple files.
fn init_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    git_init(tmp.path());

    let file_a_path = tmp.path().join("file_a.ts");
    std::fs::write(
        &file_a_path,
        "function processPayment() {\n  return 1;\n}\n",
    )
    .unwrap();

    let file_b_path = tmp.path().join("file_b.ts");
    std::fs::write(
        &file_b_path,
        "function processPayment() {\n  return 2;\n}\n",
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

/// A git repo with `n` duplicate `processPayment` definitions across `n` files.
fn ambiguous_repo(n: usize) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    git_init(tmp.path());

    for i in 1..=n {
        let file_path = tmp.path().join(format!("file_{i}.ts"));
        std::fs::write(
            &file_path,
            format!("function processPayment() {{\n  return {i};\n}}\n"),
        )
        .unwrap();
    }

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

/// Number of rendered `fn:processPayment` definition lines in stderr.
fn definition_lines(stderr: &str) -> usize {
    stderr
        .lines()
        .filter(|l| l.contains("fn:processPayment (function) \u{2014}"))
        .count()
}

#[test]
fn test_graph_cli_ambiguous_2_to_20() {
    let repo = init_repo();
    let assert = Command::cargo_bin("travsr")
        .unwrap()
        .env("TRAVSR_DISABLE_REGISTRY", "1") // UX-017: don't pollute the real registry
        .current_dir(repo.path())
        .args(["graph", "processPayment"])
        .assert()
        .failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(stderr.contains(
        "'processPayment' is ambiguous, 2 definitions. Re-run with a `--path` hint to pick one:"
    ));
    assert!(stderr.contains("fn:processPayment (function) \u{2014} file_a.ts:1"));
    assert!(stderr.contains("fn:processPayment (function) \u{2014} file_b.ts:1"));
    assert!(stderr.contains("ambiguous symbol query"));
}

/// At exactly the display limit (20): the full list is shown with an exact
/// count and NO truncation notice.
#[test]
fn test_graph_cli_ambiguous_at_limit_20() {
    let repo = ambiguous_repo(20);
    let assert = Command::cargo_bin("travsr")
        .unwrap()
        .env("TRAVSR_DISABLE_REGISTRY", "1") // UX-017: don't pollute the real registry
        .current_dir(repo.path())
        .args(["graph", "processPayment"])
        .assert()
        .failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(stderr.contains(
        "'processPayment' is ambiguous, 20 definitions. Re-run with a `--path` hint to pick one:"
    ));
    assert_eq!(definition_lines(&stderr), 20);
    assert!(
        !stderr.contains("[truncated:"),
        "exactly-at-limit must not print a truncation notice: {stderr}"
    );
    assert!(
        !stderr.contains("at least"),
        "exactly-at-limit count is exact, not a lower bound: {stderr}"
    );
}

/// One over the display limit (21): only 20 are listed, the count reads as a
/// lower bound ("at least 21"), and the truncation notice fires.
#[test]
fn test_graph_cli_ambiguous_one_over_limit_21() {
    let repo = ambiguous_repo(21);
    let assert = Command::cargo_bin("travsr")
        .unwrap()
        .env("TRAVSR_DISABLE_REGISTRY", "1") // UX-017: don't pollute the real registry
        .current_dir(repo.path())
        .args(["graph", "processPayment"])
        .assert()
        .failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(stderr.contains(
        "'processPayment' is ambiguous, showing 20 of at least 21 definitions. \
         Re-run with a `--path` hint to pick one:"
    ));
    assert_eq!(definition_lines(&stderr), 20);
    assert!(stderr.contains("[truncated: additional filtering/narrowing is required]"));
    assert!(stderr.contains("ambiguous symbol query"));
}

/// Two over the display limit (22), via the bare-name Tier-2 path.
#[test]
fn test_graph_cli_ambiguous_more_than_20() {
    let repo = ambiguous_repo(22);
    let assert = Command::cargo_bin("travsr")
        .unwrap()
        .env("TRAVSR_DISABLE_REGISTRY", "1") // UX-017: don't pollute the real registry
        .current_dir(repo.path())
        .args(["graph", "processPayment"])
        .assert()
        .failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(stderr.contains(
        "'processPayment' is ambiguous, showing 20 of at least 22 definitions. \
         Re-run with a `--path` hint to pick one:"
    ));
    assert_eq!(definition_lines(&stderr), 20);
    assert!(stderr.contains("[truncated: additional filtering/narrowing is required]"));
    assert!(stderr.contains("ambiguous symbol query"));
}

/// Regression guard for the Tier-1 blocker: querying the FULL signature
/// (`fn:processPayment`) routes through `lookup_nodes_exact`, whose row cap used
/// to equal the display limit. With >20 same-signature definitions this must
/// still list 20, report the count as a lower bound, and fire the truncation
/// notice — not silently claim an exact "20 definitions" (#565 / RFC-002).
#[test]
fn test_graph_cli_ambiguous_full_signature_tier1() {
    let repo = ambiguous_repo(25);
    let assert = Command::cargo_bin("travsr")
        .unwrap()
        .env("TRAVSR_DISABLE_REGISTRY", "1") // UX-017: don't pollute the real registry
        .current_dir(repo.path())
        .args(["graph", "fn:processPayment"])
        .assert()
        .failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains("is ambiguous, showing 20 of at least 21 definitions."),
        "Tier-1 exact-signature path must report a truncated lower bound, got: {stderr}"
    );
    assert_eq!(definition_lines(&stderr), 20);
    assert!(stderr.contains("[truncated: additional filtering/narrowing is required]"));
}

/// `--format json` on an ambiguous query emits a machine-readable result on
/// stdout (still a non-zero exit) so an agent can read the candidates and pick a
/// `--path`, instead of getting human prose it cannot distinguish from a failure.
#[test]
fn test_graph_cli_ambiguous_json() {
    let repo = ambiguous_repo(3);
    let assert = Command::cargo_bin("travsr")
        .unwrap()
        .env("TRAVSR_DISABLE_REGISTRY", "1") // UX-017: don't pollute the real registry
        .current_dir(repo.path())
        .args(["graph", "processPayment", "--format", "json"])
        .assert()
        .failure();

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {stdout}"));
    assert_eq!(parsed["status"], "ambiguous");
    assert_eq!(parsed["count"], 3);
    assert_eq!(parsed["truncated"], false);
    let candidates = parsed["candidates"].as_array().expect("candidates array");
    assert_eq!(candidates.len(), 3);
    assert!(
        candidates
            .iter()
            .all(|c| c["signature"] == "fn:processPayment"),
        "each candidate carries its signature: {stdout}"
    );
}

/// File-name queries disambiguate too (#565 / RFC-002): two files sharing a
/// basename must list candidates instead of silently seeding the first, and a
/// `--path` resolves to the intended file.
#[test]
fn test_graph_cli_ambiguous_file_name() {
    let tmp = tempfile::tempdir().unwrap();
    git_init(tmp.path());
    for dir in ["pkg_a", "pkg_b"] {
        let d = tmp.path().join(dir);
        std::fs::create_dir(&d).unwrap();
        std::fs::write(d.join("service.ts"), "export const x = 1;\n").unwrap();
    }
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

    // Bare file name: ambiguous, both files listed, no arbitrary pick.
    let assert = Command::cargo_bin("travsr")
        .unwrap()
        .env("TRAVSR_DISABLE_REGISTRY", "1") // UX-017: don't pollute the real registry
        .current_dir(tmp.path())
        .args(["graph", "service.ts"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains("is ambiguous"),
        "expected ambiguity: {stderr}"
    );
    assert!(
        stderr.contains("pkg_a/service.ts"),
        "must list the first file: {stderr}"
    );
    assert!(
        stderr.contains("pkg_b/service.ts"),
        "must list the second file: {stderr}"
    );

    // A `--path` pin resolves to one file and renders.
    Command::cargo_bin("travsr")
        .unwrap()
        .env("TRAVSR_DISABLE_REGISTRY", "1") // UX-017: don't pollute the real registry
        .current_dir(tmp.path())
        .args(["graph", "service.ts", "--path", "pkg_a/service.ts"])
        .assert()
        .success();
}

#[test]
fn test_graph_cli_disambiguated_path() {
    let repo = init_repo();
    let assert = Command::cargo_bin("travsr")
        .unwrap()
        .env("TRAVSR_DISABLE_REGISTRY", "1") // UX-017: don't pollute the real registry
        .current_dir(repo.path())
        .args(["graph", "processPayment", "--path", "file_b.ts"])
        .assert()
        .success();

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(stdout.contains("fn:processPayment (function)"));
}

#[test]
fn test_graph_cli_invalid_path() {
    let repo = init_repo();
    let assert = Command::cargo_bin("travsr")
        .unwrap()
        .env("TRAVSR_DISABLE_REGISTRY", "1") // UX-017: don't pollute the real registry
        .current_dir(repo.path())
        .args(["graph", "processPayment", "--path", "nonexistent.ts"])
        .assert()
        .failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(stderr
        .contains("no matching definition found for 'processPayment' in path 'nonexistent.ts'"));
}

#[test]
fn test_graph_cli_still_ambiguous_path() {
    let tmp = tempfile::tempdir().unwrap();
    git_init(tmp.path());

    let dir1 = tmp.path().join("subdir1");
    std::fs::create_dir(&dir1).unwrap();
    std::fs::write(
        dir1.join("file.ts"),
        "function processPayment() {\n  return 1;\n}\n",
    )
    .unwrap();

    let dir2 = tmp.path().join("subdir2");
    std::fs::create_dir(&dir2).unwrap();
    std::fs::write(
        dir2.join("file.ts"),
        "function processPayment() {\n  return 2;\n}\n",
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

    let assert = Command::cargo_bin("travsr")
        .unwrap()
        .env("TRAVSR_DISABLE_REGISTRY", "1") // UX-017: don't pollute the real registry
        .current_dir(tmp.path())
        .args(["graph", "processPayment", "--path", "file.ts"])
        .assert()
        .failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(stderr.contains(
        "'processPayment' is ambiguous, 2 definitions. Re-run with a `--path` hint to pick one:"
    ));
    assert!(stderr.contains("fn:processPayment (function) \u{2014} subdir1/file.ts:1"));
    assert!(stderr.contains("fn:processPayment (function) \u{2014} subdir2/file.ts:1"));
}
