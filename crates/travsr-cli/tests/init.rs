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
        .env("TRAVSR_DISABLE_REGISTRY", "1") // UX-017: don't pollute the real registry
        .current_dir(dir)
        .arg("init")
        .assert()
}

fn travsr_status(dir: &std::path::Path) -> assert_cmd::assert::Assert {
    Command::cargo_bin("travsr")
        .unwrap()
        .env("TRAVSR_DISABLE_REGISTRY", "1") // UX-017: don't pollute the real registry
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
        .env("TRAVSR_DISABLE_REGISTRY", "1") // UX-017: don't pollute the real registry
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
        .env("TRAVSR_DISABLE_REGISTRY", "1") // UX-017: don't pollute the real registry
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
        .env("TRAVSR_DISABLE_REGISTRY", "1") // UX-017: don't pollute the real registry
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

// ── #811: `init --semantic --force` reconciles `ref_resolution_state` ─────────

/// The issue's own measurement, verbatim, against the on-disk database.
fn raw_pending_count(db_path: &std::path::Path) -> i64 {
    rusqlite::Connection::open(db_path)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM ref_resolution_state WHERE state = 'pending'",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

fn raw_has_row(db_path: &std::path::Path, src: travsr_core::NodeId, line: u32, name: &str) -> bool {
    rusqlite::Connection::open(db_path)
        .unwrap()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM ref_resolution_state \
             WHERE src = ?1 AND ref_line = ?2 AND name = ?3)",
            rusqlite::params![src.0 as i64, line as i64, name],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
        != 0
}

fn raw_edge_site_exists(db_path: &std::path::Path, src: travsr_core::NodeId, line: u32) -> bool {
    rusqlite::Connection::open(db_path)
        .unwrap()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM edge_sites WHERE src = ?1 AND line = ?2)",
            rusqlite::params![src.0 as i64, line as i64],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
        != 0
}

fn travsr_init_semantic(dir: &std::path::Path, force: bool) {
    let mut cmd = Command::cargo_bin("travsr").unwrap();
    cmd.env("TRAVSR_DISABLE_REGISTRY", "1") // UX-017: don't pollute the real registry
        .current_dir(dir)
        .args(["init", "--semantic"]);
    if force {
        cmd.arg("--force");
    }
    cmd.assert().success();
}

fn pending(src: travsr_core::NodeId, line: u32, name: &str) -> travsr_store::RefResolution {
    travsr_store::RefResolution {
        src,
        ref_line: line,
        ref_col: 0,
        name: name.to_string(),
        state: "pending",
        resolved_dst: None,
    }
}

/// #811 at the binary boundary: `travsr init --semantic --force` must leave
/// `ref_resolution_state` consistent with the graph it just rebuilt.
///
/// The daemon crate reproduces the issue through the live lane itself; here the
/// stale state is seeded the way a pre-fix session leaves it, so the test
/// exercises the shipped CLI rather than library entry points. `notify()` on
/// line 4 is a call the rebuilt graph resolves (the parameter shadows a unique
/// repo-wide function, which the live lane refuses but Phase B's resolver does
/// not), `frobnicate()` on line 5 resolves to nothing, and a third row hangs off
/// a node id that does not exist.
#[test]
fn init_semantic_force_reconciles_stale_pending_refs() {
    let tmp = tempfile::tempdir().unwrap();
    git_init(tmp.path());
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src/billing.ts"),
        "export class Billing {\n  charge(): void {}\n}\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("src/notify.ts"),
        "export function notify(): void {}\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("src/caller.ts"),
        "import { Billing } from \"./billing\";\n\
         export function run(bill: Billing, notify: () => void): void {\n\
         \x20 bill.charge();\n\
         \x20 notify();\n\
         \x20 frobnicate();\n\
         }\n",
    )
    .unwrap();
    git(tmp.path(), &["add", "-A"]);
    git(tmp.path(), &["commit", "-q", "-m", "seed"]);

    // A fresh, complete index.
    travsr_init_semantic(tmp.path(), false);
    let db_path = tmp.path().join(".travsr/graph.db");
    let (corpus, run) = {
        let store = SqliteStore::open(&db_path).unwrap();
        let corpus = store.get_meta("corpus").unwrap().unwrap_or_default();
        let run = store
            .enclosing_definition_at(&corpus, "src/caller.ts", 4)
            .unwrap()
            .expect("`run` must enclose line 4");
        (corpus, run)
    };
    assert!(
        raw_edge_site_exists(&db_path, run, 4),
        "precondition: Phase B must record a call site for `notify()`"
    );
    assert!(
        !raw_edge_site_exists(&db_path, run, 5),
        "precondition: nothing may resolve `frobnicate()`"
    );
    assert_eq!(
        raw_pending_count(&db_path),
        0,
        "precondition: a clean index"
    );

    // The state a pre-fix session leaves behind.
    let ghost = travsr_core::NodeId(0xDEAD_BEEF);
    {
        let mut store = SqliteStore::open(&db_path).unwrap();
        store
            .replace_ref_resolution_states(
                &corpus,
                "src/caller.ts",
                &[pending(run, 4, "notify"), pending(run, 5, "frobnicate")],
            )
            .unwrap();
        store
            .upsert_ref_resolution_states(&[pending(ghost, 1, "gone")])
            .unwrap();
    }
    assert_eq!(
        raw_pending_count(&db_path),
        3,
        "precondition: the stale state is in place"
    );
    let refs_before = ref_edge_count(&db_path);

    // The command the issue names.
    travsr_init_semantic(tmp.path(), true);

    // Graph state is correct...
    let store = SqliteStore::open(&db_path).unwrap();
    assert_eq!(
        store.get_meta("phase_b_commit").unwrap(),
        store.get_meta("last_commit").unwrap(),
        "Phase B must be current after --semantic"
    );
    assert_eq!(store.count_edges_with_provenance("live").unwrap(), 0);
    assert_eq!(
        ref_edge_count(&db_path),
        refs_before,
        "a rebuild of an unchanged tree must reproduce the same ref edges"
    );
    assert!(
        raw_edge_site_exists(&db_path, run, 4),
        "the rebuilt graph resolves line 4 again"
    );
    // ...and the table agrees with it.
    assert!(
        !raw_has_row(&db_path, run, 4, "notify"),
        "a pending row with a call site beside it must not survive `init --semantic --force` (#811)"
    );
    assert!(
        !raw_has_row(&db_path, ghost, 1, "gone"),
        "the orphan must be purged"
    );
    assert!(
        raw_has_row(&db_path, run, 5, "frobnicate"),
        "the genuine unresolved reference must remain pending"
    );
    assert_eq!(raw_pending_count(&db_path), 1);
    assert_eq!(store.pending_ref_count().unwrap(), 1);
    drop(store);

    // A second rebuild is stable: nothing reintroduced, nothing more removed.
    travsr_init_semantic(tmp.path(), true);
    assert_eq!(raw_pending_count(&db_path), 1);
    assert!(raw_has_row(&db_path, run, 5, "frobnicate"));
    assert_eq!(ref_edge_count(&db_path), refs_before);
}

/// The `ref/*` edges only. Repeated `--force` passes are not byte-stable on
/// Phase A's speculative import `resolves-to` edges (a pre-existing property of
/// the purge-and-restage path, unrelated to #811), so the graph invariant this
/// test holds the reconcile to is the semantic edge set it is keyed on.
fn ref_edge_count(db_path: &std::path::Path) -> usize {
    SqliteStore::open(db_path)
        .unwrap()
        .all_edges()
        .unwrap()
        .iter()
        .filter(|(_, _, kind, _)| kind.starts_with("ref/"))
        .count()
}
