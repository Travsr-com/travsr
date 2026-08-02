// L2: `travsr embed gc` reclaims embed.db rows and HNSW index files for
// embedding models that are not the repo's active model (and not named with
// --keep). Dry-run by default; opt-in reclamation only (see the plan's
// reasoning for why this is not automatic on `embed switch`).

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command as StdCommand;

use assert_cmd::Command;

fn git_init(dir: &Path) {
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

/// A git repo with just enough `.travsr/` state for `cmd_gc`: a stub
/// `graph.db` (never opened by `cmd_gc` itself) and a repo-level embed config
/// naming the active model.
fn indexed_repo(dir: &Path, active_model: &str) {
    git_init(dir);
    std::fs::create_dir_all(dir.join(".travsr")).unwrap();
    std::fs::write(dir.join(".travsr/graph.db"), b"").unwrap();
    std::fs::write(
        dir.join(".travsr/embed.toml"),
        format!("active = \"{active_model}\"\n"),
    )
    .unwrap();
}

fn write_embed_db(path: &Path, rows: &[(&str, u64)]) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE node_embeddings (
             node_id   INTEGER NOT NULL,
             model_id  TEXT    NOT NULL,
             embedding BLOB    NOT NULL,
             text_hash TEXT,
             PRIMARY KEY (node_id, model_id)
         ) WITHOUT ROWID;",
    )
    .unwrap();
    let blob = vec![0u8; 8];
    let mut node_id = 0i64;
    for (model_id, count) in rows {
        for _ in 0..*count {
            node_id += 1;
            conn.execute(
                "INSERT INTO node_embeddings (node_id, model_id, embedding) VALUES (?1, ?2, ?3)",
                rusqlite::params![node_id, model_id, blob],
            )
            .unwrap();
        }
    }
}

fn model_row_counts(embed_db: &Path) -> BTreeMap<String, i64> {
    let conn = rusqlite::Connection::open(embed_db).unwrap();
    let mut stmt = conn
        .prepare("SELECT model_id, COUNT(*) FROM node_embeddings GROUP BY model_id")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

fn stdout_of(assert: assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&assert.get_output().stdout).into_owned()
}

#[test]
fn gc_dry_run_changes_nothing() {
    let repo = tempfile::tempdir().unwrap();
    indexed_repo(repo.path(), "arctic-embed-m-v1.5");
    let embed_db = repo.path().join(".travsr/embed.db");
    write_embed_db(
        &embed_db,
        &[("arctic-embed-m-v1.5", 5), ("bge-large-en-v1.5", 3)],
    );
    let before = model_row_counts(&embed_db);

    let out = stdout_of(
        Command::cargo_bin("travsr")
            .unwrap()
            .current_dir(repo.path())
            .args(["embed", "gc"])
            .assert()
            .success(),
    );
    assert!(out.contains("Would reclaim"), "got: {out}");
    assert!(out.contains("--apply"), "got: {out}");

    assert_eq!(
        model_row_counts(&embed_db),
        before,
        "dry run must not change row counts"
    );
}

#[test]
fn gc_never_deletes_the_active_model() {
    let repo = tempfile::tempdir().unwrap();
    indexed_repo(repo.path(), "arctic-embed-m-v1.5");
    let embed_db = repo.path().join(".travsr/embed.db");
    write_embed_db(
        &embed_db,
        &[("arctic-embed-m-v1.5", 5), ("bge-large-en-v1.5", 3)],
    );

    Command::cargo_bin("travsr")
        .unwrap()
        .current_dir(repo.path())
        .args(["embed", "gc", "--apply"])
        .assert()
        .success();

    let after = model_row_counts(&embed_db);
    assert_eq!(after.get("arctic-embed-m-v1.5"), Some(&5));
    assert_eq!(after.get("bge-large-en-v1.5"), None);
}

#[test]
fn gc_with_keep_retains_the_named_model() {
    let repo = tempfile::tempdir().unwrap();
    indexed_repo(repo.path(), "arctic-embed-m-v1.5");
    let embed_db = repo.path().join(".travsr/embed.db");
    write_embed_db(
        &embed_db,
        &[
            ("arctic-embed-m-v1.5", 5),
            ("bge-large-en-v1.5", 3),
            ("bge-small-en-v1.5", 2),
        ],
    );

    Command::cargo_bin("travsr")
        .unwrap()
        .current_dir(repo.path())
        .args(["embed", "gc", "--apply", "--keep", "bge-large-en-v1.5"])
        .assert()
        .success();

    let after = model_row_counts(&embed_db);
    assert_eq!(after.get("arctic-embed-m-v1.5"), Some(&5));
    assert_eq!(after.get("bge-large-en-v1.5"), Some(&3));
    assert_eq!(after.get("bge-small-en-v1.5"), None);
}

#[test]
fn gc_is_idempotent() {
    let repo = tempfile::tempdir().unwrap();
    indexed_repo(repo.path(), "arctic-embed-m-v1.5");
    let embed_db = repo.path().join(".travsr/embed.db");
    write_embed_db(
        &embed_db,
        &[("arctic-embed-m-v1.5", 5), ("bge-large-en-v1.5", 3)],
    );

    Command::cargo_bin("travsr")
        .unwrap()
        .current_dir(repo.path())
        .args(["embed", "gc", "--apply"])
        .assert()
        .success();
    let after_first = model_row_counts(&embed_db);

    let out = stdout_of(
        Command::cargo_bin("travsr")
            .unwrap()
            .current_dir(repo.path())
            .args(["embed", "gc", "--apply"])
            .assert()
            .success(),
    );
    assert!(out.contains("Nothing to reclaim"), "got: {out}");

    assert_eq!(
        model_row_counts(&embed_db),
        after_first,
        "second run must delete nothing further"
    );
}

#[test]
fn gc_removes_orphaned_hnsw_files_for_reclaimed_models() {
    let repo = tempfile::tempdir().unwrap();
    indexed_repo(repo.path(), "arctic-embed-m-v1.5");
    let embed_db = repo.path().join(".travsr/embed.db");
    write_embed_db(
        &embed_db,
        &[("arctic-embed-m-v1.5", 5), ("bge-large-en-v1.5", 3)],
    );

    let travsr_dir = repo.path().join(".travsr");
    std::fs::write(travsr_dir.join("arctic-embed-m-v1.5.hnsw.usearch"), b"x").unwrap();
    std::fs::write(travsr_dir.join("bge-large-en-v1.5.hnsw.usearch"), b"x").unwrap();
    std::fs::write(travsr_dir.join("bge-large-en-v1.5-docs.hnsw.usearch"), b"x").unwrap();

    Command::cargo_bin("travsr")
        .unwrap()
        .current_dir(repo.path())
        .args(["embed", "gc", "--apply"])
        .assert()
        .success();

    assert!(
        travsr_dir.join("arctic-embed-m-v1.5.hnsw.usearch").exists(),
        "active model's index must survive"
    );
    assert!(!travsr_dir.join("bge-large-en-v1.5.hnsw.usearch").exists());
    assert!(!travsr_dir
        .join("bge-large-en-v1.5-docs.hnsw.usearch")
        .exists());
}

/// The keep-set is resolved from *config*; what gets deleted is decided by
/// what is in *embed.db*. They diverge the moment a user runs `embed switch`
/// (which prints `Re-embed` and `Reclaim` two lines apart) and then reaches
/// for the second hint before the first. At that point the active model has
/// zero vectors, so "keep only the active model" means "keep nothing" — this
/// must never be a silent success.
#[test]
fn gc_refuses_when_the_active_model_has_no_embeddings_of_its_own() {
    let repo = tempfile::tempdir().unwrap();
    // Switched to arctic, not yet re-embedded: every row still belongs to bge.
    indexed_repo(repo.path(), "arctic-embed-m-v1.5");
    let embed_db = repo.path().join(".travsr/embed.db");
    write_embed_db(&embed_db, &[("bge-large-en-v1.5", 3)]);
    let before = model_row_counts(&embed_db);

    let assert = Command::cargo_bin("travsr")
        .unwrap()
        .current_dir(repo.path())
        .args(["embed", "gc", "--apply"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();

    assert!(
        stderr.contains("every vector in the repo"),
        "the refusal must say what was at stake, got: {stderr}"
    );
    assert!(
        stderr.contains("travsr embed reindex"),
        "the refusal must name the way out, got: {stderr}"
    );
    assert_eq!(
        model_row_counts(&embed_db),
        before,
        "nothing may be deleted when the sweep would empty the repo"
    );
}

/// Same shape, dry-run: a `Would reclaim: <everything>` preview is a promise
/// the command must not be willing to keep.
#[test]
fn gc_dry_run_also_refuses_a_whole_repo_sweep() {
    let repo = tempfile::tempdir().unwrap();
    indexed_repo(repo.path(), "arctic-embed-m-v1.5");
    write_embed_db(
        &repo.path().join(".travsr/embed.db"),
        &[("bge-large-en-v1.5", 3)],
    );

    Command::cargo_bin("travsr")
        .unwrap()
        .current_dir(repo.path())
        .args(["embed", "gc"])
        .assert()
        .failure();
}

/// `--keep` is the escape hatch: naming a model that actually has rows is an
/// explicit statement about what to preserve, so the guard steps aside.
#[test]
fn gc_keep_clears_the_whole_repo_sweep_guard() {
    let repo = tempfile::tempdir().unwrap();
    indexed_repo(repo.path(), "arctic-embed-m-v1.5");
    let embed_db = repo.path().join(".travsr/embed.db");
    write_embed_db(
        &embed_db,
        &[("bge-large-en-v1.5", 3), ("bge-small-en-v1.5", 2)],
    );

    Command::cargo_bin("travsr")
        .unwrap()
        .current_dir(repo.path())
        .args(["embed", "gc", "--apply", "--keep", "bge-large-en-v1.5"])
        .assert()
        .success();

    let after = model_row_counts(&embed_db);
    assert_eq!(after.get("bge-large-en-v1.5"), Some(&3));
    assert_eq!(after.get("bge-small-en-v1.5"), None);
}
