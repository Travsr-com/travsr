//! #741: the semantic marker must return to a clean state after `init` runs
//! Phase B, and must keep doing so across commits.
//!
//! `travsr status` derives `semantic:` from three meta keys. `phase_b_commit ==
//! last_commit` with `phase_b_dirty = 1` renders as
//! `stale (run travsr init --semantic to refresh)`.
//!
//! The flag is set by `reindex_files`, because rewriting a file's Phase A nodes
//! drops its `ref/call` edges (#583). Init's own indexing does not route through
//! `reindex_files`, so in the field the flag arrives from the watcher or a
//! commit hook before the user ever runs `init`, and the tests below have to
//! stamp it deliberately (see `mark_dirty`).
//!
//! The daemon's own Phase B path clears the flag when it advances
//! `phase_b_commit`; the init path stamped the commit and left the flag set, so
//! the state was permanent: nothing else cleared it, and the command the status
//! message named did not either. The graph was correct throughout, which makes
//! this a status-honesty bug rather than real staleness.
//!
//! Both halves are asserted, since fixing the flag by suppressing the reindex
//! would be a regression in the other direction. Only `--semantic` clears it:
//! plain `init` defers Phase B, so the edges really are missing there and the
//! flag is honest, which is why the remedy names `--semantic`.

use std::path::Path;
use std::process::Command as StdCommand;

fn git(dir: &Path, args: &[&str]) {
    let ok = StdCommand::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("git must be runnable")
        .success();
    assert!(ok, "git {args:?} failed");
}

fn git_init(dir: &Path) {
    git(dir, &["-c", "init.defaultBranch=main", "init", "-q"]);
    git(dir, &["config", "user.email", "qa@travsr.test"]);
    git(dir, &["config", "user.name", "QA Bot"]);
}

fn commit_all(dir: &Path, message: &str) {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", message]);
}

fn meta(db: &Path, key: &str) -> Option<String> {
    let store = travsr_store::SqliteStore::open(db).expect("open graph.db");
    store.get_meta(key).expect("read meta")
}

/// Whether `travsr status` would call the semantic layer dirty.
///
/// Asserted on this rather than on the literal stored string, because only
/// `"1"` means dirty: an absent key reads as clean (`query.rs` compares against
/// `Some("1")`). An earlier version of this file asserted `Some("0")` exactly,
/// which failed on an absent key that was already functionally clean, so it
/// reported a bug that was not there while not testing the clearing at all.
fn reads_as_dirty(db: &Path) -> bool {
    meta(db, "phase_b_dirty").as_deref() == Some("1")
}

/// Set the flag the way the watcher and reindex paths do (#583).
///
/// The test has to arrange this explicitly. Init's own indexing does not route
/// through `reindex_files`, so without setting it here there is nothing for init
/// to clear and the assertion is vacuous. In the field the flag arrives via the
/// watcher or a hook-driven reindex before the user runs `travsr init`.
fn mark_dirty(db: &Path) {
    let mut store = travsr_store::SqliteStore::open(db).expect("open graph.db");
    store
        .set_meta("phase_b_dirty", "1")
        .expect("set phase_b_dirty");
}

/// `travsr init --semantic`: Phase B runs inline, which is the path that stamps
/// `phase_b_commit`. Plain `init_repo` defers Phase B to the daemon and so never
/// reaches the code under test. An earlier version of this file used
/// `init_repo`, and both tests passed with the fix reverted because their
/// assertions were guarded behind a marker that was never stamped.
/// `init_repo_with_progress` registers the repo unless this is set, so without
/// it these tests append tempdir paths to the developer's real
/// `~/.travsr/registry.json`, which `travsr repos` and `travsr mcp --global`
/// then list after `tempfile` has deleted the directories.
///
/// Set from every test rather than once, because `set_var` is process-global and
/// these run in parallel; writing the same value from each is safe. This is the
/// pattern the travsr-cli integration tests already use.
fn disable_registry() {
    std::env::set_var("TRAVSR_DISABLE_REGISTRY", "1");
}

fn init_semantic(root: &Path) {
    disable_registry();
    travsr_daemon::init_repo_with_progress(root, None, true, false, &mut |_| {})
        .expect("init_repo_with_progress(semantic = true)");
}

/// `init_repo` must leave the semantic layer described as clean, not dirty.
#[test]
fn init_clears_phase_b_dirty_after_running_phase_b() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git_init(root);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/m.py"),
        "def add(a, b):\n    return a + b\n\n\ndef run():\n    return add(1, 2)\n",
    )
    .unwrap();
    commit_all(root, "seed");

    init_semantic(root);
    let db = root.join(".travsr/graph.db");

    // Arrange the degraded state the flag exists to describe, then let init's
    // Phase B rebuild the edges it is warning about.
    mark_dirty(&db);
    assert!(
        reads_as_dirty(&db),
        "arrangement failed: the flag is not set"
    );

    init_semantic(root);

    let pb = meta(&db, "phase_b_commit");
    let last = meta(&db, "last_commit");
    assert!(
        pb.is_some() && pb == last,
        "init --semantic must stamp phase_b_commit at HEAD, otherwise the \
         assertion below is vacuous (pb={pb:?} last={last:?})"
    );
    assert!(
        !reads_as_dirty(&db),
        "init stamped phase_b_commit at HEAD but left phase_b_dirty set, which \
         renders as `stale (run travsr init to refresh)` even though Phase B just \
         rebuilt the edges the flag is warning about"
    );
}

/// The state must survive a commit and a reindex, which is the shape a user hits.
/// A one-shot check would pass even if the flag were only cleared on a first run.
#[test]
fn the_marker_recovers_after_a_source_commit_and_reindex() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git_init(root);
    std::fs::create_dir_all(root.join("src")).unwrap();
    let src = root.join("src/m.py");
    std::fs::write(
        &src,
        "def add(a, b):\n    return a + b\n\n\ndef run():\n    return add(1, 2)\n",
    )
    .unwrap();
    commit_all(root, "seed");

    init_semantic(root);
    let db = root.join(".travsr/graph.db");
    assert!(
        meta(&db, "phase_b_commit").is_some(),
        "init --semantic must stamp phase_b_commit, otherwise this test proves nothing"
    );

    // Arrange the flag, as the watcher or a hook-driven reindex would.
    mark_dirty(&db);

    // Edit and commit, exactly as a user would.
    std::fs::write(
        &src,
        "def add(a, b):\n    return a + b\n\n\ndef run():\n    return add(1, 2)\n\n\ndef mul(a, b):\n    return a * b\n",
    )
    .unwrap();
    commit_all(root, "add mul");

    init_semantic(root);

    let last = meta(&db, "last_commit");
    let pb = meta(&db, "phase_b_commit");
    assert_eq!(
        pb, last,
        "phase_b_commit must catch up to HEAD after a reindex"
    );
    assert!(
        !reads_as_dirty(&db),
        "the marker must return to a clean state after a commit and a reindex, \
         not sit on `stale` with `travsr init` as its own unworkable remedy"
    );

    // The data half. If the flag were cleared by skipping the Phase A reindex,
    // the flag would look right and the graph would be wrong, which is worse than
    // the bug being fixed.
    let store = travsr_store::SqliteStore::open(&db).unwrap();
    let found = store
        .search_nodes_by_name("mul")
        .expect("search_nodes_by_name")
        .into_iter()
        .any(|n| n.vname.signature.contains("mul"));
    assert!(
        found,
        "the symbol committed before the reindex must be in the graph; a clean \
         marker over a stale graph is a worse failure than the one being fixed"
    );
}

/// Plain `travsr init` must NOT clear the flag, and `status` must not tell users
/// to run it.
///
/// Review on #742 caught that both other tests here go through `--semantic`, so
/// neither covered the path `travsr status` actually named. `run_phase_b_inline`
/// is `semantic || !has_commit`, so on an already-committed repo plain init
/// re-runs Phase A and returns without rebuilding the `ref/call` edges the flag
/// reports as missing.
///
/// The flag staying set is correct: the edges are genuinely still missing, so
/// clearing it would be a lie. What was wrong was the remedy the message named,
/// which is asserted in `the_remediation_names_a_command_that_works`.
#[test]
fn plain_init_leaves_the_flag_set_because_it_does_not_run_phase_b() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git_init(root);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/m.py"),
        "def add(a, b):\n    return a + b\n\n\ndef run():\n    return add(1, 2)\n",
    )
    .unwrap();
    commit_all(root, "seed");

    init_semantic(root);
    let db = root.join(".travsr/graph.db");

    // The state a watcher or hook reindex leaves behind.
    mark_dirty(&db);
    assert!(
        reads_as_dirty(&db),
        "arrangement failed: the flag is not set"
    );

    // Plain init: semantic = false, and the repo has a HEAD commit.
    travsr_daemon::init_repo(root).expect("plain init_repo");

    assert!(
        reads_as_dirty(&db),
        "plain `travsr init` defers Phase B, so the ref/call edges are still \
         missing and the flag must stay set; clearing it here would report a \
         clean semantic layer over a degraded graph"
    );

    // And the documented remedy does clear it.
    init_semantic(root);
    assert!(
        !reads_as_dirty(&db),
        "`travsr init --semantic` must clear what plain init cannot"
    );
}

/// The remediation string must name a command that can actually clear the state
/// it appears in. Pinned as a string because the message is the whole product
/// surface for this bug: #741 was a dead-end loop precisely because the text
/// named `travsr init`, which cannot clear the flag on an already-committed repo.
#[test]
fn the_remediation_names_a_command_that_works() {
    let status_rs = include_str!("../../travsr-cli/src/status.rs");
    assert!(
        status_rs.contains("stale (run travsr init --semantic to refresh)"),
        "the stale message must name `travsr init --semantic`"
    );
    assert!(
        !status_rs.contains("stale (run travsr init to refresh)"),
        "the old message named plain `travsr init`, which defers Phase B and so \
         cannot clear the flag: running it returns the user to the same message"
    );
}

/// #742 review: a reindex that lands *while* Phase B is running must not have
/// its flag cleared by that Phase B.
///
/// `init.lock` serialises two `travsr init` invocations; it does not exclude the
/// watcher, which is a separate process with its own store. Init reads the file
/// set, the user saves a file, the watcher rewrites that file's Phase A nodes
/// and drops its `ref/call` edges, and init then finishes a pass that never saw
/// the edit and clears the flag anyway.
///
/// The bump has to land inside init's window, between it reading the counter and
/// it clearing the flag. Bumping beforehand simply becomes init's own baseline,
/// which is what my first version of this test did: it failed for that reason
/// rather than because the fix was missing. A bumper thread wins the race
/// reliably, since the window spans a whole Phase B pass.
///
/// Comparing the flag's *value* would not catch this at all: it is already "1"
/// in the case that matters, so before and after look identical.
#[test]
fn a_reindex_during_phase_b_keeps_the_flag_set() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git_init(root);
    std::fs::write(root.join("a.ts"), "export function a() { return 1; }\n").unwrap();
    commit_all(root, "init");

    let db = root.join(".travsr/graph.db");
    init_semantic(root);
    mark_dirty(&db);

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let bumper = {
        let stop = std::sync::Arc::clone(&stop);
        let db = db.clone();
        std::thread::spawn(move || {
            // Opened once and reused. Reopening per tick ran the migration
            // runner each time, which takes heavier locks than the write itself
            // and made init's bulk FTS insert fail with "database is locked" on
            // Windows. Only one bump has to land inside init's window, so a tick
            // that cannot get the lock is dropped rather than retried hard.
            let Ok(mut store) = travsr_store::SqliteStore::open(&db) else {
                return;
            };
            let mut next = read_dirty_seq(&mut store) + 1;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                if store
                    .set_meta("phase_b_dirty_seq", &next.to_string())
                    .is_ok()
                {
                    next += 1;
                }
                std::thread::sleep(std::time::Duration::from_millis(150));
            }
        })
    };

    init_semantic(root);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    bumper.join().expect("bumper thread");

    assert!(
        reads_as_dirty(&db),
        "a mark that arrived while Phase B was running must survive it: clearing \
         it reports `complete` over a file with no ref/call edges, and nothing \
         rearms until the next commit"
    );
}

/// Read the counter through a store that is already open.
fn read_dirty_seq(store: &mut travsr_store::SqliteStore) -> u64 {
    store
        .get_meta("phase_b_dirty_seq")
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}
