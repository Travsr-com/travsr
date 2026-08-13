use std::process::Command as StdCommand;

use assert_cmd::Command;
use travsr_store::SqliteStore;

fn git_init(dir: &std::path::Path) {
    StdCommand::new("git")
        .args(["-c", "init.defaultBranch=main", "init", "-q"])
        .current_dir(dir)
        .status()
        .expect("git init failed");
    StdCommand::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(dir)
        .status()
        .expect("git config email failed");
    StdCommand::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(dir)
        .status()
        .expect("git config name failed");
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

fn git(dir: &std::path::Path, args: &[&str]) {
    let status = StdCommand::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap_or_else(|_| panic!("git {args:?} failed to spawn"));
    assert!(status.success(), "git {args:?} exited non-zero");
}

/// TEST-1: covers `travsr hook-run --from-hook` end-to-end. This is the path
/// every developer commit takes after the security fix in hook.rs, so it must
/// have explicit coverage.
#[test]
fn hook_run_from_hook_reindexes_changed_file() {
    let tmp = tempfile::tempdir().unwrap();
    git_init(tmp.path());

    // Initial commit + travsr init so a baseline graph exists.
    std::fs::write(tmp.path().join("svc.ts"), "export function foo() {}").unwrap();
    git(tmp.path(), &["add", "svc.ts"]);
    git(tmp.path(), &["commit", "-q", "-m", "initial"]);
    travsr_init(tmp.path()).success();

    let db_path = tmp.path().join(".travsr/graph.db");
    let count_before = SqliteStore::open(&db_path).unwrap().node_count().unwrap();
    assert!(count_before > 0, "initial graph must have nodes");

    // Second commit adds a new function; --from-hook must pick it up via
    // `git diff-tree HEAD` and reindex svc.ts.
    std::fs::write(
        tmp.path().join("svc.ts"),
        "export function foo() {}\nexport function bar() {}",
    )
    .unwrap();
    git(tmp.path(), &["add", "svc.ts"]);
    git(tmp.path(), &["commit", "-q", "-m", "add bar"]);

    Command::cargo_bin("travsr")
        .unwrap()
        .current_dir(tmp.path())
        .args(["hook-run", "--from-hook"])
        .assert()
        .success();

    let count_after = SqliteStore::open(&db_path).unwrap().node_count().unwrap();
    assert!(
        count_after > count_before,
        "hook-run --from-hook must reindex the changed file and add nodes for bar() \
         (before={count_before}, after={count_after})"
    );
}

/// Item C (dogfooding): `travsr init` must install a post-commit hook that
/// `exec`s the SAME binary that ran init (an absolute path), never bare
/// `travsr`. On a dogfooding box a bare hook resolves to the npm-global wrapper
/// (often stale) and reindexes a fresh daemon's graph with old behavior.
#[test]
fn init_hook_pins_installing_binary_not_bare_travsr() {
    let tmp = tempfile::tempdir().unwrap();
    git_init(tmp.path());
    std::fs::write(tmp.path().join("svc.ts"), "export function foo() {}").unwrap();
    git(tmp.path(), &["add", "svc.ts"]);
    git(tmp.path(), &["commit", "-q", "-m", "initial"]);

    travsr_init(tmp.path()).success();

    let hook = tmp.path().join(".git/hooks/post-commit");
    let body = std::fs::read_to_string(&hook).expect("post-commit hook must be installed");

    let bin = assert_cmd::cargo::cargo_bin("travsr");
    let bin = bin.to_str().unwrap();
    assert!(
        body.contains(bin),
        "hook must embed the installing binary's absolute path ({bin}), got:\n{body}"
    );
    assert!(body.contains("hook-run --from-hook"));
    assert!(
        !body.contains("exec travsr hook-run"),
        "hook must not invoke bare `travsr`, got:\n{body}"
    );
}

/// BUG-1 regression: `git diff-tree HEAD` on a merge commit returns nothing
/// without `--first-parent`. This test creates a branch, modifies a file, and
/// merges back — the resulting merge commit's hook-run must still reindex the
/// merged file.
#[test]
fn hook_run_from_hook_reindexes_after_merge_commit() {
    let tmp = tempfile::tempdir().unwrap();
    git_init(tmp.path());

    // Baseline on main.
    std::fs::write(tmp.path().join("svc.ts"), "export function foo() {}").unwrap();
    git(tmp.path(), &["add", "svc.ts"]);
    git(tmp.path(), &["commit", "-q", "-m", "initial"]);
    travsr_init(tmp.path()).success();

    let db_path = tmp.path().join(".travsr/graph.db");
    let count_before = SqliteStore::open(&db_path).unwrap().node_count().unwrap();

    // Create a feature branch with a new function, then merge back to main
    // with --no-ff to force a merge commit (so we exercise the multi-parent path).
    git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
    std::fs::write(
        tmp.path().join("svc.ts"),
        "export function foo() {}\nexport function bar() {}",
    )
    .unwrap();
    git(tmp.path(), &["add", "svc.ts"]);
    git(tmp.path(), &["commit", "-q", "-m", "add bar on feature"]);
    git(tmp.path(), &["checkout", "-q", "main"]);
    git(
        tmp.path(),
        &["merge", "--no-ff", "-q", "-m", "merge feature", "feature"],
    );

    // HEAD is now a merge commit with two parents. --from-hook must still
    // see svc.ts and reindex it.
    Command::cargo_bin("travsr")
        .unwrap()
        .current_dir(tmp.path())
        .args(["hook-run", "--from-hook"])
        .assert()
        .success();

    let count_after = SqliteStore::open(&db_path).unwrap().node_count().unwrap();
    assert!(
        count_after > count_before,
        "hook-run --from-hook must reindex files brought in by a merge commit \
         (--first-parent semantics; before={count_before}, after={count_after})"
    );
}
