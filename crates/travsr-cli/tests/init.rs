use std::process::Command as StdCommand;

use assert_cmd::Command;
use travsr_store::SqliteStore;

fn git_init(dir: &std::path::Path) {
    StdCommand::new("git")
        .args(["-c", "init.defaultBranch=main", "init", "-q"])
        .current_dir(dir)
        .status()
        .expect("git init failed");
}

fn travsr_init(dir: &std::path::Path) -> assert_cmd::assert::Assert {
    Command::cargo_bin("travsr")
        .unwrap()
        .current_dir(dir)
        .arg("init")
        .assert()
}

fn travsr_status(dir: &std::path::Path) -> assert_cmd::assert::Assert {
    Command::cargo_bin("travsr")
        .unwrap()
        .current_dir(dir)
        .arg("status")
        .assert()
}

#[test]
fn init_creates_db_in_git_repo() {
    let tmp = tempfile::tempdir().unwrap();
    git_init(tmp.path());
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/ts-small/a.ts"),
        tmp.path().join("a.ts"),
    )
    .unwrap();

    travsr_init(tmp.path()).success();

    let db_path = tmp.path().join(".travsr/graph.db");
    assert!(db_path.exists(), ".travsr/graph.db must be created");

    let store = SqliteStore::open(&db_path).unwrap();
    assert!(
        store.node_count().unwrap() > 0,
        "graph must contain at least one node"
    );
}

#[test]
fn init_fails_outside_git_repo() {
    let tmp = tempfile::tempdir().unwrap();

    let output = Command::cargo_bin("travsr")
        .unwrap()
        .current_dir(tmp.path())
        .arg("init")
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "init must fail outside a git repo"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("not inside a git repository"),
        "error message must mention 'not inside a git repository', got: {combined}"
    );
}

#[test]
fn init_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    git_init(tmp.path());
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/ts-small/a.ts"),
        tmp.path().join("a.ts"),
    )
    .unwrap();

    travsr_init(tmp.path()).success();
    let db_path = tmp.path().join(".travsr/graph.db");
    let count_after_first = SqliteStore::open(&db_path).unwrap().node_count().unwrap();

    travsr_init(tmp.path()).success();
    let count_after_second = SqliteStore::open(&db_path).unwrap().node_count().unwrap();

    assert_eq!(
        count_after_first, count_after_second,
        "node count must be stable across repeated init runs"
    );
}

#[test]
fn init_skips_node_modules() {
    let tmp = tempfile::tempdir().unwrap();
    git_init(tmp.path());

    // A .gitignore that excludes node_modules is how the ignore crate knows to skip it.
    std::fs::write(tmp.path().join(".gitignore"), "node_modules/\n").unwrap();
    std::fs::create_dir_all(tmp.path().join("node_modules")).unwrap();
    std::fs::write(
        tmp.path().join("node_modules/index.ts"),
        "export const x = 1;",
    )
    .unwrap();

    travsr_init(tmp.path()).success();

    let store = SqliteStore::open(&tmp.path().join(".travsr/graph.db")).unwrap();
    assert_eq!(
        store.node_count().unwrap(),
        0,
        "node_modules must be excluded from indexing"
    );
}

#[test]
fn status_reports_counts_after_init() {
    let tmp = tempfile::tempdir().unwrap();
    git_init(tmp.path());
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/ts-small/a.ts"),
        tmp.path().join("a.ts"),
    )
    .unwrap();

    travsr_init(tmp.path()).success();

    let output = travsr_status(tmp.path()).success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(stdout.contains("nodes:"), "status must report node count");

    let nodes: u64 = stdout
        .split("nodes:")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.trim_end_matches('|').parse().ok())
        .expect("could not parse node count from status output");

    assert!(nodes > 0, "status must report at least one node after init");
}
