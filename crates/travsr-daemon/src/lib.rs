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
use travsr_core::{canonical_corpus, canonical_corpus_local, Language, SIGNATURE_FORMAT_VERSION};
use travsr_indexer::{
    hash_file, ingest_lsif, link_imports, link_imports_python_fs, link_imports_rust,
    run_lsif_emitter, FfiMarker, Indexer,
};
use travsr_store::{SqliteStore, Store};

#[cfg(unix)]
pub use hook::try_dispatch_to_daemon;
pub use hook::{changed_files_from_git, install_hook};

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
/// 3. Walk all `.ts`/`.tsx`/`.rs` files (honours `.gitignore`, skips `target/`)
///    and index them via the delta path so the `files` hash table is populated
///    from the start.
pub fn init_repo(repo_root: &Path) -> anyhow::Result<InitStats> {
    let travsr_dir = repo_root.join(".travsr");
    std::fs::create_dir_all(&travsr_dir).context("creating .travsr directory")?;

    let db_path = travsr_dir.join("graph.db");
    let mut store =
        SqliteStore::open(&db_path).with_context(|| format!("opening {}", db_path.display()))?;

    // SEC: graph.db contains derived IP — restrict to owner only.
    // A silent failure here would leave the file world-readable, so warn loudly.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o600)) {
            tracing::warn!(
                path = %db_path.display(),
                err = %e,
                "failed to restrict graph.db permissions to 0600 — file may be world-readable"
            );
        }
    }

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

    // Stamp the format version BEFORE indexing so that the reindex_files calls
    // below see version == SIGNATURE_FORMAT_VERSION and don't skip files.
    store
        .set_signature_format_version(SIGNATURE_FORMAT_VERSION)
        .context("writing signature_format_version")?;

    // ARCH-102: detect canonical corpus from the git remote and persist it so
    // every VName in this graph uses the same corpus identifier.
    // reindex_files reads this value back on subsequent hook runs.
    let corpus = detect_corpus(repo_root);
    store
        .set_meta("corpus", &corpus)
        .context("writing corpus to meta (ARCH-102)")?;
    tracing::debug!("corpus for {}: {corpus}", repo_root.display());

    let mut files_indexed: u64 = 0;

    let walker = WalkBuilder::new(repo_root)
        .hidden(false)
        .git_ignore(true)
        .follow_links(false)
        .build();

    let mut indexable_paths: Vec<PathBuf> = Vec::new();
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!("walk error: {err}");
                continue;
            }
        };
        // Use entry.file_type() before into_path() — it does NOT follow symlinks,
        // so symlinks pointing at source files are excluded. p.is_file() would follow them.
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let p = entry.into_path();
        // Skip Rust build artifacts — the `target/` directory can be enormous
        // and contains no user-authored source files worth indexing.
        // CORRECTNESS: strip repo_root first so we check *relative* components
        // only. Checking the absolute path would falsely skip every file in a
        // repo that lives under e.g. `/home/target_user/myproject/`.
        let rel = p.strip_prefix(repo_root).unwrap_or(&p);
        if rel.components().any(|c| c.as_os_str() == "target") {
            continue;
        }
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
        if Language::from_extension(ext).is_some() {
            indexable_paths.push(p);
        }
    }

    // DEBT(travsr-73): files_indexed counts every path handed to reindex_files,
    // including files skipped due to size or parse errors. The user-visible
    // "indexed N files" message is therefore optimistic. Fix by returning a
    // per-file success/skip result from reindex_files.
    //
    // PERF: Two COUNT(*) per file (N files → 2N full-table scans) was causing
    // WAL page-cache bloat during large init_repo runs. Replaced with two
    // total counts: one snapshot before the loop, one after.
    let edges_before = store.edge_count().unwrap_or(0);
    for abs_path in &indexable_paths {
        reindex_files(std::slice::from_ref(abs_path), repo_root, &mut store)?;
        files_indexed += 1;
    }
    let edges_written = store.edge_count().unwrap_or(0).saturating_sub(edges_before);

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
    // RFC-002: detect signature format version mismatch before touching the
    // graph. The hook must never block a commit, so return Ok(()) on mismatch
    // and let the user resolve it with `travsr init`.
    match store.get_signature_format_version() {
        Ok(stored) if stored != SIGNATURE_FORMAT_VERSION => {
            tracing::warn!(
                "skipping reindex: graph.db was built with signature format v{stored} \
                 but this binary uses v{SIGNATURE_FORMAT_VERSION}. \
                 Run `travsr init` to re-index and update the graph."
            );
            return Ok(());
        }
        Err(e) => {
            tracing::warn!(
                "could not read signature_format_version: {e}, skipping reindex. \
                 Run `travsr init` to repair the graph."
            );
            return Ok(());
        }
        _ => {}
    }

    // ARCH-102: read the corpus that was set during init_repo so that
    // incremental hook runs produce VNames with the same corpus as the
    // initial full index. Fall back gracefully for legacy DBs.
    let corpus = match store.get_meta("corpus") {
        Ok(Some(c)) => c,
        Ok(None) => {
            tracing::warn!(
                "no corpus in meta — VNames will use empty corpus. \
                 Run `travsr init` to set the canonical corpus (ARCH-102)."
            );
            String::new()
        }
        Err(e) => {
            tracing::warn!("could not read corpus from meta: {e} — using empty corpus");
            String::new()
        }
    };

    let indexer = Indexer::with_corpus(&corpus);
    // Accumulate FFI markers across all files for repo-level cross-language
    // resolution (RFC-005). Resolution runs once after the per-file loop so
    // markers from both sides of each FFI boundary are available.
    let mut all_ffi_markers: Vec<FfiMarker> = Vec::new();
    let mut any_changed = false;

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

        // Route to the language-appropriate import resolver.
        let ext = abs_path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let import_edges = match Language::from_extension(ext) {
            Some(Language::TypeScript) => link_imports(&out.nodes, &vname_path, &corpus),
            Some(Language::Rust) => link_imports_rust(&out.nodes, &vname_path, &corpus),
            Some(Language::Python) => {
                link_imports_python_fs(&out.nodes, &vname_path, &corpus, repo_root)
            }
            _ => Vec::new(),
        };
        for edge in import_edges {
            if let Err(err) = store.put_edge(&edge) {
                tracing::warn!("resolves-to edge write error: {err}");
            }
        }

        store.put_file_hash(&vname_path, &new_hex)?;
        any_changed = true;

        // Collect FFI markers for the repo-level pass (RFC-005).
        all_ffi_markers.extend(out.ffi_markers);
    }

    // Cross-language FFI resolution — single pass over all accumulated markers
    // (RFC-005). Runs here so markers from both sides of each FFI boundary are
    // visible (e.g. Rust NapiExport + TypeScript NapiCall from the same batch).
    // DEBT(travsr-024): incremental indexing only accumulates markers for files
    // that changed; a future pass should also load existing markers from the store
    // to handle the case where only one side of a boundary is re-indexed.
    if !all_ffi_markers.is_empty() {
        let ffi_edges = indexer.resolve_ffi_edges(&all_ffi_markers);
        for edge in &ffi_edges {
            if let Err(err) = store.put_edge(edge) {
                tracing::warn!("ffi edge write error: {err}");
            }
        }
    }

    // Record the current HEAD commit so `travsr status` can show freshness.
    // Only write when at least one file actually changed — avoids stamping
    // noise events (sockets, gitignored files, directories) as a real reindex.
    if any_changed {
        if let Ok(sha) = read_head_commit_sha(repo_root) {
            let _ = store.set_meta("last_commit", &sha);
        }
    }

    // PERF-002: The LSIF semantic pass was running on every `reindex_files`
    // call that touched any .ts file, including every post-commit hook run.
    // `run_lsif_emitter` buffers the entire tsc JSON output into a String
    // (200-500 MB for large projects) before parsing. Running this on every
    // commit caused a repeated 200-500 MB RSS spike.
    //
    // The LSIF pass now runs only from `init_repo` (full initial index).
    // Per-commit LSIF delta is tracked as DEBT(travsr-25).

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
        // Simulate init_repo: stamp the version before any reindex call.
        store
            .set_signature_format_version(travsr_core::SIGNATURE_FORMAT_VERSION)
            .unwrap();

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
        // Simulate init_repo: stamp the version before any reindex call.
        store
            .set_signature_format_version(travsr_core::SIGNATURE_FORMAT_VERSION)
            .unwrap();

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

    /// RFC-002: when the stored signature format version differs from the binary's
    /// version, `reindex_files` must return `Ok(())` without touching the graph.
    /// This is the core correctness guarantee — a version mismatch must never
    /// silently corrupt the graph with mixed-format NodeIds.
    /// DAEMON-201: init_repo must walk and index `.rs` files the same way it
    /// handles `.ts` files — at least one node must be written for a valid Rust
    /// source file placed in the repo root.
    #[test]
    fn init_repo_indexes_rust_files() {
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        std::fs::write(tmp.path().join("lib.rs"), "pub fn hello() -> u32 { 42 }").unwrap();

        let stats = init_repo(tmp.path()).unwrap();

        assert!(
            stats.files_indexed >= 1,
            "at least one Rust file must be counted as indexed"
        );
        assert!(
            stats.nodes_written > 0,
            "at least one node must be written for the Rust file"
        );
    }

    /// DAEMON-201: any relative path component named `target` must be skipped
    /// during the initial walk so Rust build artifacts are never indexed.
    /// The `.gitignore` is intentionally left empty to ensure the component
    /// check — not gitignore — is responsible for excluding `target/`.
    #[test]
    fn target_directory_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());

        // Intentionally empty gitignore — the walker's component check must do
        // the work, not a gitignore rule. This guards against a future git
        // template that auto-adds `target/` and masks the real mechanism.
        std::fs::write(tmp.path().join(".gitignore"), "# intentionally empty\n").unwrap();

        // Simulate a Rust build artifact inside target/.
        let target_dir = tmp.path().join("target").join("debug");
        std::fs::create_dir_all(&target_dir).unwrap();
        std::fs::write(target_dir.join("build.rs"), "fn main() {}").unwrap();

        // Real source file alongside it.
        std::fs::write(tmp.path().join("src.rs"), "pub fn real() {}").unwrap();

        let stats = init_repo(tmp.path()).unwrap();

        // Only src.rs must be indexed — target/debug/build.rs must be skipped.
        assert_eq!(
            stats.files_indexed, 1,
            "target/ contents must not be indexed; expected 1 file, got {}",
            stats.files_indexed
        );
    }

    #[test]
    fn reindex_files_skips_on_version_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        let ts_path = tmp.path().join("svc.ts");
        std::fs::write(&ts_path, "export class Svc { go() {} }").unwrap();

        let db_path = tmp.path().join(".travsr/graph.db");
        std::fs::create_dir_all(tmp.path().join(".travsr")).unwrap();
        let mut store = travsr_store::SqliteStore::open(&db_path).unwrap();
        // Stamp version 0 — simulates a DB built with an older binary.
        store.set_signature_format_version(0).unwrap();

        // Must succeed (hook must never block a commit) but must skip indexing.
        reindex_files(std::slice::from_ref(&ts_path), tmp.path(), &mut store).unwrap();

        assert_eq!(
            store.node_count().unwrap(),
            0,
            "version mismatch must leave graph untouched"
        );
        // Version must still be 0 — reindex must not overwrite it.
        assert_eq!(
            store.get_signature_format_version().unwrap(),
            0,
            "version must not be updated by reindex on mismatch"
        );
    }
}

/// Messages sent to the daemon's control socket (Unix only).
#[cfg(unix)]
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
enum ControlRequest {
    ReindexCommit { sha: String },
    ReindexPaths { paths: Vec<PathBuf> },
    Status,
    Shutdown,
}

#[cfg(unix)]
#[derive(Debug, serde::Serialize)]
struct ControlResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

/// The Travsr daemon — owns the file watcher, indexer worker, and control socket.
#[derive(Debug, Default)]
pub struct Daemon {
    _private: (),
}

impl Daemon {
    pub fn new() -> Self {
        Self::default()
    }

    /// Run the daemon event loop. Acquires an exclusive lockfile, starts the
    /// file watcher, control socket, and GC ticker. Blocks until SIGTERM/SIGINT.
    pub async fn run(repo_root: std::path::PathBuf) -> anyhow::Result<()> {
        use fs2::FileExt as _;
        use std::sync::{Arc, Mutex};
        #[cfg(unix)]
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        #[cfg(unix)]
        use tokio::net::UnixListener;

        let travsr_dir = repo_root.join(".travsr");
        std::fs::create_dir_all(&travsr_dir).context("creating .travsr")?;

        // Acquire exclusive lockfile — OS releases the lock on process death.
        let lock_path = travsr_dir.join("daemon.lock");
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&lock_path)
            .context("opening daemon.lock")?;
        lock_file.try_lock_exclusive().map_err(|_| {
            // Read PID for a helpful error message.
            let pid = std::fs::read_to_string(&lock_path)
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
                .map(|p| format!(" PID={p}"))
                .unwrap_or_default();
            anyhow::anyhow!("another travsr daemon is already running{pid}")
        })?;
        // Write our PID into the lockfile for `daemon status`.
        use std::io::Write as _;
        write!(&lock_file, "{}", std::process::id())?;

        let db_path = travsr_dir.join("graph.db");
        let store = Arc::new(Mutex::new(
            SqliteStore::open(&db_path).context("opening graph.db")?,
        ));

        // Bounded channel: 256 events max. The single indexer worker below drains
        // this channel one event at a time, so back-pressure naturally throttles
        // the watcher rather than letting thousands of events queue up in memory.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<watcher::WatchEvent>(256);
        let start_time = std::time::Instant::now();

        // Remove stale socket BEFORE starting the watcher. The kqueue backend
        // (macos_kqueue feature) opens every file during its initial recursive
        // scan; a leftover socket from a previous run causes open() → ENOTSUP
        // which aborts the entire watch setup and silently kills file watching.
        #[cfg(unix)]
        let sock_path = travsr_dir.join("daemon.sock");
        #[cfg(unix)]
        let _ = std::fs::remove_file(&sock_path);

        let _watcher_handle =
            watcher::spawn(&repo_root, tx.clone(), start_time).context("starting file watcher")?;

        // Control socket — Unix domain socket at .travsr/daemon.sock (Unix only).
        #[cfg(unix)]
        let listener = UnixListener::bind(&sock_path).context("binding daemon.sock")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o600));
        }

        let mut gc_tick = tokio::time::interval(std::time::Duration::from_secs(300));
        let mut shutdown = std::pin::pin!(tokio::signal::ctrl_c());

        tracing::info!(repo = %repo_root.display(), "travsr daemon started");
        #[cfg(unix)]
        tracing::info!(sock = %sock_path.display(), "control socket bound");

        let repo_root_arc = Arc::new(repo_root.clone());
        // Notify used for socket-initiated shutdown signal (Unix only).
        #[cfg(unix)]
        let sock_shutdown = Arc::new(tokio::sync::Notify::new());

        // ── Single dedicated indexer worker ───────────────────────────────────
        // PERF-001: Previously the event loop spawned a new blocking thread for
        // every incoming WatchEvent. Under a large checkout or IDE auto-save
        // flood (e.g. 100 events in < 500 ms) this created up to 100 OS threads
        // simultaneously. All of them immediately blocked on `store.lock()`,
        // serialising anyway — so the only effect was ~800 MB of wasted thread
        // stacks (100 threads × 8 MB stack = 800 MB RSS spike).
        //
        // The fix: a single `spawn_blocking` worker owns a `std::sync::mpsc`
        // receiver and processes events one at a time. The Tokio async loop
        // forwards `WatchEvent`s through a std channel (`index_tx`) to this
        // worker. No new threads are ever spawned for indexing — at most ONE
        // blocking thread handles indexing at any time.
        let (index_tx, index_rx) = std::sync::mpsc::channel::<watcher::WatchEvent>();
        let indexer_worker = {
            let store_worker = Arc::clone(&store);
            let repo_worker = Arc::clone(&repo_root_arc);
            tokio::task::spawn_blocking(move || {
                while let Ok(ev) = index_rx.recv() {
                    handle_watch_event(ev, &repo_worker, &store_worker);
                }
                tracing::debug!("indexer worker exiting — channel closed");
            })
        };

        loop {
            // tokio::select! does not support #[cfg] on individual branches,
            // so we use two complete select! blocks, one per platform.
            #[cfg(unix)]
            {
                let sock_shutdown_wait = sock_shutdown.notified();
                tokio::select! {
                    Some(ev) = rx.recv() => {
                        // Forward to the dedicated indexer worker — never spawn a
                        // new thread here (PERF-001).
                        if index_tx.send(ev).is_err() {
                            tracing::warn!("indexer worker has exited; dropping watch event");
                        }
                    }
                    Ok((conn, _)) = listener.accept() => {
                        let store = Arc::clone(&store);
                        let repo = Arc::clone(&repo_root_arc);
                        let sd_notify = Arc::clone(&sock_shutdown);
                        tokio::spawn(async move {
                            let (reader, mut writer) = conn.into_split();
                            let mut lines = BufReader::new(reader).lines();
                            if let Ok(Some(line)) = lines.next_line().await {
                                // reindex_files blocks on I/O and the store mutex —
                                // run on the blocking thread pool so we don't stall
                                // the Tokio executor.
                                let (resp, shutdown_requested) =
                                    tokio::task::spawn_blocking(move || {
                                        handle_control_message(&line, repo.as_path(), &store)
                                    })
                                    .await
                                    .unwrap_or_else(|_| (
                                        ControlResponse {
                                            ok: false,
                                            message: Some("internal error".to_string()),
                                        },
                                        false,
                                    ));
                                let _ = writer
                                    .write_all(
                                        format!(
                                            "{}\n",
                                            serde_json::to_string(&resp)
                                                .unwrap_or_default()
                                        )
                                        .as_bytes(),
                                    )
                                    .await;
                                if shutdown_requested {
                                    sd_notify.notify_one();
                                }
                            }
                        });
                    }
                    _ = gc_tick.tick() => {
                        tracing::debug!(
                            "daemon heartbeat — uptime {}s",
                            start_time.elapsed().as_secs()
                        );
                    }
                    _ = &mut shutdown => {
                        tracing::info!("travsr daemon shutting down (SIGINT)");
                        break;
                    }
                    _ = sock_shutdown_wait => {
                        tracing::info!("travsr daemon shutting down (control socket)");
                        break;
                    }
                }
            }
            #[cfg(not(unix))]
            {
                tokio::select! {
                    Some(ev) = rx.recv() => {
                        // Forward to the dedicated indexer worker — never spawn a
                        // new thread here (PERF-001).
                        if index_tx.send(ev).is_err() {
                            tracing::warn!("indexer worker has exited; dropping watch event");
                        }
                    }
                    _ = gc_tick.tick() => {
                        tracing::debug!(
                            "daemon heartbeat — uptime {}s",
                            start_time.elapsed().as_secs()
                        );
                    }
                    _ = &mut shutdown => {
                        tracing::info!("travsr daemon shutting down");
                        break;
                    }
                }
            }
        }

        // 1. Drain remaining events from the Tokio mpsc channel into the indexer
        //    worker's std channel before signalling shutdown. This avoids calling
        //    handle_watch_event (blocking SQLite I/O) directly on the async
        //    executor thread, which would stall the current_thread runtime if the
        //    worker concurrently holds store.lock().
        rx.close();
        while let Ok(ev) = rx.try_recv() {
            let _ = index_tx.send(ev);
        }
        // 2. Signal the indexer worker: drop index_tx to close the std channel.
        //    The worker drains any remaining events from index_rx then exits.
        drop(index_tx);
        // 3. Wait for the indexer worker to finish draining index_rx and exit.
        //    Without this await, the tokio runtime would wait for the detached
        //    spawn_blocking task at shutdown, which caused the test hang.
        //    30s is generous; in practice the worker exits in milliseconds once
        //    index_tx is dropped.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(30), indexer_worker).await;

        #[cfg(unix)]
        let _ = std::fs::remove_file(&sock_path);
        drop(lock_file);
        tracing::info!("travsr daemon stopped");
        Ok(())
    }
}

fn handle_watch_event(
    ev: watcher::WatchEvent,
    repo_root: &std::path::Path,
    store: &std::sync::Mutex<SqliteStore>,
) {
    use watcher::WatchEvent;
    match ev {
        WatchEvent::Upsert(path) => {
            let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(e) = reindex_files(std::slice::from_ref(&path), repo_root, &mut s) {
                tracing::warn!(path=%path.display(), err=%e, "watcher reindex failed");
            }
        }
        WatchEvent::Remove(path) => {
            let vname_path = path
                .strip_prefix(repo_root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(e) = s.delete_nodes_for_path(&vname_path) {
                tracing::warn!(path=%path.display(), err=%e, "watcher delete failed");
            }
        }
    }
}

/// Returns `(response, should_shutdown)`.
#[cfg(unix)]
fn handle_control_message(
    line: &str,
    repo_root: &std::path::Path,
    store: &std::sync::Mutex<SqliteStore>,
) -> (ControlResponse, bool) {
    match serde_json::from_str::<ControlRequest>(line) {
        Ok(ControlRequest::ReindexCommit { sha }) => {
            tracing::info!(sha=%sha, "control: reindex-commit");
            let paths = changed_files_from_git(repo_root).unwrap_or_default();
            let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(e) = reindex_files(&paths, repo_root, &mut s) {
                tracing::warn!(err=%e, "control reindex failed");
                return (
                    ControlResponse {
                        ok: false,
                        message: Some(e.to_string()),
                    },
                    false,
                );
            }
            (
                ControlResponse {
                    ok: true,
                    message: Some(format!("reindexed {} paths", paths.len())),
                },
                false,
            )
        }
        Ok(ControlRequest::ReindexPaths { paths }) => {
            let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(e) = reindex_files(&paths, repo_root, &mut s) {
                return (
                    ControlResponse {
                        ok: false,
                        message: Some(e.to_string()),
                    },
                    false,
                );
            }
            (
                ControlResponse {
                    ok: true,
                    message: None,
                },
                false,
            )
        }
        Ok(ControlRequest::Status) => (
            ControlResponse {
                ok: true,
                message: Some("running".to_string()),
            },
            false,
        ),
        Ok(ControlRequest::Shutdown) => {
            // Signal the event loop to stop via the broadcast channel.
            (
                ControlResponse {
                    ok: true,
                    message: None,
                },
                true,
            )
        }
        Err(e) => (
            ControlResponse {
                ok: false,
                message: Some(format!("parse error: {e}")),
            },
            false,
        ),
    }
}
