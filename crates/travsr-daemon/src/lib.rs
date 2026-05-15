//! travsr-daemon — long-running orchestrator for Travsr.
//!
//! Owns git hook installation, file watching, incremental reindexing, and
//! will host the MCP server in Sprint 3. "Always fresh" is the daemon's
//! core mandate — see CLAUDE.md principle #2.

#![forbid(unsafe_code)]

mod hook;
pub mod watcher;

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use ignore::WalkBuilder;
use travsr_indexer::{hash_file, Indexer};
use travsr_store::{SqliteStore, Store};

pub use hook::install_hook;

/// Statistics returned by [`init_repo`] and displayed by `travsr init`.
#[derive(Debug, Default)]
pub struct InitStats {
    pub files_indexed: u64,
    /// Net change in node count (positive = added, negative = removed).
    pub nodes_written: i64,
    pub edges_written: u64,
}

/// Initialise a Travsr index in `repo_root`:
/// 1. Create `.travsr/graph.db` with WAL-mode migrations.
/// 2. Install the `post-commit` git hook.
/// 3. Walk all `.ts`/`.tsx` files (honours `.gitignore`) and index them via
///    the delta path so the `files` hash table is populated from the start.
pub fn init_repo(repo_root: &Path) -> anyhow::Result<InitStats> {
    let travsr_dir = repo_root.join(".travsr");
    std::fs::create_dir_all(&travsr_dir).context("creating .travsr directory")?;

    let db_path = travsr_dir.join("graph.db");
    let mut store =
        SqliteStore::open(&db_path).with_context(|| format!("opening {}", db_path.display()))?;

    install_hook(repo_root)?;

    let nodes_before = store.node_count().context("counting nodes before init")? as i64;

    let mut files_indexed: u64 = 0;
    let mut edges_written: u64 = 0;

    let walker = WalkBuilder::new(repo_root)
        .hidden(false)
        .git_ignore(true)
        .build();

    let mut ts_paths: Vec<PathBuf> = Vec::new();
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!("walk error: {err}");
                continue;
            }
        };
        let p = entry.into_path();
        if !p.is_file() {
            continue;
        }
        let ext = p.extension().and_then(|e| e.to_str());
        if matches!(ext, Some("ts" | "tsx")) {
            ts_paths.push(p);
        }
    }

    for abs_path in &ts_paths {
        let edges_before = store.edge_count().unwrap_or(0);
        reindex_files(std::slice::from_ref(abs_path), repo_root, &mut store)?;
        edges_written += store.edge_count().unwrap_or(0).saturating_sub(edges_before);
        files_indexed += 1;
    }

    let nodes_after = store.node_count().context("counting nodes after init")? as i64;

    Ok(InitStats {
        files_indexed,
        nodes_written: nodes_after - nodes_before,
        edges_written,
    })
}

/// Re-index a set of changed files into `store`.
///
/// For each file:
/// - Compute its SHA-256 hash.
/// - Skip if the stored hash matches (file unchanged).
/// - Otherwise: delete its nodes/edges, re-parse, persist new records,
///   update the file hash, and record the HEAD commit SHA in `meta`.
pub fn reindex_files(
    paths: &[PathBuf],
    repo_root: &Path,
    store: &mut SqliteStore,
) -> anyhow::Result<()> {
    let indexer = Indexer::new();

    for abs_path in paths {
        let vname_path = abs_path
            .strip_prefix(repo_root)
            .unwrap_or(abs_path)
            .to_string_lossy()
            .replace('\\', "/");

        let new_hash = match hash_file(abs_path) {
            Ok(h) => h,
            Err(err) => {
                tracing::warn!("skipping {}: {err}", abs_path.display());
                continue;
            }
        };
        let new_hex = hex_encode(&new_hash);

        let old_hex = store.get_file_hash(&vname_path)?;
        if old_hex.as_deref() == Some(&new_hex) {
            continue; // unchanged — skip
        }

        store.delete_nodes_for_path(&vname_path)?;

        let out = match indexer.parse_file_with_vname(abs_path, &vname_path) {
            Ok(o) => o,
            Err(err) => {
                tracing::warn!("parse error for {}: {err}", abs_path.display());
                continue;
            }
        };

        for node in &out.nodes {
            if let Err(err) = store.put_node(node) {
                tracing::warn!("node write error: {err}");
            }
        }
        for edge in &out.edges {
            if let Err(err) = store.put_edge(edge) {
                tracing::warn!("edge write error: {err}");
            }
        }

        store.put_file_hash(&vname_path, &new_hex)?;
    }

    // Record the current HEAD commit so `travsr status` can show freshness.
    if let Ok(sha) = read_head_commit_sha(repo_root) {
        let _ = store.set_meta("last_commit", &sha);
    }

    Ok(())
}

fn hex_encode(bytes: &[u8; 32]) -> String {
    bytes.iter().fold(String::with_capacity(64), |mut s, b| {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn read_head_commit_sha(repo_root: &Path) -> anyhow::Result<String> {
    let out = std::process::Command::new("git")
        .args([
            "-C",
            &repo_root.to_string_lossy(),
            "rev-parse",
            "--short",
            "HEAD",
        ])
        .output()
        .context("running git rev-parse")?;
    anyhow::ensure!(out.status.success(), "git rev-parse failed");
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;

    fn git_init(dir: &std::path::Path) {
        StdCommand::new("git")
            .args(["-c", "init.defaultBranch=main", "init", "-q"])
            .current_dir(dir)
            .status()
            .expect("git init");
        // Minimal git config so commits work
        StdCommand::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir)
            .status()
            .unwrap();
        StdCommand::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir)
            .status()
            .unwrap();
    }

    #[test]
    fn init_repo_creates_db_and_hook() {
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        std::fs::write(tmp.path().join("app.ts"), "export class App { run() {} }").unwrap();

        let stats = init_repo(tmp.path()).unwrap();

        assert!(
            tmp.path().join(".travsr/graph.db").exists(),
            "DB must be created"
        );
        assert!(
            tmp.path().join(".git/hooks/post-commit").exists(),
            "hook must be installed"
        );
        assert!(stats.files_indexed >= 1);
        assert!(stats.nodes_written > 0, "at least one node indexed");
    }

    #[test]
    fn reindex_files_skips_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        let ts_path = tmp.path().join("svc.ts");
        std::fs::write(&ts_path, "export class Svc { go() {} }").unwrap();

        let db_path = tmp.path().join(".travsr/graph.db");
        std::fs::create_dir_all(tmp.path().join(".travsr")).unwrap();
        let mut store = travsr_store::SqliteStore::open(&db_path).unwrap();

        reindex_files(std::slice::from_ref(&ts_path), tmp.path(), &mut store).unwrap();
        let count_after_first = store.node_count().unwrap();

        reindex_files(std::slice::from_ref(&ts_path), tmp.path(), &mut store).unwrap();
        let count_after_second = store.node_count().unwrap();

        assert_eq!(
            count_after_first, count_after_second,
            "unchanged file must not add duplicate nodes"
        );
    }

    #[test]
    fn reindex_files_updates_on_change() {
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        let ts_path = tmp.path().join("svc.ts");
        std::fs::write(&ts_path, "export class OldSvc { foo() {} }").unwrap();

        let db_path = tmp.path().join(".travsr/graph.db");
        std::fs::create_dir_all(tmp.path().join(".travsr")).unwrap();
        let mut store = travsr_store::SqliteStore::open(&db_path).unwrap();

        reindex_files(std::slice::from_ref(&ts_path), tmp.path(), &mut store).unwrap();

        // Edit the file — different content means different hash
        std::fs::write(&ts_path, "export class NewSvc { bar() {} baz() {} }").unwrap();
        reindex_files(std::slice::from_ref(&ts_path), tmp.path(), &mut store).unwrap();

        // NewSvc has 2 methods → more nodes than OldSvc with 1 method
        let node_count = store.node_count().unwrap();
        assert!(
            node_count >= 3,
            "new class + 2 methods = at least 3 nodes, got {node_count}"
        );

        // OldSvc class node must be gone
        let results = store.search_nodes_by_name("OldSvc").unwrap();
        assert!(
            results.is_empty(),
            "old class node must be deleted after reindex"
        );
    }
}

/// The Travsr daemon process (event loop wires in Sprint 3).
#[derive(Debug, Default)]
pub struct Daemon {
    _private: (),
}

impl Daemon {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn run() -> anyhow::Result<()> {
        tracing::info!("travsr-daemon: event loop stub — Sprint 3");
        Ok(())
    }
}
