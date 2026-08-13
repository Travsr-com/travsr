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
        .current_dir(tmp.path())
        .arg("init")
        .assert()
        .success();

    tmp
}

/// A git repo with more than 20 duplicate definitions of processPayment.
fn init_large_ambiguous_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    git_init(tmp.path());

    for i in 1..=22 {
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
        .current_dir(tmp.path())
        .arg("init")
        .assert()
        .success();

    tmp
}

#[test]
fn test_graph_cli_ambiguous_2_to_20() {
    let repo = init_repo();
    let assert = Command::cargo_bin("travsr")
        .unwrap()
        .current_dir(repo.path())
        .args(["graph", "processPayment"])
        .assert()
        .failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(stderr.contains(
        "'processPayment' is ambiguous — 2 definitions. Re-run with a `--path` hint to pick one:"
    ));
    assert!(stderr.contains("fn:processPayment (function) — file_a.ts:1"));
    assert!(stderr.contains("fn:processPayment (function) — file_b.ts:1"));
    assert!(stderr.contains("ambiguous symbol query"));
}

#[test]
fn test_graph_cli_ambiguous_more_than_20() {
    let repo = init_large_ambiguous_repo();
    let assert = Command::cargo_bin("travsr")
        .unwrap()
        .current_dir(repo.path())
        .args(["graph", "processPayment"])
        .assert()
        .failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(stderr.contains(
        "'processPayment' is ambiguous — 22 definitions. Re-run with a `--path` hint to pick one:"
    ));
    // Count how many definitions are printed
    let fn_lines_count = stderr
        .lines()
        .filter(|l| l.contains("fn:processPayment (function) —"))
        .count();
    assert_eq!(
        fn_lines_count, 20,
        "should list exactly 20 definitions in stderr"
    );
    // Truncation line
    assert!(stderr.contains(
        "[truncated: showing 20 of 22 definitions — additional filtering/narrowing is required]"
    ));
    assert!(stderr.contains("ambiguous symbol query"));
}

#[test]
fn test_graph_cli_disambiguated_path() {
    let repo = init_repo();
    let assert = Command::cargo_bin("travsr")
        .unwrap()
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
        .current_dir(tmp.path())
        .arg("init")
        .assert()
        .success();

    let assert = Command::cargo_bin("travsr")
        .unwrap()
        .current_dir(tmp.path())
        .args(["graph", "processPayment", "--path", "file.ts"])
        .assert()
        .failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(stderr.contains(
        "'processPayment' is ambiguous — 2 definitions. Re-run with a `--path` hint to pick one:"
    ));
    assert!(stderr.contains("fn:processPayment (function) — subdir1/file.ts:1"));
    assert!(stderr.contains("fn:processPayment (function) — subdir2/file.ts:1"));
}
