//! #454: `travsr repos` must tell "never indexed" apart from "index deleted".

use assert_cmd::Command;

/// Build a registry whose three rows cover the three reachable index states,
/// then return the temp HOME holding it.
fn home_with_three_states() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();

    let present = home.path().join("present/.travsr/graph.db");
    std::fs::create_dir_all(present.parent().unwrap()).unwrap();
    std::fs::write(&present, b"x").unwrap();

    let registry = serde_json::json!({
        "repos": {
            "/repos/present": { "db_path": present, "indexed_at": 1_700_000_000u64 },
            "/repos/never": { "db_path": home.path().join("never/.travsr/graph.db") },
            "/repos/deleted": {
                "db_path": home.path().join("deleted/.travsr/graph.db"),
                "indexed_at": 1_700_000_000u64
            },
        }
    });
    let dir = home.path().join(".travsr");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("registry.json"),
        serde_json::to_string_pretty(&registry).unwrap(),
    )
    .unwrap();
    home
}

fn repos(home: &std::path::Path, args: &[&str]) -> String {
    let out = Command::cargo_bin("travsr")
        .unwrap()
        .env("HOME", home)
        .env("USERPROFILE", home)
        .arg("repos")
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(out).unwrap()
}

#[test]
fn repos_json_reports_a_status_for_each_index_state() {
    let home = home_with_three_states();
    let rows: Vec<serde_json::Value> = serde_json::from_str(&repos(home.path(), &["--json"]))
        .expect("`repos --json` must emit a JSON array");

    let status = |name: &str| -> String {
        rows.iter()
            .find(|r| r["name"] == name)
            .unwrap_or_else(|| panic!("row {name} missing from {rows:?}"))["status"]
            .as_str()
            .unwrap_or_else(|| panic!("row {name} has no status field"))
            .to_string()
    };
    assert_eq!(status("present"), "indexed");
    assert_eq!(status("never"), "not_indexed");
    assert_eq!(status("deleted"), "index_missing");

    let exists = |name: &str| rows.iter().find(|r| r["name"] == name).unwrap()["exists"].clone();
    assert_eq!(exists("present"), serde_json::json!(true));
    assert_eq!(exists("never"), serde_json::json!(false));
    assert_eq!(exists("deleted"), serde_json::json!(false));
}

#[test]
fn repos_table_labels_the_two_missing_index_cases_differently() {
    let home = home_with_three_states();
    let table = repos(home.path(), &[]);
    assert!(
        table.contains("never indexed"),
        "table must say a never-indexed repo was never indexed, got:\n{table}"
    );
    assert!(
        table.contains("index deleted"),
        "table must say a deleted index was deleted, got:\n{table}"
    );
}
