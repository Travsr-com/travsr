//! #862: `travsr embed status` must report internally consistent raw counts.
//!
//! `embed_progress` used to count every `node_embeddings` row for the model as
//! `embedded` while `Total symbols` counted only embeddable nodes, so a vector
//! on an ineligible node (a `field` whose `embed_text` was cleared by a model
//! switch) produced `Total symbols : 9,996` beside `Embedded : 10,009`. The
//! percentage hid it, because `pct_display` clamps to 100. These tests read the
//! raw counts the command prints, never the percentage.

use std::path::Path;
use std::process::Command as StdCommand;

use assert_cmd::Command;
use travsr_core::{Node, VName};
use travsr_store::{SqliteStore, Store as _};

/// A backend that is in the catalog, so `embed status` gets past the install
/// check and prints the per-repo progress section. Whether its binary is
/// installed on this machine only changes the install lines, not the counts.
const MODEL: &str = "bge-small-en-v1.5";

fn git_init(dir: &Path) {
    for args in [
        vec!["-c", "init.defaultBranch=main", "init", "-q"],
        vec!["config", "user.email", "test@test.com"],
        vec!["config", "user.name", "Test"],
    ] {
        let ok = StdCommand::new("git")
            .args(&args)
            .current_dir(dir)
            .status()
            .expect("git must be runnable")
            .success();
        assert!(ok, "git {args:?} failed");
    }
}

fn node(kind: &str, sig: &str) -> Node {
    Node::new(VName::new("c", "", "src/lib.ts", "typescript", sig), kind)
}

/// A repo whose graph.db holds `eligible` function nodes and `ineligible`
/// `field` nodes with no `embed_text`, and whose embed.db holds one vector for
/// every one of them under `MODEL`. Returns the repo root.
fn repo_with_vectors(eligible: usize, ineligible: usize) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    git_init(tmp.path());
    std::fs::create_dir_all(tmp.path().join(".travsr")).unwrap();
    std::fs::write(
        tmp.path().join(".travsr/embed.toml"),
        format!("active = \"{MODEL}\"\n"),
    )
    .unwrap();

    let mut ids = Vec::new();
    {
        let mut store = SqliteStore::open(&tmp.path().join(".travsr/graph.db")).unwrap();
        store.set_meta("last_commit", "abc123").unwrap();
        store.set_meta("phase_b_commit", "abc123").unwrap();
        for i in 0..eligible {
            let n = node("function", &format!("fn:f{i}"));
            store.put_node(&n).unwrap();
            ids.push(n.id.0 as i64);
        }
        for i in 0..ineligible {
            let n = node("field", &format!("field:T.f{i}"));
            store.put_node(&n).unwrap();
            ids.push(n.id.0 as i64);
        }
    }

    let conn = rusqlite::Connection::open(tmp.path().join(".travsr/embed.db")).unwrap();
    conn.execute_batch(
        "CREATE TABLE node_embeddings (
             node_id   INTEGER NOT NULL,
             model_id  TEXT    NOT NULL,
             embedding BLOB    NOT NULL,
             text_hash TEXT,
             PRIMARY KEY (node_id, model_id)
         ) WITHOUT ROWID;
         CREATE INDEX idx_node_embeddings_model ON node_embeddings(model_id);",
    )
    .unwrap();
    for id in ids {
        conn.execute(
            "INSERT INTO node_embeddings (node_id, model_id, embedding) VALUES (?1, ?2, X'00')",
            rusqlite::params![id, MODEL],
        )
        .unwrap();
    }
    tmp
}

fn embed_status(dir: &Path) -> String {
    let out = Command::cargo_bin("travsr")
        .unwrap()
        .env("TRAVSR_DISABLE_REGISTRY", "1")
        .current_dir(dir)
        .args(["embed", "status"])
        .assert()
        .success();
    String::from_utf8_lossy(&out.get_output().stdout).into_owned()
}

/// The raw count on a `Label : 1,234` line, commas stripped.
fn count_after(stdout: &str, label: &str) -> u64 {
    let line = stdout
        .lines()
        .find(|l| l.trim_start().starts_with(label))
        .unwrap_or_else(|| panic!("no `{label}` line in:\n{stdout}"));
    let value = line.split(':').nth(1).expect("label : value");
    let digits: String = value
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == ',')
        .filter(|c| *c != ',')
        .collect();
    digits
        .parse()
        .unwrap_or_else(|_| panic!("unparseable count on `{line}`"))
}

/// The #862 shape: every eligible node embedded, plus vectors on ineligible
/// nodes. `Embedded` must equal `Total symbols`, not exceed it.
#[test]
fn embed_status_raw_counts_stay_consistent_with_ineligible_vectors_present() {
    let tmp = repo_with_vectors(10, 3);
    let stdout = embed_status(tmp.path());

    let total = count_after(&stdout, "Total symbols");
    let embedded = count_after(&stdout, "Embedded");
    assert_eq!(
        total, 10,
        "only the function nodes are embeddable:\n{stdout}"
    );
    assert_eq!(
        embedded, 10,
        "vectors on the three field nodes must not count (was 13):\n{stdout}"
    );
    assert!(embedded <= total, "{stdout}");
    assert!(
        stdout.contains("done"),
        "with every eligible node embedded the bar reads done:\n{stdout}"
    );
}

/// Genuinely missing vectors stay visible beside the ineligible ones: the fix
/// must not make the command read complete when it is not.
#[test]
fn embed_status_still_shows_pending_work_beside_ineligible_vectors() {
    let tmp = repo_with_vectors(10, 3);
    // Drop the vectors of four eligible nodes, so 6/10 are embedded.
    {
        let conn = rusqlite::Connection::open(tmp.path().join(".travsr/embed.db")).unwrap();
        let deleted = conn
            .execute(
                "DELETE FROM node_embeddings WHERE node_id IN \
                 (SELECT node_id FROM node_embeddings ORDER BY node_id LIMIT 4)",
                [],
            )
            .unwrap();
        assert_eq!(deleted, 4);
    }
    // Which four went is whatever sorted first; make sure the eligible count
    // matches what the store now sees rather than assuming.
    let expected_embedded = {
        let store = SqliteStore::open(&tmp.path().join(".travsr/graph.db")).unwrap();
        store.embed_progress(MODEL, 3).unwrap().1
    };
    assert!(
        expected_embedded < 10,
        "precondition: at least one eligible vector was deleted"
    );

    let stdout = embed_status(tmp.path());
    let total = count_after(&stdout, "Total symbols");
    let embedded = count_after(&stdout, "Embedded");
    assert_eq!(total, 10, "{stdout}");
    assert_eq!(embedded, expected_embedded, "{stdout}");
    assert!(
        embedded < total,
        "pending work must remain visible:\n{stdout}"
    );
}
