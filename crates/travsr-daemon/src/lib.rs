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
use travsr_core::{canonical_corpus, canonical_corpus_local};
use travsr_indexer::{hash_file, ingest_lsif, link_imports, run_lsif_emitter, Indexer};
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

    // Register in global registry so `travsr mcp --global` can find this repo.
    let repo_name = repo_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    if let Err(e) = travsr_store::registry::register(repo_name, &db_path) {
        tracing::warn!("registry update failed (non-fatal): {e}");
    }

    let nodes_before = store.node_count().context("counting nodes before init")? as i64;

    // ARCH-102: detect canonical corpus from the git remote and persist it so
    // every VName in this graph uses the same corpus identifier.
    // reindex_files reads this value back on subsequent hook runs.
    let corpus = detect_corpus(repo_root);
    if let Err(e) = store.set_meta("corpus", &corpus) {
        tracing::warn!("could not write corpus to meta: {e}");
    }
    tracing::debug!("corpus for {}: {corpus}", repo_root.display());

    let mut files_indexed: u64 = 0;
    let mut edges_written: u64 = 0;

    let walker = WalkBuilder::new(repo_root)
        .hidden(false)
        .git_ignore(true)
        .follow_links(false)
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
        // Use entry.file_type() before into_path() — it does NOT follow symlinks,
        // so symlinks pointing at .ts files are excluded. p.is_file() would follow them.
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let p = entry.into_path();
        let ext = p.extension().and_then(|e| e.to_str());
        if matches!(ext, Some("ts" | "tsx")) {
            ts_paths.push(p);
        }
    }

    // DEBT(travsr-73): files_indexed counts every path handed to reindex_files,
    // including files skipped due to size or parse errors. The user-visible
    // "indexed N files" message is therefore optimistic. Fix by returning a
    // per-file success/skip result from reindex_files.
    for abs_path in &ts_paths {
        let edges_before = store.edge_count().unwrap_or(0);
        reindex_files(std::slice::from_ref(abs_path), repo_root, &mut store)?;
        edges_written += store.edge_count().unwrap_or(0).saturating_sub(edges_before);
        files_indexed += 1;
    }

    let nodes_after = store.node_count().context("counting nodes after init")? as i64;

    // LSIF semantic pass — adds RefCall edges on top of structural edges.
    // DEBT(travsr-25): whole-project re-emit; file-level delta is Phase 3.
    run_lsif_pass(repo_root, &mut store);

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
    // ARCH-102: read the corpus that was set during init_repo so that
    // incremental hook runs produce VNames with the same corpus as the
    // initial full index. Fall back gracefully for legacy DBs.
    let corpus = store
        .get_meta("corpus")
        .unwrap_or_default()
        .unwrap_or_default();
    if corpus.is_empty() {
        tracing::warn!(
            "no corpus in meta — VNames will use empty corpus. \
             Run `travsr init` to set the canonical corpus (ARCH-102)."
        );
    }

    let indexer = Indexer::with_corpus(&corpus);

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

        for edge in link_imports(&out.nodes, &vname_path, &corpus) {
            if let Err(err) = store.put_edge(&edge) {
                tracing::warn!("resolves-to edge write error: {err}");
            }
        }

        store.put_file_hash(&vname_path, &new_hex)?;
    }

    // Record the current HEAD commit so `travsr status` can show freshness.
    if let Ok(sha) = read_head_commit_sha(repo_root) {
        let _ = store.set_meta("last_commit", &sha);
    }

    // LSIF semantic pass — only when at least one TypeScript file was in the
    // delta. This avoids a whole-project TS compile on every commit that only
    // touches Rust/config files. Full file-level delta is DEBT(travsr-25).
    let any_ts = paths
        .iter()
        .any(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("ts" | "tsx")));
    if any_ts {
        run_lsif_pass(repo_root, store);
    }

    Ok(())
}

/// Run the LSIF semantic pass if `tsconfig.json` is present at the repo root.
///
/// Failures (binary not on PATH, tsconfig absent, parse errors) are logged as
/// warnings and silently skipped — they must never fail the overall index.
fn run_lsif_pass(repo_root: &Path, store: &mut SqliteStore) {
    let tsconfig = repo_root.join("tsconfig.json");
    if !tsconfig.exists() {
        return;
    }

    let dump = match run_lsif_emitter(&tsconfig) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("lsif emitter skipped: {e}");
            return;
        }
    };

    let lsif_out = match ingest_lsif(&dump) {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!("lsif ingest error: {e}");
            return;
        }
    };

    for edge in &lsif_out.edges {
        if let Err(e) = store.put_edge_lsif(edge) {
            tracing::warn!("lsif edge write error: {e}");
        }
    }

    tracing::debug!(
        "lsif pass: {} RefCall edges persisted",
        lsif_out.edges.len()
    );
}

/// Derive the canonical corpus for `repo_root` by reading `git remote get-url origin`.
/// Falls back to `local/<basename>` if no remote is configured or git fails.
fn detect_corpus(repo_root: &Path) -> String {
    let output = std::process::Command::new("git")
        .args([
            "-C",
            &repo_root.to_string_lossy(),
            "remote",
            "get-url",
            "origin",
        ])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let remote = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if remote.is_empty() {
                canonical_corpus_local(repo_root)
            } else {
                canonical_corpus(&remote)
            }
        }
        _ => canonical_corpus_local(repo_root),
    }
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

    #[cfg(unix)]
    #[test]
    fn symlink_is_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());

        // Create a directory outside the repo with a .ts file inside it.
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.ts"), "export const secret = 1;").unwrap();

        // Symlink pointing at the outside directory from inside the repo.
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("linked")).unwrap();

        let stats = init_repo(tmp.path()).unwrap();
        // The symlink target must not have been indexed — files_indexed
        // counts only real files in the repo.
        assert_eq!(stats.files_indexed, 0, "symlink target must not be indexed");
    }

    // init_repo must complete successfully even when an oversized .ts file is present.
    // The oversized file must be silently skipped (parse error is logged, not propagated).
    #[test]
    fn init_repo_gracefully_skips_oversized_ts_file() {
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());

        // Write a file over the 10 MB limit.
        let big = vec![b'a'; 11 * 1024 * 1024];
        std::fs::write(tmp.path().join("giant.ts"), &big).unwrap();

        // Must not return Err — skipping is a warning, not a fatal error.
        let result = init_repo(tmp.path());
        assert!(
            result.is_ok(),
            "init_repo must succeed even with an oversized file: {result:?}"
        );
    }

    // Valid files must still be indexed alongside an oversized file.
    // Regression: the oversized file must not abort indexing of healthy files.
    #[test]
    fn init_repo_indexes_valid_files_alongside_oversized() {
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());

        // One valid file.
        std::fs::write(tmp.path().join("real.ts"), "export class Real { ok() {} }").unwrap();
        // One oversized file next to it.
        let big = vec![b'a'; 11 * 1024 * 1024];
        std::fs::write(tmp.path().join("giant.ts"), &big).unwrap();

        let stats = init_repo(tmp.path()).unwrap();
        assert!(
            stats.nodes_written > 0,
            "valid file must be indexed even when an oversized file is present"
        );
    }

    // SEC-006: entry.file_type() (no symlink follow) must exclude a symlink that
    // points directly at a .ts file outside the repo.
    #[cfg(unix)]
    #[test]
    fn symlink_to_ts_file_is_not_indexed() {
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());

        // Place a .ts file outside the repo.
        let outside = tempfile::tempdir().unwrap();
        let secret_path = outside.path().join("secret.ts");
        std::fs::write(&secret_path, "export const apiKey = 'hunter2';").unwrap();

        // Symlink to the FILE (not a directory) from inside the repo.
        std::os::unix::fs::symlink(&secret_path, tmp.path().join("leak.ts")).unwrap();

        let stats = init_repo(tmp.path()).unwrap();
        assert_eq!(
            stats.files_indexed, 0,
            "symlink to a .ts file must not be indexed"
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
