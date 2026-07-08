//! travsr-daemon — long-running orchestrator for Travsr.
//!
//! Owns git hook installation, file watching, incremental reindexing, and
//! will host the MCP server in Sprint 3. "Always fresh" is the daemon's
//! core mandate — see CLAUDE.md principle #2.

#![forbid(unsafe_code)]

mod hook;
mod phase_b_sched;
mod query_cache;
mod scip_unifier;
pub mod watcher;

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use ignore::WalkBuilder;
use travsr_analysis::skeleton::{embed_texts_for_file, EmbedRichness};
use travsr_core::{canonical_corpus, canonical_corpus_local, Language, SIGNATURE_FORMAT_VERSION};
use travsr_indexer::{
    hash_file, ingest_lsif, link_imports, link_imports_go, link_imports_python_fs,
    link_imports_rust, run_lsif_emitter, FfiMarker,
};
use travsr_plugin_host::PluginIndexer;
use travsr_retrieval::compute_kcore;
use travsr_store::{BatchWriteCounts, FileGraph, SqliteStore, Store};

pub use hook::{changed_files_from_git, install_hook, try_dispatch_to_daemon};

/// Set the process-level opt-in flag that allows `rust-analyzer` to run
/// unconfined when the OS sandbox is unavailable.
///
/// # Scope: `travsr init` only (R2)
///
/// This function has exactly **one** caller: `travsr_cli::init::run`, which sets
/// it once before `init_repo_with_progress` and then exits. The flag therefore
/// applies only to the single-shot `travsr init` process.
///
/// The **long-lived daemon** (git-hook / file-watcher incremental reindex path)
/// is a separate process that never calls this setter, so
/// `ALLOW_UNSANDBOXED_BY_CLI` stays `false` on every daemon-triggered reindex.
/// This is intentional: the daemon always fails closed unless the operator sets
/// `TRAVSR_ALLOW_UNSANDBOXED_LSIF=1` in the daemon's process environment — a
/// deliberate, auditable, per-environment decision that cannot be silently
/// inherited from a one-time `init` invocation.
///
/// If you need the daemon to run RA unconfined, set
/// `TRAVSR_ALLOW_UNSANDBOXED_LSIF=1` in the environment where the daemon is
/// launched (e.g. your shell profile or systemd unit file).
pub fn set_allow_unsandboxed_lsif(val: bool) {
    travsr_indexer::sandbox::set_cli_allow_unsandboxed(val);
}

/// Statistics returned by [`init_repo`] and displayed by `travsr init`.
#[derive(Debug, Default)]
pub struct InitStats {
    pub files_indexed: u64,
    /// Files skipped because their SHA-256 hash matched the stored value.
    pub files_skipped_unchanged: u64,
    /// Files skipped because they matched a `.travsrignore` / built-in rule.
    pub files_skipped_ignored: u64,
    /// Whether `.travsrignore` was freshly created on this run (first `travsr init`).
    pub travsrignore_scaffolded: bool,
    /// Net change in node count. `i64` to allow negative values if nodes are
    /// removed in the future (e.g. delete-by-file support); currently always >= 0.
    pub nodes_written: i64,
    pub edges_written: u64,
    /// Absolute node count after init (for "up to date" display on re-runs).
    pub total_nodes: u64,
    /// Absolute edge count after init (for "up to date" display on re-runs).
    pub total_edges: u64,
    /// Per-language Phase B outcome, populated by the full init path.
    pub phase_b_report: Option<PhaseBReport>,
}

/// Per-language Phase B outcome, surfaced in [`InitStats`] so the CLI can
/// tell the user which analyzers ran and which were absent.
#[derive(Debug, Default, Clone)]
pub struct PhaseBReport {
    /// Languages for which semantic analysis ran successfully.
    pub ran: Vec<String>,
    /// Languages P1-gated because no source files of that type exist in the
    /// repo. Not shown to the user — irrelevant when the language is absent.
    pub skipped_not_in_repo: Vec<String>,
    /// Languages present in the repo but with no semantic analyzer available.
    /// Shown to the user with a `travsr lang add` call-to-action.
    pub skipped_no_analyzer: Vec<String>,
    /// Languages registered in the resolver but not in lang.toml.
    pub skipped_unregistered: Vec<String>,
    /// Languages that are RequiresElevated but have no PSE approval in lang.toml.
    /// Shown to the user with a `travsr lang approve` call-to-action.
    pub skipped_needs_approval: Vec<String>,
    /// Languages whose analyzer spawned but died or errored mid-invoke.
    pub crashed: Vec<String>,
    /// Languages whose sidecar responded with a mismatched protocol version.
    /// Shown to the user with a `travsr lang install <lang>` call-to-action.
    /// Tuple: (language, expected_version, got_version).
    pub version_mismatch: Vec<(String, u32, u32)>,
}

/// Progress events emitted during [`init_repo_with_progress`] so a caller (the
/// CLI) can render a live indicator. Counts are best-effort. The callback runs
/// on the indexing thread, so keep it cheap and non-blocking.
#[derive(Debug, Clone, Copy)]
pub enum InitProgress {
    /// Walking the work tree to discover indexable files. `scanned` is the
    /// number of directory entries seen so far (the total is not yet known).
    Scanning { scanned: u64 },
    /// Phase A: parallel parse + batch write. `done`/`total` are files processed.
    /// `workers` is the thread count used.
    Indexing {
        done: u64,
        total: u64,
        workers: usize,
    },
    /// Post-index semantic passes (LSIF + Phase B); no granular count.
    /// Only emitted when `--semantic` is passed or there is no HEAD commit.
    Finalizing,
    /// Phase B deferred to the daemon background scheduler. Emitted on the
    /// normal (non-`--semantic`) path once Phase A completes successfully.
    PhaseBDeferred,
}

/// Initialise a Travsr index in `repo_root`:
/// 1. Create `.travsr/graph.db` with WAL-mode migrations.
/// 2. Install the `post-commit` git hook.
/// 3. Walk all `.ts`/`.tsx`/`.rs` files (honours `.gitignore`, skips `target/`)
///    and index them via the delta path so the `files` hash table is populated
///    from the start.
///
/// Outcome from one worker's parse of a single file.
struct ParseResult {
    file_graph: FileGraph,
    ffi_markers: Vec<FfiMarker>,
    /// `true` when the file was skipped (unchanged hash) — graph is empty.
    unchanged: bool,
}

/// Parallel Phase-A parse → batched write pipeline for `init_repo`.
///
/// # Data flow
/// ```text
/// preloaded hash map ─┐
///                     ├──► N worker threads (parse + link_imports, no store access)
///                     │       │  bounded mpsc channel
///                     │       ▼
///                     └── single writer thread (owns &mut SqliteStore)
///                              batches BATCH_SIZE results → write_file_graphs_batch
/// ```
///
/// `reindex_files` (commit hook) is NOT changed — this path is init-only.
/// That structurally preserves the full-reindex == incremental invariant.
/// Returns `(write_counts, files_skipped_unchanged)`.
fn index_paths_parallel(
    paths: &[PathBuf],
    repo_root: &Path,
    corpus: &str,
    jobs: usize,
    bulk: bool,
    store: &mut SqliteStore,
    on_progress: &mut dyn FnMut(InitProgress),
) -> anyhow::Result<(BatchWriteCounts, u64)> {
    use std::sync::mpsc;

    // 512 files per batch: fewer transactions than 64, WAL still stays bounded.
    // Bulk init skips per-node FTS5 writes so memory per batch stays modest.
    const BATCH_SIZE: usize = 512;

    let total = paths.len() as u64;
    if total == 0 {
        return Ok((BatchWriteCounts::default(), 0));
    }

    // Pre-load all stored hashes once — workers compare in-memory.
    let stored_hashes = store.get_all_file_hashes().unwrap_or_default();

    // Divide `paths` into `jobs` slices (last shard may be smaller).
    let shard_size = paths.len().div_ceil(jobs);

    let (tx, rx) = mpsc::sync_channel::<anyhow::Result<ParseResult>>(jobs * 4);

    // ── spawn worker threads ──────────────────────────────────────────────────
    // Each thread owns a PluginIndexer (cheap to construct, not Send).
    std::thread::scope(|scope| -> anyhow::Result<(BatchWriteCounts, u64)> {
        for shard in paths.chunks(shard_size) {
            let tx = tx.clone();
            let stored = &stored_hashes;
            let corpus = corpus.to_owned();
            let repo = repo_root.to_path_buf();

            scope.spawn(move || {
                let mut indexer = PluginIndexer::new(&corpus);
                for abs_path in shard {
                    let vname_path = abs_path
                        .strip_prefix(&repo)
                        .unwrap_or(abs_path)
                        .to_string_lossy()
                        .replace('\\', "/");

                    // Hash-delta skip — same as reindex_files.
                    let new_hash = match hash_file(abs_path) {
                        Ok(h) => h,
                        Err(e) => {
                            tracing::warn!(path=%abs_path.display(), err=%e, "hash failed, skipping");
                            continue;
                        }
                    };
                    let new_hex = hex_encode(&new_hash);
                    if stored.get(&vname_path).map(String::as_str) == Some(&new_hex) {
                        let _ = tx.send(Ok(ParseResult {
                            file_graph: FileGraph {
                                vname_path,
                                new_hash: new_hex,
                                nodes: vec![],
                                edges: vec![],
                            },
                            ffi_markers: vec![],
                            unchanged: true,
                        }));
                        continue;
                    }

                    // Parse Phase A.
                    let out = match indexer.parse_file_with_vname(abs_path, &vname_path) {
                        Ok(o) => o,
                        Err(e) => {
                            tracing::warn!(path=%abs_path.display(), err=%e, "parse error, skipping");
                            continue;
                        }
                    };

                    // Import resolution (read-only FS, no store access).
                    let ext = abs_path.extension().and_then(|e| e.to_str()).unwrap_or("");
                    let import_edges = match Language::from_extension(ext) {
                        Some(Language::TypeScript) => {
                            link_imports(&out.nodes, &vname_path, &corpus)
                        }
                        Some(Language::Rust) => {
                            link_imports_rust(&out.nodes, &vname_path, &corpus)
                        }
                        Some(Language::Python) => {
                            link_imports_python_fs(&out.nodes, &vname_path, &corpus, &repo)
                        }
                        Some(Language::Go) => {
                            link_imports_go(&out.nodes, &vname_path, &corpus, None)
                        }
                        _ => vec![],
                    };

                    let mut edges = out.edges;
                    edges.extend(import_edges);

                    let _ = tx.send(Ok(ParseResult {
                        file_graph: FileGraph {
                            vname_path,
                            new_hash: new_hex,
                            nodes: out.nodes,
                            edges,
                        },
                        ffi_markers: out.ffi_markers,
                        unchanged: false,
                    }));
                }
            });
        }
        // Drop the sender owned by this scope so `rx` sees EOF when all workers exit.
        drop(tx);

        // ── writer thread: drain channel, batch-write ─────────────────────────
        let mut counts = BatchWriteCounts::default();
        let mut files_skipped_unchanged: u64 = 0;
        let mut batch: Vec<FileGraph> = Vec::with_capacity(BATCH_SIZE);
        let mut all_ffi_markers: Vec<FfiMarker> = Vec::new();

        for (done, result) in (1_u64..).zip(rx) {
            let pr = result?;

            if pr.unchanged {
                files_skipped_unchanged += 1;
                // Emit progress even for unchanged files so the bar moves.
                on_progress(InitProgress::Indexing {
                    done,
                    total,
                    workers: jobs,
                });
                continue;
            }

            all_ffi_markers.extend(pr.ffi_markers);
            batch.push(pr.file_graph);

            if batch.len() >= BATCH_SIZE {
                let written = store.write_file_graphs_batch(&batch, bulk)?;
                counts.nodes_upserted += written.nodes_upserted;
                counts.edges_upserted += written.edges_upserted;
                counts.files_written += written.files_written;
                batch.clear();
            }

            on_progress(InitProgress::Indexing {
                done,
                total,
                workers: jobs,
            });
        }

        // Flush remaining files.
        if !batch.is_empty() {
            let written = store.write_file_graphs_batch(&batch, bulk)?;
            counts.nodes_upserted += written.nodes_upserted;
            counts.edges_upserted += written.edges_upserted;
            counts.files_written += written.files_written;
        }

        // Repo-level FFI resolution (same logic as reindex_files).
        if !all_ffi_markers.is_empty() {
            let indexer = PluginIndexer::new(corpus);
            let ffi_edges = indexer.resolve_ffi_edges(&all_ffi_markers);
            for edge in &ffi_edges {
                if let Err(e) = store.put_edge(edge) {
                    tracing::warn!(err=%e, "ffi edge write error");
                }
                counts.edges_upserted += 1;
            }
        }

        Ok((counts, files_skipped_unchanged))
    })
}

/// Built-in default ignore patterns written into the scaffolded `.travsrignore`.
///
/// Precedence: SKIP_DIRS (hard, non-overridable) < these defaults (soft, user can
/// negate with `!pattern` in `.travsrignore`) < any additional user rules.
const DEFAULT_TRAVSRIGNORE: &str = "\
# .travsrignore — gitignore-syntax exclusions for Travsr graph indexing.
# Patterns here are additive to .gitignore. Negate with ! to re-include.
# Generated by `travsr init` — safe to edit.

# Third-party vendored dependencies
vendor/
# Common build output directories
build/
# Generated code
**/generated/
**/*.pb.go
**/testdata/
";

/// Number of default rules in [`DEFAULT_TRAVSRIGNORE`] (for the summary line).
const DEFAULT_TRAVSRIGNORE_RULE_COUNT: usize = 5;

/// Write `.travsrignore` with commented defaults if it does not already exist.
///
/// Idempotent: never overwrites an existing file.  Reports whether the file was
/// freshly created so `init_repo` can mention it in the summary.
fn scaffold_travsrignore(repo_root: &Path) -> anyhow::Result<bool> {
    let path = repo_root.join(".travsrignore");
    if path.exists() {
        return Ok(false);
    }
    std::fs::write(&path, DEFAULT_TRAVSRIGNORE)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}

/// Top-level directory names that are well-known source roots, never dep/vendor dirs.
/// Auto-exclusion never fires for these regardless of file count.
const KNOWN_SOURCE_DIRS: &[&str] = &[
    "src",
    "lib",
    "pkg",
    "internal",
    "cmd",
    "api",
    "test",
    "tests",
    "app",
    "apps",
    "plugins",
    "modules",
    "services",
    "components",
    "core",
    "common",
    "shared",
    "utils",
    "crates",
    "staging",
    "hack",
    "cluster",
    "docs",
    "examples",
    "samples",
];

/// Heuristic: a single directory holding ≥ 1 000 source-language files AND
/// ≥ 15 % of the total discovered source files is flagged as a "large dep dir",
/// unless the directory name is in `KNOWN_SOURCE_DIRS`.
///
/// Returns `(dir_name, file_count, total_count)` for the first such directory
/// that is not already excluded by the walker (SKIP_DIRS or .travsrignore).
fn detect_large_dep_dir(indexable: &[PathBuf], repo_root: &Path) -> Option<(String, u64, u64)> {
    use std::collections::HashMap;

    let total = indexable.len() as u64;
    if total == 0 {
        return None;
    }

    let mut top_counts: HashMap<String, u64> = HashMap::new();
    for p in indexable {
        if let Some(first) = p.strip_prefix(repo_root).ok().and_then(|r| {
            r.components()
                .next()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
        }) {
            *top_counts.entry(first).or_insert(0) += 1;
        }
    }

    for (dir, count) in top_counts {
        if KNOWN_SOURCE_DIRS.contains(&dir.as_str()) {
            continue;
        }
        let pct = count * 100 / total;
        if count >= 1_000 && pct >= 15 {
            return Some((dir, count, total));
        }
    }
    None
}

/// If stderr is a TTY, prompt the user once to exclude a detected large dep dir.
/// If non-TTY / CI, auto-exclude and log the decision without blocking.
///
/// Appends the rule to `.travsrignore` if the user accepts (or in CI mode).
/// Returns `true` if a rule was appended (caller should re-build the walker).
fn maybe_prompt_large_dep(repo_root: &Path, dir: &str, count: u64, total: u64) -> bool {
    use std::io::{IsTerminal, Write};

    let pct = count * 100 / total;
    let is_tty = std::io::stderr().is_terminal();

    let exclude = if is_tty {
        let mut err = std::io::stderr().lock();
        let _ = write!(
            err,
            "\nDetected {dir}/ ({count} files, ~{pct}% of repo). \
             Exclude from index? [Y/n] "
        );
        let _ = err.flush();
        drop(err);
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        let answer = line.trim().to_ascii_lowercase();
        answer.is_empty() || answer == "y" || answer == "yes"
    } else {
        tracing::info!(
            dir = %dir,
            count,
            pct,
            "non-TTY: auto-excluding large dep dir from index (add !{dir}/ to .travsrignore to override)"
        );
        true
    };

    if exclude {
        let path = repo_root.join(".travsrignore");
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
        {
            let _ = writeln!(f, "{dir}/");
        }
    }
    exclude
}

/// Compute and persist `embed_text` for all nodes where it is currently NULL.
///
/// `richness` controls how much context is packed per node — derived from the
/// installed embed model's `params_m` at call time. Pass `EmbedRichness::Compact`
/// when no model is installed (safe default; regenerated at first reindex).
fn update_embed_texts(store: &mut SqliteStore, repo_root: &Path, richness: EmbedRichness) {
    let nodes = match store.nodes_missing_embed_text() {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("update_embed_texts: failed to query missing nodes: {e}");
            return;
        }
    };
    if nodes.is_empty() {
        return;
    }
    tracing::info!(
        count = nodes.len(),
        ?richness,
        "regenerating embed_text (parse-once-per-file, parallel)"
    );

    // Canonicalize the repo root once (skeleton_for_node did it per node).
    let canon_root = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf());

    // Group nodes by source file so each file is tree-sitter-parsed exactly once
    // instead of once per symbol — the O(nodes) re-parse that made this ~20 min on
    // a 130k-node repo (a file with N symbols was read + parsed N times).
    let mut by_file: std::collections::HashMap<&str, Vec<&travsr_core::Node>> =
        std::collections::HashMap::new();
    // Package/external nodes have path="" — collect separately for direct text gen.
    let mut pathless: Vec<&travsr_core::Node> = Vec::new();
    for node in &nodes {
        if !node.vname.path.is_empty() {
            by_file
                .entry(node.vname.path.as_str())
                .or_default()
                .push(node);
        } else {
            pathless.push(node);
        }
    }
    let files: Vec<(&str, Vec<&travsr_core::Node>)> = by_file.into_iter().collect();
    // NB: do NOT early-return when `files` is empty — pathless package nodes
    // (path="") are handled below and would otherwise be stranded without
    // embed_text whenever they are the only nodes missing it.

    // Parse + build text in parallel across files (tree-sitter parsing is CPU-bound,
    // so this scales with cores — the reason a single-threaded regen only used 1 CPU).
    let nthreads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 8);
    let files_ref = &files;
    let canon_ref = canon_root.as_path();
    let pairs: Vec<(travsr_core::NodeId, String)> = if files.is_empty() {
        Vec::new()
    } else {
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..nthreads)
                .map(|t| {
                    scope.spawn(move || {
                        let mut local = Vec::new();
                        let mut i = t;
                        while i < files_ref.len() {
                            let (path, fnodes) = &files_ref[i];
                            local.extend(embed_texts_for_file(
                                repo_root, canon_ref, path, fnodes, richness,
                            ));
                            i += nthreads;
                        }
                        local
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().unwrap_or_default())
                .collect()
        })
    };

    // Generate embed text for pathless package nodes (pkg:foo@v1 → "foo v1").
    let mut pairs = pairs;
    for node in &pathless {
        if node.kind == "package" {
            let text = node
                .vname
                .signature
                .trim_start_matches("pkg:")
                .replace(['@', '/', ':'], " ");
            pairs.push((node.id, text));
        }
    }

    // Write path is single-threaded SQLite; batch to keep transactions small.
    for chunk in pairs.chunks(500) {
        if let Err(e) = store.write_embed_texts_batch(chunk) {
            tracing::warn!("update_embed_texts: batch write failed: {e}");
        }
    }
}

/// Derive the richness tier for the model currently configured in this repo.
/// Returns `Compact` when no model is configured (safe default).
fn richness_from_meta(repo_root: &Path) -> EmbedRichness {
    travsr_plugin_host::repo_backend_id(repo_root)
        .and_then(|id| travsr_plugin_host::lookup_embed_backend(&id))
        .map(|b| EmbedRichness::from_params_m(b.params_m))
        .unwrap_or(EmbedRichness::Compact)
}

/// If the active embed model differs from what generated the stored `embed_text`,
/// NULL all embed_text rows, regenerate with the correct richness, and update
/// the meta key — ensuring the sidecar always sees correctly-tiered text.
///
/// The meta key is written AFTER regeneration so a crash mid-way leaves the key
/// pointing to the old model and triggers a clean retry on the next call.
///
/// Returns `true` when regeneration was performed.
pub fn regenerate_embed_texts_if_stale(db_path: &Path) -> anyhow::Result<bool> {
    let repo_root = db_path
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(|| anyhow::anyhow!("cannot derive repo_root from db_path"))?;
    // Use the PER-REPO configured model, not the global active.
    // If this repo has not been configured via `travsr embed init`, skip silently.
    let active_id = match travsr_plugin_host::repo_backend_id(repo_root) {
        Some(id) => id,
        None => return Ok(false),
    };
    let backend = match travsr_plugin_host::lookup_embed_backend(&active_id) {
        Some(b) => b,
        None => return Ok(false),
    };

    let mut store = travsr_store::SqliteStore::open(db_path)
        .context("opening store for embed_text regeneration")?;

    let stored_id = store.get_meta("embed_text_model_id").ok().flatten();
    if stored_id.as_deref() == Some(active_id.as_str()) {
        // Model unchanged — still populate embed_text for any nodes indexed since
        // the last embed pass (e.g. data-format nodes added by a code change).
        let richness = EmbedRichness::from_params_m(backend.params_m);
        update_embed_texts(&mut store, repo_root, richness);
        return Ok(false);
    }

    let richness = EmbedRichness::from_params_m(backend.params_m);
    tracing::info!(
        old = ?stored_id,
        new = %active_id,
        ?richness,
        "embed model changed — regenerating embed_text"
    );

    // NULL all rows so update_embed_texts picks them all up.
    store
        .clear_all_embed_texts()
        .context("clearing embed_text for model tier change")?;

    update_embed_texts(&mut store, repo_root, richness);

    // Write the new model_id only after successful regeneration.
    store
        .set_meta("embed_text_model_id", &active_id)
        .context("writing embed_text_model_id")?;

    Ok(true)
}

/// Graph integrity check and optional repair — the `travsr fsck` entry point.
///
/// Walks the repo, computes the ghost set (paths tracked in the DB but absent
/// on disk), and reports them. With `fix=true`, deletes ghosts and sweeps
/// orphan edges. With `force=true`, overrides the mass-delete circuit breaker.
///
/// Returns a [`travsr_core::GcReport`] suitable for human or JSON output.
pub fn fsck_repo(
    repo_root: &Path,
    fix: bool,
    force: bool,
) -> anyhow::Result<travsr_core::GcReport> {
    let db_path = repo_root.join(".travsr/graph.db");
    anyhow::ensure!(
        db_path.exists(),
        "no graph.db found at {} — run `travsr init` first",
        db_path.display()
    );

    let mut store =
        SqliteStore::open(&db_path).with_context(|| format!("opening {}", db_path.display()))?;

    let corpus = store.get_meta("corpus")?.unwrap_or_default();

    let mut report = travsr_core::GcReport {
        node_count: store.node_count()?,
        edge_count: store.edge_count()?,
        ..travsr_core::GcReport::default()
    };

    // Walk the disk (follow_links=false per §6.5 S4, matching the watcher).
    let mut walked_paths = std::collections::HashSet::<String>::new();
    for entry in ignore::WalkBuilder::new(repo_root)
        .follow_links(false)
        .build()
        .flatten()
    {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let rel = path.strip_prefix(repo_root).unwrap_or(path);
        if rel
            .components()
            .any(|c| watcher::SKIP_DIRS.iter().any(|skip| c.as_os_str() == *skip))
        {
            continue;
        }
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if travsr_core::Language::from_extension(ext).is_none() {
            continue;
        }
        walked_paths.insert(rel.to_string_lossy().replace('\\', "/"));
    }

    // Compute ghost set: in DB but absent on disk.
    let db_hashes = store.get_all_file_hashes()?;
    let db_paths: std::collections::HashSet<String> = db_hashes.into_keys().collect();
    let ghosts: Vec<String> = db_paths.difference(&walked_paths).cloned().collect();
    report.ghost_paths = ghosts.clone();

    if !fix {
        return Ok(report);
    }

    let policy = if force {
        travsr_core::SafetyPolicy {
            mass_delete_ceiling_pct: 1.0,
            ..Default::default()
        }
    } else {
        travsr_core::SafetyPolicy::default()
    };

    // reconcile enforces the circuit breaker, TOCTOU re-check, and batched deletes.
    let mut fix_report = store.reconcile(&walked_paths, &policy, repo_root, &corpus)?;

    // Sweep any residual orphan edges (Tier-4 defect indicator).
    let orphans = store.sweep_orphans()?;
    fix_report.orphan_edges_swept = orphans;
    if orphans > 0 {
        tracing::warn!(
            orphans,
            "fsck: non-zero orphan edge count — write-path invariant violated"
        );
    }

    Ok(fix_report)
}

pub fn init_repo(repo_root: &Path) -> anyhow::Result<InitStats> {
    init_repo_with_progress(repo_root, None, false, &mut |_| {})
}

/// Like [`init_repo`], but reports progress via `on_progress` so the CLI can
/// show that a long indexing run is alive (issue #293). The callback is invoked
/// on the indexing thread; keep it cheap.
///
/// `jobs` sets the parse-worker count (`None` = `available_parallelism()`).
///
/// `semantic` forces Phase B to run synchronously before returning, matching
/// the pre-deferred-Phase-B behaviour. Use this for CI / scripts that query
/// call edges immediately after init. When `false` (the default), Phase B is
/// deferred to the daemon background scheduler.
pub fn init_repo_with_progress(
    repo_root: &Path,
    jobs: Option<usize>,
    semantic: bool,
    on_progress: &mut dyn FnMut(InitProgress),
) -> anyhow::Result<InitStats> {
    // M3: canonicalize so ~/.travsr/registry.json never gets two entries for the
    // same repo (e.g. `/home/user/proj` vs `/home/user/proj/`).
    let repo_root = &repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf());

    let travsr_dir = repo_root.join(".travsr");
    std::fs::create_dir_all(&travsr_dir).context("creating .travsr directory")?;

    // C2: cross-process init lock — prevents two concurrent `travsr init` runs
    // (two terminals, CI + local) from writing the same graph.db simultaneously.
    // Uses an exclusive flock so the second caller blocks until the first
    // finishes, then proceeds (incremental re-init is idempotent).
    let init_lock_path = travsr_dir.join("init.lock");
    let init_lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&init_lock_path)
        .with_context(|| format!("opening {}", init_lock_path.display()))?;
    fs2::FileExt::lock_exclusive(&init_lock_file)
        .context("acquiring init.lock — another `travsr init` may be running for this repo")?;

    let db_path = travsr_dir.join("graph.db");
    let mut store = SqliteStore::open(&db_path).with_context(|| {
        // M5: distinguish corruption from other open failures so the user knows
        // whether to re-run init (corrupt) vs fix disk space (full).
        let hint = if db_path.exists() {
            " — if graph.db is corrupted, delete it and re-run `travsr init`"
        } else {
            ""
        };
        format!("opening {}{hint}", db_path.display())
    })?;

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
    // SEC (Windows): mirror the Unix 0o600 restriction via icacls.
    // /inheritance:r strips all inherited ACEs; /grant:r grants Full Control to
    // the current user only. icacls ships with every Windows install since Vista.
    // USERDOMAIN\USERNAME is used rather than USERNAME alone to avoid ambiguity
    // on domain-joined machines where a local and domain user share the same name.
    #[cfg(windows)]
    'acl: {
        let Some(path_str) = db_path.to_str() else {
            tracing::warn!(
                path = %db_path.display(),
                "graph.db path is not valid UTF-8 — skipping icacls permission restriction"
            );
            break 'acl;
        };
        let user = std::env::var("USERNAME").unwrap_or_default();
        let domain = std::env::var("USERDOMAIN").unwrap_or_default();
        if user.is_empty() {
            tracing::warn!(
                path = %db_path.display(),
                "USERNAME env var not set — skipping graph.db permission restriction on Windows"
            );
            break 'acl;
        }
        let account = if domain.is_empty() {
            user
        } else {
            format!("{domain}\\{user}")
        };
        let status = std::process::Command::new("icacls")
            .args([
                path_str,
                "/inheritance:r",
                "/grant:r",
                &format!("{account}:(F)"),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => tracing::warn!(
                path = %db_path.display(),
                exit_code = ?s.code(),
                "icacls failed to restrict graph.db permissions — file may be readable by other users on this machine"
            ),
            Err(e) => tracing::warn!(
                path = %db_path.display(),
                err = %e,
                "icacls not available — graph.db permissions not restricted on Windows"
            ),
        }
    }

    install_hook(repo_root)?;

    // GHOST-NODE PURGE: directories added to SKIP_DIRS after a previous run may
    // have left stale nodes in graph.db. The hash-delta loop skips files that
    // no longer exist on disk, so those nodes are never tombstoned incrementally.
    // Purge before the walk so no ghost nodes survive a re-init.
    for &skip_dir in crate::watcher::SKIP_DIRS {
        let prefix = format!("{skip_dir}/");
        match store.delete_nodes_for_path_prefix(&prefix) {
            Ok(0) => {}
            Ok(n) => tracing::info!(skip_dir, purged = n, "purged ghost nodes for skip dir"),
            Err(e) => tracing::warn!(skip_dir, err = %e, "ghost-node purge failed (non-fatal)"),
        }
    }

    // SC-H1: cap node_tombstones before indexing. Full init rebuilds embeddings
    // from scratch so tombstones older than 7 days are redundant. Hard ceiling of
    // 50 k rows prevents the table from growing unbounded if the embed sidecar
    // has been offline long-term.
    const TOMBSTONE_MAX_AGE_SECS: u64 = 7 * 24 * 3600;
    const TOMBSTONE_MAX_ROWS: u64 = 50_000;
    if let Err(e) = store.prune_tombstones(TOMBSTONE_MAX_AGE_SECS, TOMBSTONE_MAX_ROWS) {
        tracing::warn!("tombstone GC failed (non-fatal): {e}");
    }

    // Register in global registry so `travsr mcp --global` can find this repo.
    // TRAVSR_DISABLE_REGISTRY=1 bypasses registration — set in tests and CI to
    // prevent temp-dir paths polluting ~/.travsr/registry.json.
    if std::env::var("TRAVSR_DISABLE_REGISTRY").as_deref() != Ok("1") {
        // M3: use the canonicalized absolute path as the registry key so
        // `~/proj` and `/home/user/proj` never create duplicate entries.
        let registry_key = repo_root.to_string_lossy().into_owned();
        if let Err(e) = travsr_store::registry::register(&registry_key, &db_path) {
            tracing::warn!("registry update failed (non-fatal): {e}");
        }
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
    let stored_corpus = store.get_meta("corpus").ok().flatten().unwrap_or_default();
    let corpus = detect_corpus(repo_root);

    // §5 #12 / G5: corpus change (git remote changed) means every NodeId is
    // different — the old corpus string is baked into every BLAKE3 hash. Purge
    // all graph data so the new init starts clean rather than mixing id spaces.
    if nodes_before > 0 && !stored_corpus.is_empty() && stored_corpus != corpus {
        tracing::warn!(
            old = %stored_corpus,
            new = %corpus,
            "corpus changed — purging all graph data for clean rebuild (§5 #12)"
        );
        let empty_walked = std::collections::HashSet::<String>::new();
        let purge_policy = travsr_core::SafetyPolicy {
            mass_delete_ceiling_pct: 1.0,
            ..Default::default()
        };
        store
            .reconcile(&empty_walked, &purge_policy, repo_root, &stored_corpus)
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context("corpus-change global-invalidation purge")?;
        tracing::info!("corpus-change purge complete — rebuilding from scratch");
    }

    store
        .set_meta("corpus", &corpus)
        .context("writing corpus to meta (ARCH-102)")?;
    tracing::debug!("corpus for {}: {corpus}", repo_root.display());

    // Persist repo_root so MCP snippet tools can resolve vname.path → absolute
    // path at query time without threading repo_root through function signatures.
    if let Some(root_str) = repo_root.to_str() {
        store
            .set_meta("repo_root", root_str)
            .context("writing repo_root to meta")?;
    } else {
        // Non-UTF-8 repo paths are valid on Linux (ext4 allows arbitrary bytes)
        // but cannot be stored in the meta table. Snippet retrieval will degrade
        // to metadata-only output and prompt the user to re-init. Warn so the
        // cause is visible in logs rather than silently degrading.
        tracing::warn!(
            path = %repo_root.display(),
            "repo_root contains non-UTF-8 bytes — snippet tool will be unavailable for this repo"
        );
    }

    // T4 (1b): scaffold .travsrignore before the walker reads it so default
    // patterns are active on the very first `travsr init` run.
    let scaffolded = scaffold_travsrignore(repo_root).unwrap_or(false);
    if scaffolded {
        tracing::info!("wrote .travsrignore ({DEFAULT_TRAVSRIGNORE_RULE_COUNT} default rules)");
    }

    let walker = WalkBuilder::new(repo_root)
        .hidden(false)
        .git_ignore(true)
        .follow_links(false)
        // T3 (1a): honor .travsrignore (gitignore-syntax) in addition to .gitignore.
        // Precedence: SKIP_DIRS (hard) < .gitignore < .travsrignore (user wins).
        .add_custom_ignore_filename(".travsrignore")
        .build();

    let mut indexable_paths: Vec<PathBuf> = Vec::new();
    // P1 (#322): collected during the walk so Phase B skips sidecars for absent
    // languages without spawning a single subprocess for them.
    let mut present_languages: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut scanned: u64 = 0;
    for entry in walker {
        // Emit a scanning tick periodically so a large tree does not look hung
        // during discovery; the index loop below reports exact file counts.
        scanned += 1;
        if scanned % 1024 == 0 {
            on_progress(InitProgress::Scanning { scanned });
        }
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
        if rel.components().any(|c| {
            crate::watcher::SKIP_DIRS
                .iter()
                .any(|skip| c.as_os_str() == *skip)
        }) {
            continue;
        }
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
        if let Some(lang) = Language::from_extension(ext) {
            present_languages.insert(lang.as_str().to_string());
            indexable_paths.push(p);
        }
    }

    // L13: warn if a rebase is in progress — init during rebase risks indexing
    // conflict-marker noise into graph.db; the user should finish rebasing first.
    if repo_root.join(".git").join("REBASE_HEAD").exists() {
        eprintln!(
            "warning: a git rebase is in progress — consider finishing or aborting it \
             before running `travsr init` to avoid indexing conflict markers"
        );
    }

    // T4 (1c): detect a large un-excluded dep dir and prompt/auto-exclude it.
    // If the user accepts, re-discover so the excluded files are dropped.
    if let Some((dir, count, total)) = detect_large_dep_dir(&indexable_paths, repo_root) {
        let appended = maybe_prompt_large_dep(repo_root, &dir, count, total);
        if appended {
            // Re-build the walker and re-discover now that .travsrignore is updated.
            let walker2 = WalkBuilder::new(repo_root)
                .hidden(false)
                .git_ignore(true)
                .follow_links(false)
                .add_custom_ignore_filename(".travsrignore")
                .build();
            indexable_paths.clear();
            present_languages.clear();
            for entry in walker2.flatten() {
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    continue;
                }
                let p = entry.into_path();
                let rel = p.strip_prefix(repo_root).unwrap_or(&p);
                if rel.components().any(|c| {
                    crate::watcher::SKIP_DIRS
                        .iter()
                        .any(|skip| c.as_os_str() == *skip)
                }) {
                    continue;
                }
                let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
                if let Some(lang) = Language::from_extension(ext) {
                    present_languages.insert(lang.as_str().to_string());
                    indexable_paths.push(p);
                }
            }
        }
    }

    // M10: warn before spending minutes indexing when the file count is unusually
    // large — likely a missing .gitignore / .travsrignore entry for generated code.
    const LARGE_REPO_THRESHOLD: usize = 200_000;
    if indexable_paths.len() > LARGE_REPO_THRESHOLD {
        eprintln!(
            "warning: {} source files found — indexing may take several minutes. \
             Add large generated directories to .travsrignore to speed up future runs.",
            indexable_paths.len()
        );
    }

    // Count source files the walker would have found without any ignore rules so
    // we can surface how many were excluded by .gitignore / .travsrignore in the
    // terminal summary. This walk does no I/O (stat only) so it is fast.
    let source_files_without_ignore = WalkBuilder::new(repo_root)
        .hidden(false)
        .git_ignore(false)
        .follow_links(false)
        .build()
        .flatten()
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .filter(|e| {
            let rel = e.path().strip_prefix(repo_root).unwrap_or(e.path());
            !rel.components().any(|c| {
                crate::watcher::SKIP_DIRS
                    .iter()
                    .any(|skip| c.as_os_str() == *skip)
            })
        })
        .filter(|e| {
            Language::from_extension(e.path().extension().and_then(|x| x.to_str()).unwrap_or(""))
                .is_some()
        })
        .count() as u64;
    let files_skipped_ignored =
        source_files_without_ignore.saturating_sub(indexable_paths.len() as u64);

    let jobs = jobs.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    });

    // Bulk-init mode: skip fsync on WAL writes + expand page cache for the
    // duration of indexing. Safe because `travsr init` is always re-runnable.
    // L10 note: SQLite pragmas (synchronous, cache_size) are connection-scoped —
    // they reset automatically when `store` drops, so early `?` returns are safe.
    store
        .set_bulk_init_mode(true)
        .context("enabling bulk init mode")?;
    store
        .begin_bulk_fts_tracking()
        .context("creating bulk FTS tracking table")?;
    // Only activate staging on a fresh DB (node_count == 0).
    // Re-init of an existing repo falls back to the incremental path so that
    // stale nodes for changed files are deleted before re-insertion and the
    // FTS index is not corrupted by duplicate rowids.
    if store.node_count().unwrap_or(0) == 0 {
        store
            .begin_staging_tables()
            .context("creating staging temp tables")?;
    }

    let edges_before = store.edge_count().unwrap_or(0);
    let t_parse = std::time::Instant::now();
    let index_result = index_paths_parallel(
        &indexable_paths,
        repo_root,
        &corpus,
        jobs,
        true,
        &mut store,
        on_progress,
    );
    tracing::info!(
        elapsed_ms = t_parse.elapsed().as_millis(),
        "TIMING: index_paths_parallel done"
    );

    // Flush staging tables → production in one deduplicating GROUP BY pass.
    // Must happen before rebuild_fts_from_map, which reads nodes_fts_map rows
    // written during the staging phase and joins them against production nodes.
    let t_flush = std::time::Instant::now();
    if index_result.is_ok() {
        let (nodes_written, edges_written) = store
            .flush_staging_to_production()
            .context("flushing staging tables to production")?;
        tracing::info!(
            elapsed_ms = t_flush.elapsed().as_millis(),
            nodes = nodes_written,
            edges = edges_written,
            "TIMING: flush_staging_to_production done"
        );
    }

    // Rebuild FTS + vocab in one pass now that all nodes are written.
    // Do this before restoring pragmas so the rebuild benefits from the
    // expanded cache and synchronous=OFF.
    let t_fts = std::time::Instant::now();
    if index_result.is_ok() {
        store
            .rebuild_fts_from_map()
            .context("rebuilding FTS after bulk init")?;
    }
    tracing::info!(
        elapsed_ms = t_fts.elapsed().as_millis(),
        "TIMING: rebuild_fts_from_map done"
    );

    // Always restore pragmas — even on error — so the store is left in a
    // consistent state if the caller catches the error and continues.
    let t_pragma = std::time::Instant::now();
    store
        .set_bulk_init_mode(false)
        .context("restoring sync mode after bulk init")?;
    store
        .run_pragma_optimize()
        .context("PRAGMA optimize after bulk init")?;
    tracing::info!(
        elapsed_ms = t_pragma.elapsed().as_millis(),
        "TIMING: set_bulk_init_mode(false) + optimize done"
    );

    // M4: translate SQLITE_FULL into an actionable message so the user knows
    // exactly what to do rather than seeing a raw SQLite error code.
    let (batch_counts, files_skipped_unchanged) = index_result.map_err(|e| {
        let msg = e.to_string();
        if msg.contains("disk I/O error")
            || msg.contains("database or disk is full")
            || msg.contains("SQLITE_FULL")
        {
            anyhow::anyhow!("disk is full — free space and re-run `travsr init` (original: {e:#})")
        } else {
            e
        }
    })?;

    let nodes_after = store.node_count().context("counting nodes after init")? as i64;

    // Decide whether to run Phase B inline now, defer it, or skip it entirely.
    //
    // Already-done path: `phase_b_commit == HEAD` means Phase B is current for
    // this commit (e.g. a previous `--semantic` run or a completed background
    // refresh). No message, no daemon spawn — silently return a dummy report so
    // the caller knows Phase B is not pending.
    //
    // Deferred path (default): Phase B runs in the background via the daemon's
    // `run_background_phase_b` once the user's IDE / agent starts it. The
    // `phase_b_commit` meta key is intentionally left unset so the daemon's
    // `phase_b_tick` auto-arms the scheduler on startup.
    //
    // Inline path (`--semantic` flag, or repo has no HEAD commit):
    //   • `--semantic`: callers (CI, scripts) need call edges before querying.
    //   • No commit: `run_background_phase_b` bails when `last_commit` is empty,
    //     so there is no deferred path available for fresh repos.
    let current_sha = read_head_commit_sha(repo_root).unwrap_or_default();
    let has_commit = !current_sha.is_empty();
    let phase_b_commit_stored = store
        .get_meta("phase_b_commit")
        .ok()
        .flatten()
        .unwrap_or_default();
    let phase_b_already_done = !current_sha.is_empty() && phase_b_commit_stored == current_sha;
    let run_phase_b_inline = semantic || !has_commit;

    let phase_b_report = if run_phase_b_inline {
        on_progress(InitProgress::Finalizing);

        // LSIF semantic pass — adds RefCall edges on top of structural edges.
        // DEBT(travsr-25): whole-project re-emit; file-level delta is Phase 3.
        let t_lsif = std::time::Instant::now();
        run_lsif_pass(repo_root, &corpus, &mut store);
        tracing::info!(
            elapsed_ms = t_lsif.elapsed().as_millis(),
            "TIMING: run_lsif_pass done"
        );

        // Phase B — deep semantic analysis via sidecar plugins (RFC-011 §3).
        let t_phase_b = std::time::Instant::now();
        let report = {
            // R1: reset the per-Phase-B skip latch before each run so a previous
            // skip on another repo doesn't produce a false degradation flag for
            // this repo's write_phase_b_results call.
            travsr_indexer::sandbox::reset_ra_lsif_sandbox_skip();
            let phase_b_indexer = travsr_plugin_host::PluginIndexer::new(&corpus);
            let inputs = travsr_plugin_host::PhaseBInputs {
                repo_root,
                present_languages: present_languages.clone(),
                // P6 (#329): reuse the already-walked file list so Phase B runners
                // skip their own directory walks.
                indexable_paths: &indexable_paths,
            };
            let (pb_nodes, mut pb_edges, pb_refs, pb_unresolved, pb_outcome) =
                phase_b_indexer.invoke_phase_b_all(&inputs);
            let (resolved, resolved_sites) = resolve_unresolved_calls(&store, &pb_unresolved);
            tracing::debug!(
                resolved_cross_crate_edges = resolved.len(),
                "Phase B UnresolvedCall resolution complete"
            );
            pb_edges.extend(resolved);
            let report =
                write_phase_b_results(&mut store, &corpus, pb_nodes, pb_edges, pb_refs, pb_outcome);
            // #299 WS-4: record cross-crate call occurrence lines after the edges
            // (and their callee nodes) are in the store.
            if let Err(e) = store.record_edge_sites(&resolved_sites) {
                tracing::warn!("recording cross-crate edge_sites: {e:#}");
            }
            report
        };
        tracing::info!(
            elapsed_ms = t_phase_b.elapsed().as_millis(),
            "TIMING: invoke_phase_b_all done"
        );
        Some(report)
    } else if phase_b_already_done {
        // Phase B is current for this commit — nothing to do, no message.
        // Return Some(empty) so init.rs skips the daemon spawn.
        Some(PhaseBReport::default())
    } else {
        on_progress(InitProgress::PhaseBDeferred);
        None
    };

    // ── Co-package Depends pass ────────────────────────────────────────────────
    // Languages where files share a namespace without explicit imports (Go,
    // Swift, Kotlin, Java, Dart) emit a package/module node during Phase A
    // that every co-package file points at via DefinesBinding. A single SQL
    // self-join finds all such groups and writes file_B --Depends--> file_A
    // for every ordered pair, giving blast-radius BFS structural coupling.
    //
    // DEBT-024: reindex_files (commit hook) does not run this pass.
    {
        const COPKG_KINDS: &[&str] = &[
            "go-pkg",
            "swift-module",
            "java-package",
            "kotlin-package",
            "dart-library",
        ];
        match store.emit_copackage_depends(COPKG_KINDS) {
            Ok(count) if count > 0 => tracing::info!(
                count,
                "co-package pass: emitted intra-package Depends edges"
            ),
            Ok(_) => {}
            Err(e) => tracing::warn!("co-package pass: {e:#}"),
        }
    }

    // Capture edges_written AFTER the LSIF pass so RefCall edges are included
    // in the delta shown by `travsr init` on first run.
    let edges_written = store.edge_count().unwrap_or(0).saturating_sub(edges_before);

    // Always stamp last_commit after init — independent of whether any file
    // changed. Fixes the regression where reindex_files's any_changed guard
    // suppressed the stamp on clean re-runs (PR #207).
    if let Ok(sha) = read_head_commit_sha(repo_root) {
        let _ = store.set_meta("last_commit", &sha);
        // C4: phase_b_commit is stamped only when Phase B ran inline AND no
        // language crashed. A partial result should not suppress the next
        // background refresh, which might recover the crashed language.
        // On the deferred path we leave it absent so the daemon's phase_b_tick
        // auto-arms the scheduler when it opens the store.
        let phase_b_clean = phase_b_report
            .as_ref()
            .map(|r| r.crashed.is_empty())
            .unwrap_or(false);
        if run_phase_b_inline && phase_b_clean {
            let _ = store.set_meta("phase_b_commit", &sha);
        }
    }

    // Compute k-core shell numbers over the fully-built graph.
    // Runs after Phase A + Phase B so shell numbers reflect the complete
    // node/edge set (including SCIP semantic edges when Phase B ran inline).
    match compute_kcore(&store) {
        Ok(shells) => {
            let pairs: Vec<_> = shells.into_iter().collect();
            if let Err(e) = store.write_shell_numbers(&pairs) {
                tracing::warn!("kcore: failed to write shell numbers after init: {e}");
            }
        }
        Err(e) => tracing::warn!("kcore: computation failed after init: {e}"),
    }

    // Embed text generation is intentionally omitted here.
    // `regenerate_embed_texts_if_stale` (called from `travsr embed init` and
    // `travsr embed reindex`) generates embed_text with the correct richness
    // tier for the active model just before the sidecar runs — that is the
    // right place. Running it inline during `travsr init` blocks the terminal
    // for minutes on large repos and produces Compact-richness text that would
    // be regenerated immediately anyway.

    let total_edges = edges_before + edges_written;
    Ok(InitStats {
        files_indexed: batch_counts.files_written,
        files_skipped_unchanged,
        files_skipped_ignored,
        travsrignore_scaffolded: scaffolded,
        nodes_written: nodes_after - nodes_before,
        edges_written,
        total_nodes: nodes_after as u64,
        total_edges,
        phase_b_report,
    })
}

/// Write the result of one Phase B (SCIP) pass into `store`, returning a
/// [`PhaseBReport`].
///
/// Resolve cross-crate `UnresolvedCall`s emitted by Phase B into real `RefCall` edges.
///
/// Phase B cannot anchor callee VNames for cross-crate bare/scoped calls because
/// it only has the caller file in scope. This function batch-queries the store
/// (which holds all Phase A nodes) to find the real callee NodeId by signature,
/// then emits `EdgeKind::RefCall` edges for each resolved pair.
///
/// Overconnection is safe: when multiple nodes share a signature (e.g. two crates
/// both define `fn:new`) all matches are emitted. PPR damping absorbs the noise.
/// Resolve cross-crate `UnresolvedCall`s against Phase A nodes.
///
/// Returns `(edges, sites)`:
///   - `edges`: the deduped `RefCall` edges (caller → resolved callee).
///   - `sites`: `(caller_node, callee_node, caller_line)` occurrence tuples for
///     `edge_sites` (issue #299 WS-4). `caller_line` comes from the tree-sitter
///     call site; `0` (unknown) rows are skipped by `record_edge_sites`. This is
///     what gives `find_references` occurrence `path:line` for Rust cross-crate
///     bare/lowercase-scoped calls, which are not visible to the same-file
///     ScipRef pass.
fn resolve_unresolved_calls(
    store: &SqliteStore,
    unresolved: &[travsr_core::UnresolvedCall],
) -> (
    Vec<travsr_core::Edge>,
    Vec<(travsr_core::NodeId, travsr_core::NodeId, u32)>,
) {
    if unresolved.is_empty() {
        return (Vec::new(), Vec::new());
    }

    let sigs: Vec<String> = {
        let mut s: Vec<String> = unresolved.iter().map(|u| u.callee_sig.clone()).collect();
        s.sort_unstable();
        s.dedup();
        s
    };

    let candidates = match store.nodes_by_signatures(&sigs) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("resolve_unresolved_calls: store query failed: {e}");
            return (Vec::new(), Vec::new());
        }
    };

    // sig → Vec<(NodeId, path)>
    let mut by_sig: std::collections::HashMap<&str, Vec<(travsr_core::NodeId, &str)>> =
        std::collections::HashMap::new();
    for (id, sig, path) in &candidates {
        by_sig
            .entry(sig.as_str())
            .or_default()
            .push((*id, path.as_str()));
    }

    let mut edges: Vec<travsr_core::Edge> = Vec::new();
    let mut sites: Vec<(travsr_core::NodeId, travsr_core::NodeId, u32)> = Vec::new();
    for u in unresolved {
        let Some(matches) = by_sig.get(u.callee_sig.as_str()) else {
            continue;
        };
        let filtered: Vec<_> = if let Some(hint) = &u.hint_crate {
            let hint_dash = hint.replace('_', "-");
            matches
                .iter()
                .filter(|(_, path)| path.contains(hint.as_str()) || path.contains(&*hint_dash))
                .copied()
                .collect()
        } else {
            matches.to_vec()
        };
        // CO-A1: bare calls with no crate hint resolve to ALL same-named functions
        // across all crates → false edges that flood get_callers / blast_radius.
        // Only emit a RefCall when the match is unambiguous (exactly one candidate).
        if u.hint_crate.is_none() && filtered.len() != 1 {
            continue;
        }
        for (dst, _) in filtered {
            if u.src != dst {
                edges.push(travsr_core::Edge::new(
                    u.src,
                    dst,
                    travsr_core::EdgeKind::RefCall,
                ));
                // #299: record the call-site occurrence. `u.src` is the caller
                // function node (same file as the call), so nodes.path[src] is the
                // occurrence file — exactly what reference_sites returns.
                sites.push((u.src, dst, u.caller_line));
            }
        }
    }

    edges.sort_unstable_by_key(|e| (e.src.0, e.dst.0));
    edges.dedup_by(|a, b| a.src == b.src && a.dst == b.dst);
    sites.sort_unstable_by_key(|(s, d, l)| (s.0, d.0, *l));
    sites.dedup();
    (edges, sites)
}

/// Factored out of `init_repo_with_progress` so the background refresh
/// (#318 O3, [`run_background_phase_b`]) writes results through the exact same
/// unification + attribution path as a full init — there is one and only one
/// place that decides how Phase B nodes/edges/refs land in the store.
fn write_phase_b_results(
    store: &mut SqliteStore,
    corpus: &str,
    pb_nodes: Vec<travsr_core::Node>,
    pb_edges: Vec<travsr_core::Edge>,
    pb_refs: Vec<travsr_core::ScipRef>,
    pb_outcome: travsr_plugin_host::PhaseBOutcome,
) -> PhaseBReport {
    let pb_node_count = pb_nodes.len();
    let pb_edge_count = pb_edges.len();
    if pb_refs.is_empty() {
        // Old-style sidecar: no G2 attribution data — write nodes+edges directly.
        if let Err(e) = store.write_phase_b_batch(&pb_nodes, &pb_edges) {
            tracing::warn!("phase B batch write error: {e:#}");
        }
    } else {
        // G1: unify SCIP nodes (all languages) onto tree-sitter nodes before
        // writing. Mutates pb_refs in-place to redirect callee_id to unified
        // TS nodes, and returns the alias map (SCIP id → TS id).
        let mut pb_refs_mut = pb_refs;
        let alias_map = crate::scip_unifier::unify_all(store, corpus, &pb_nodes, &mut pb_refs_mut);
        let pb_refs = pb_refs_mut;

        // Drop unified SCIP definition nodes: the tree-sitter node already
        // represents them (symbol_aliases preserves scip_symbol → TS node
        // resolution), and writing them would re-create the duplicate node
        // + FTS rows that unification exists to eliminate.
        let pb_nodes: Vec<travsr_core::Node> = pb_nodes
            .into_iter()
            .filter(|n| !alias_map.contains_key(&n.id))
            .collect();

        // Rewrite SCIP structural edges through the alias map so they land
        // on the unified TS nodes instead of the dropped duplicates; an
        // edge that collapses to a self-loop after rewriting carried no
        // information beyond the node itself — drop it.
        let pb_edges: Vec<travsr_core::Edge> = pb_edges
            .into_iter()
            .filter_map(|mut e| {
                if let Some(&ts_id) = alias_map.get(&e.src) {
                    e.src = ts_id;
                }
                if let Some(&ts_id) = alias_map.get(&e.dst) {
                    e.dst = ts_id;
                }
                (e.src != e.dst).then_some(e)
            })
            .collect();

        // G2 path: span-attributed ref/call edges.
        if let Err(e) = store.write_scip_attributed_batch(corpus, &pb_nodes, &pb_refs) {
            tracing::warn!("phase B attributed write error: {e:#}");
        }
        // Structural edges from SCIP relationships (Pass 2 in scip-reader) still
        // need to be written — they are not represented in ScipRef records.
        if !pb_edges.is_empty() {
            if let Err(e) = store.write_phase_b_batch(&[], &pb_edges) {
                tracing::warn!("phase B structural edges write error: {e:#}");
            }
        }
    }
    if pb_node_count > 0 || pb_edge_count > 0 {
        tracing::info!(
            nodes = pb_node_count,
            structural_edges = pb_edge_count,
            "phase B indexing complete"
        );
    }
    // H3: stamp phase_b_warnings in the meta table so `travsr status` can surface
    // actionable issues without the user having to re-read init output.
    let mut warnings: Vec<String> = Vec::new();
    for lang in &pb_outcome.crashed {
        warnings.push(format!("crashed:{lang}"));
    }
    for (lang, expected, got) in &pb_outcome.version_mismatch {
        warnings.push(format!("version_mismatch:{lang}:{expected}:{got}"));
    }
    for lang in &pb_outcome.skipped_needs_approval {
        warnings.push(format!("needs_approval:{lang}"));
    }
    if !warnings.is_empty() {
        let _ = store.set_meta("phase_b_warnings", &warnings.join(","));
    } else {
        let _ = store.set_meta("phase_b_warnings", "");
    }

    // M1 degradation flag: record when rust-analyzer LSIF was skipped because
    // the OS sandbox was unavailable and --allow-unsandboxed-lsif was not set.
    // Surfaced by `travsr status` so the user knows Rust semantic edges are
    // degraded without having to grep logs.
    if travsr_indexer::sandbox::ra_lsif_sandbox_was_skipped() {
        let _ = store.set_meta("rust_lsif_degraded", "sandbox_unavailable");
    } else {
        let _ = store.set_meta("rust_lsif_degraded", "");
    }

    PhaseBReport {
        ran: pb_outcome.ran,
        skipped_not_in_repo: pb_outcome.skipped_not_in_repo,
        skipped_no_analyzer: pb_outcome.skipped_no_analyzer,
        skipped_unregistered: pb_outcome.skipped_unregistered,
        skipped_needs_approval: pb_outcome.skipped_needs_approval,
        crashed: pb_outcome.crashed,
        version_mismatch: pb_outcome.version_mismatch,
    }
}

/// Walk `repo_root` and return the set of language names (Language::as_str)
/// and the list of indexable absolute file paths. Used by the background Phase B
/// refresh (#318 O3) so P1 gating (#322) and P6 file-list forwarding (#329)
/// both come from a single directory walk.
fn collect_present_languages_and_paths(
    repo_root: &Path,
) -> (std::collections::HashSet<String>, Vec<PathBuf>) {
    use ignore::WalkBuilder;
    let mut langs = std::collections::HashSet::new();
    let mut paths = Vec::new();
    for entry in WalkBuilder::new(repo_root)
        .hidden(false)
        .git_ignore(true)
        .follow_links(false)
        .add_custom_ignore_filename(".travsrignore")
        .build()
        .flatten()
    {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let p = entry.into_path();
        let rel = p.strip_prefix(repo_root).unwrap_or(&p);
        if rel.components().any(|c| {
            crate::watcher::SKIP_DIRS
                .iter()
                .any(|skip| c.as_os_str() == *skip)
        }) {
            continue;
        }
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
        if let Some(lang) = Language::from_extension(ext) {
            // Data formats index in Phase A only — exclude from the Phase B
            // present_languages set so P1 never attempts a nonexistent tool.
            if !lang.is_data_format() {
                langs.insert(lang.as_str().to_string());
            }
            paths.push(p);
        }
    }
    (langs, paths)
}

/// Background, single-flight Phase B refresh (#318 O3).
///
/// Runs the full semantic (SCIP/Phase B) pass off the commit hot path, then
/// advances the `phase_b_commit` marker to the commit it indexed so O5 staleness
/// reporting catches up once the debounce window settles. The expensive sidecar
/// invocation runs WITHOUT the store lock; only the final write batch takes the
/// lock, so concurrent O1 queries are not blocked for the minutes a large repo's
/// SCIP run can take.
///
/// Package-scoped incremental re-runs (the ideal, RFC #318 O3) are gated on SCIP
/// tools gaining sub-path invocation support; until then every refresh is a full
/// re-run — which is exactly why it is debounced and single-flighted by
/// [`phase_b_sched::PhaseBScheduler`] rather than run inline on every commit.
/// Check the store's freshness markers and arm the Phase B scheduler if Phase B
/// is behind the latest commit. Uses `arm_immediate` (not `mark_dirty`) so the
/// deadline is set to `now` rather than `now + debounce`. `mark_dirty` called
/// on every 5 s tick would push the 30 s deadline forward forever — Phase B
/// would never fire. `arm_immediate` is a no-op when a commit-triggered
/// debounce is already counting down.
/// Spawn embed Phase 2 if Phase 1 is complete and Phase 2 still has pending nodes.
///
/// Called from the daemon's embed_tick (every 60 s). The `phase2_spawned` flag
/// prevents re-spawning on every tick — it is reset to false whenever the daemon
/// detects new Phase 1 work (e.g. after a re-init or new Phase B run).
/// Drive the two-phase background embedding pipeline from the daemon's embed_tick.
///
/// Phase 1 embeds the structurally most central nodes (shell_number >= derived
/// threshold, covering PHASE1_COVERAGE_FRACTION of all symbol nodes). Phase 2
/// embeds the remainder after Phase 1 finishes.
///
/// Two bugs fixed here vs the old `maybe_spawn_embed_phase2`:
///
/// Bug A — hardcoded threshold 3: `embed_progress` was called with threshold=3
/// but `spawn_background_reindex_phase1/2` derive a repo-specific threshold via
/// k-core coverage. This caused the progress check to use a completely different
/// Phase 1/2 split than the sidecar actually used, so Phase 2 could never be
/// triggered after Phase 1 completed (phase1_done < phase1_total at threshold=3
/// when phase1 actually used threshold=13).
///
/// Bug B — Phase 1 never re-triggered: Phase 1 is only spawned inside
/// `run_background_phase_b`. If `embed init` is run on a repo where Phase B has
/// already completed, or the daemon is restarted while Phase 1 is mid-flight,
/// Phase 1 silently stalls forever. This function now detects both cases and
/// re-triggers Phase 1 when: Phase B is complete AND no sidecar is currently
/// running AND Phase 1 is still incomplete.
fn maybe_spawn_embed(
    repo_root: &Path,
    store: &std::sync::Mutex<SqliteStore>,
    phase2_spawned: &std::sync::atomic::AtomicBool,
) {
    use std::sync::atomic::Ordering;

    let db_path = repo_root.join(".travsr/graph.db");
    if !db_path.exists() {
        return;
    }

    // Do not auto-embed repos that haven't opted in via `travsr embed init`.
    let backend_id = match travsr_plugin_host::repo_backend_id(repo_root) {
        Some(id) => id,
        None => return,
    };

    // Fix for Bug A: use the derived threshold — same derivation as
    // spawn_background_reindex_phase1/2 — so the progress check here and the
    // actual sidecar partitioning always agree on the Phase 1/2 boundary.
    // Falls back to 3 when k-core data is not yet available (pre-Phase-B).
    let threshold = travsr_plugin_host::derive_phase1_threshold_for_status(&db_path).unwrap_or(3);

    let (total, embedded, phase1_total, phase1_done) = {
        let s = store.lock().unwrap_or_else(|e| e.into_inner());
        match s.embed_progress(&backend_id, threshold) {
            Ok(t) => t,
            Err(e) => {
                tracing::debug!("embed_tick: embed_progress query failed: {e}");
                return;
            }
        }
    };

    if phase1_done < phase1_total {
        // Fix for Bug B: catch-up trigger for Phase 1.
        // Fires when no sidecar is currently running AND Phase 1 is still
        // incomplete. Covers two cases:
        //   (a) `embed init` was run after Phase B completed — the post-Phase-B
        //       trigger fired with no backend configured and was silently skipped.
        //   (b) The daemon was restarted while Phase 1 was mid-flight — the
        //       sidecar was killed with Phase 1 partially done.
        // In both cases the correct action is to (re-)launch Phase 1; the sidecar
        // skips already-embedded nodes, so partial progress is preserved.
        if !travsr_plugin_host::embed_reindex_in_flight() {
            let phase_b_complete = {
                let s = store.lock().unwrap_or_else(|e| e.into_inner());
                let last = s.get_meta("last_commit").ok().flatten();
                let pb = s.get_meta("phase_b_commit").ok().flatten();
                matches!((last, pb), (Some(l), Some(p)) if !l.is_empty() && p == l)
            };
            if phase_b_complete {
                tracing::info!(
                    phase1_done,
                    phase1_total,
                    "embed_tick: Phase 1 incomplete, no sidecar running — spawning Phase 1"
                );
                travsr_plugin_host::spawn_background_reindex_phase1(&db_path);
            } else {
                tracing::debug!("embed_tick: Phase 1 pending — waiting for Phase B to complete");
            }
        } else {
            tracing::debug!(
                phase1_done,
                phase1_total,
                "embed_tick: Phase 1 in progress — deferring Phase 2"
            );
        }
        return;
    }

    // Phase 1 complete — handle Phase 2. Skip only while a Phase 2 run we launched
    // is STILL in progress. If it finished, failed, or timed out (in_flight=false)
    // with work remaining, fall through so the next tick retries — otherwise a
    // single Phase 2 timeout would strand the repo forever (phase2_spawned latched).
    if phase2_spawned.load(Ordering::Relaxed) && travsr_plugin_host::embed_reindex_in_flight() {
        return;
    }

    let phase2_total = total.saturating_sub(phase1_total);
    let phase2_remaining = phase2_total.saturating_sub(embedded.saturating_sub(phase1_done));

    if phase2_remaining == 0 {
        phase2_spawned.store(true, Ordering::Relaxed);
        return;
    }

    tracing::info!(
        phase2_remaining,
        "embed_tick: Phase 1 complete — spawning Phase 2"
    );
    if travsr_plugin_host::spawn_background_reindex_phase2(&db_path) {
        phase2_spawned.store(true, Ordering::Relaxed);
    }
}

fn arm_phase_b_if_pending(
    store: &std::sync::Mutex<SqliteStore>,
    sched: &phase_b_sched::PhaseBScheduler,
) {
    let s = store.lock().unwrap_or_else(|e| e.into_inner());
    let last = s.get_meta("last_commit").ok().flatten().unwrap_or_default();
    let pb = s
        .get_meta("phase_b_commit")
        .ok()
        .flatten()
        .unwrap_or_default();
    if !last.is_empty() && last != pb {
        sched.arm_immediate();
    }
}

fn run_background_phase_b(
    repo_root: &Path,
    store: &std::sync::Mutex<SqliteStore>,
    sched: &phase_b_sched::PhaseBScheduler,
) {
    // Delegate to an inner fn so we can capture its bool result and always call
    // finish_with_result, even on early returns — without unsafe raw pointers.
    let succeeded = run_background_phase_b_inner(repo_root, store);
    sched.finish_with_result(succeeded);
}

/// Inner worker for [`run_background_phase_b`].
///
/// Returns `true` when at least some semantic work succeeded (LSIF edges
/// produced, or ≥1 sidecar ran without crashing). `false` means every sidecar
/// crashed, which increments the retry-cap counter in `PhaseBScheduler`.
fn run_background_phase_b_inner(repo_root: &Path, store: &std::sync::Mutex<SqliteStore>) -> bool {
    // Brief lock: read the corpus and the two freshness markers. Bail early if
    // the semantic data already matches HEAD (e.g. a commit touched no indexed
    // files, or another run already caught up).
    let (corpus, target_sha) = {
        let s = store.lock().unwrap_or_else(|e| e.into_inner());
        let corpus = s.get_meta("corpus").ok().flatten().unwrap_or_default();
        let last = s.get_meta("last_commit").ok().flatten().unwrap_or_default();
        let phase_b = s
            .get_meta("phase_b_commit")
            .ok()
            .flatten()
            .unwrap_or_default();
        if last.is_empty() || last == phase_b {
            // Already up to date — not a failure, treat as success so the
            // retry counter is not incremented.
            return true;
        }
        (corpus, last)
    };

    tracing::info!(commit=%target_sha, "background phase B refresh starting");

    // ── LSIF pass (TypeScript compiler — expensive, runs lock-free) ───────────
    // Collect edges into a Vec first; write them under the store lock below.
    // This mirrors the SCIP sidecar pattern and keeps queries warm throughout.
    let lsif_edges = run_lsif_pass_collect(repo_root, &corpus);

    // ── SCIP sidecar pass (all languages in parallel, lock-free) ─────────────
    // P6 (#329): single walk yields both present_languages and indexable_paths
    // so Phase B runners skip their own directory walks.
    let (present_languages, indexable_paths) = collect_present_languages_and_paths(repo_root);
    // R1: reset per-Phase-B skip latch before the run (same as init_repo_with_progress).
    travsr_indexer::sandbox::reset_ra_lsif_sandbox_skip();
    let indexer = travsr_plugin_host::PluginIndexer::new(&corpus);
    let inputs = travsr_plugin_host::PhaseBInputs {
        repo_root,
        present_languages,
        indexable_paths: &indexable_paths,
    };
    let (pb_nodes, mut pb_edges, pb_refs, pb_unresolved, pb_outcome) =
        indexer.invoke_phase_b_all(&inputs);

    // ── Single write batch under the lock ─────────────────────────────────────
    let mut s = store.lock().unwrap_or_else(|e| e.into_inner());

    let (resolved, resolved_sites) = resolve_unresolved_calls(&s, &pb_unresolved);
    tracing::debug!(
        resolved_cross_crate_edges = resolved.len(),
        "Phase B UnresolvedCall resolution complete"
    );
    pb_edges.extend(resolved);

    // Write LSIF edges first (pre-collected lock-free above).
    for edge in &lsif_edges {
        if let Err(e) = s.put_edge_lsif(edge) {
            tracing::warn!("lsif edge write error: {e}");
        }
    }

    let report = write_phase_b_results(&mut s, &corpus, pb_nodes, pb_edges, pb_refs, pb_outcome);
    // #299 WS-4: record cross-crate call occurrence lines after their edges land.
    if let Err(e) = s.record_edge_sites(&resolved_sites) {
        tracing::warn!("recording cross-crate edge_sites: {e:#}");
    }

    // C4: only advance phase_b_commit when no language crashed. A partial result
    // should not suppress the next background refresh so crashed languages can
    // be retried once the user installs the missing tool or clears disk space.
    if report.crashed.is_empty() {
        let _ = s.set_meta("phase_b_commit", &target_sha);
    }

    let succeeded = !report.ran.is_empty() || !lsif_edges.is_empty() || report.crashed.is_empty();

    tracing::info!(
        commit = %target_sha,
        ran = report.ran.len(),
        lsif_edges = lsif_edges.len(),
        crashed = report.crashed.len(),
        "background phase B refresh complete"
    );

    // Re-run k-core while the lock is still held: Phase B edges change the
    // graph structure so shell numbers computed at init time are stale.
    // Phase B nodes themselves have shell_number = NULL (k-core ran before
    // Phase B); recomputing assigns them real shell numbers so Phase 1/2
    // threshold filters work correctly on the full node set.
    if succeeded {
        match compute_kcore(&s) {
            Ok(shells) => {
                let pairs: Vec<_> = shells.into_iter().collect();
                if let Err(e) = s.write_shell_numbers(&pairs) {
                    tracing::warn!("kcore: failed to update shell numbers after phase B: {e}");
                } else {
                    tracing::info!("kcore: shell numbers updated after phase B");
                }
            }
            Err(e) => tracing::warn!("kcore: computation failed after phase B: {e}"),
        }
    }

    // Stamp the Phase 1 start time before dropping the lock so `travsr embed
    // status` can compute actual throughput (nodes/sec) instead of guessing.
    if succeeded {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = s.set_meta("embed_p1_start", &now_secs.to_string());
    }

    // Release the store lock before spawning: the embed sidecars read graph.db
    // for unembed nodes but write embeddings to embed.db — zero WAL contention
    // with the daemon's graph.db write slot.
    drop(s);

    // Start Phase 1 embedding only after Phase B and k-core are both complete:
    //   1. No concurrent writes between Phase B and embed (contention-free).
    //   2. Phase B nodes have real shell_number values for Phase 1/2 filters.
    //   3. Phase 1 finishes quickly and builds a usable HNSW index.
    // Phase 2 is intentionally NOT spawned here — the embed_tick in the daemon
    // event loop detects when Phase 1 is done and then spawns Phase 2, so the
    // two phases never write to embed.db concurrently.
    if succeeded {
        let db_path = repo_root.join(".travsr/graph.db");
        if travsr_plugin_host::spawn_background_reindex_phase1(&db_path) {
            tracing::info!("triggered post-phase-B embed Phase 1");
        }
    }

    succeeded
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
) -> anyhow::Result<travsr_core::DirtySet> {
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
            return Ok(Default::default());
        }
        Err(e) => {
            tracing::warn!(
                "could not read signature_format_version: {e}, skipping reindex. \
                 Run `travsr init` to repair the graph."
            );
            return Ok(Default::default());
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

    let mut indexer = PluginIndexer::new(&corpus);
    // Accumulate FFI markers across all files for repo-level cross-language
    // resolution (RFC-005). Resolution runs once after the per-file loop so
    // markers from both sides of each FFI boundary are available.
    let mut all_ffi_markers: Vec<FfiMarker> = Vec::new();
    let mut any_changed = false;
    // Accumulate Tier-0 dirty callers across all files in this batch.
    let mut callers_all = travsr_core::DirtySet::default();

    for abs_path in paths {
        let vname_path = abs_path
            .strip_prefix(repo_root)
            .unwrap_or(abs_path)
            .to_string_lossy()
            .replace('\\', "/");

        // §2 GC keystone: detect deletion before touching the graph.
        let new_hash = match hash_file(abs_path) {
            Ok(h) => h,
            Err(ref err)
                if err
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                // File was deleted — both-direction delete + clear file hash row.
                match store.delete_file(&corpus, &vname_path) {
                    Ok(callers) => {
                        if !callers.is_empty() {
                            tracing::debug!(
                                path = %vname_path,
                                callers = %callers.len(),
                                "delete_file: collecting {} caller(s) for Tier-0 re-resolution",
                                callers.len()
                            );
                            callers_all.extend(callers);
                        }
                        any_changed = true;
                    }
                    Err(e) => tracing::warn!(path = %vname_path, err = %e, "delete_file failed"),
                }
                continue;
            }
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

        // §2 GC keystone: parse FIRST — a syntax error never erases the old graph.
        let out = match indexer.parse_file_with_vname(abs_path, &vname_path) {
            Ok(o) => o,
            Err(err) => {
                tracing::warn!("parse error for {}: {err}", abs_path.display());
                continue; // keep old graph intact
            }
        };

        // Build import-resolver edges before the atomic reindex_replace call.
        let ext = abs_path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let import_edges = match Language::from_extension(ext) {
            Some(Language::TypeScript) => link_imports(&out.nodes, &vname_path, &corpus),
            Some(Language::Rust) => link_imports_rust(&out.nodes, &vname_path, &corpus),
            Some(Language::Python) => {
                link_imports_python_fs(&out.nodes, &vname_path, &corpus, repo_root)
            }
            _ => Vec::new(),
        };
        let mut all_edges = out.edges.clone();
        all_edges.extend(import_edges);

        // §2: owned-edge-only atomic replace — preserves inbound edges to surviving
        // symbols (blank-line/body edits lossless) and eagerly deletes orphans for
        // removed symbols. Hash upsert is inside the same transaction.
        match store.reindex_replace(&corpus, &vname_path, &out.nodes, &all_edges, &new_hex) {
            Ok(report) => {
                if !report.callers.is_empty() {
                    tracing::debug!(
                        path = %vname_path,
                        removed = %report.removed_count,
                        callers = %report.callers.len(),
                        "reindex_replace: removed symbols, collecting {} caller(s) for Tier-0",
                        report.callers.len()
                    );
                    callers_all.extend(report.callers);
                }
                any_changed = true;
                // Collect FFI markers for the repo-level pass (RFC-005).
                all_ffi_markers.extend(out.ffi_markers);
            }
            Err(e) => tracing::warn!(path = %vname_path, err = %e, "reindex_replace failed"),
        }
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

        // Recompute k-core shell numbers so they stay fresh after every commit.
        // O(V + E) — fast enough to run inline on the hook path at MVP scale.
        match compute_kcore(store) {
            Ok(shells) => {
                let pairs: Vec<_> = shells.into_iter().collect();
                if let Err(e) = store.write_shell_numbers(&pairs) {
                    tracing::warn!("kcore: failed to write shell numbers: {e}");
                }
            }
            Err(e) => tracing::warn!("kcore: computation failed: {e}"),
        }

        // Populate embed_text for newly-indexed nodes (commit-hook path).
        // Only runs when a model is configured for this repo.
        let richness = richness_from_meta(repo_root);
        update_embed_texts(store, repo_root, richness);
    }

    // PERF-002: The LSIF semantic pass was running on every `reindex_files`
    // call that touched any .ts file, including every post-commit hook run.
    // `run_lsif_emitter` buffers the entire tsc JSON output into a String
    // (200-500 MB for large projects) before parsing. Running this on every
    // commit caused a repeated 200-500 MB RSS spike.
    //
    // The LSIF pass now runs only from `init_repo` (full initial index).
    // Per-commit LSIF delta is tracked as DEBT(travsr-25).

    Ok(callers_all)
}

/// Run the LSIF semantic pass if `tsconfig.json` is present at the repo root,
/// writing edges directly into `store`.
///
/// Used by the inline path (`--semantic` or no-commit repos). For the deferred
/// path use [`run_lsif_pass_collect`] + write under the store lock.
///
/// Failures (binary not on PATH, tsconfig absent, parse errors) are logged as
/// warnings and silently skipped — they must never fail the overall index.
fn run_lsif_pass(repo_root: &Path, corpus: &str, store: &mut SqliteStore) {
    let edges = run_lsif_pass_collect(repo_root, corpus);
    for edge in &edges {
        if let Err(e) = store.put_edge_lsif(edge) {
            tracing::warn!("lsif edge write error: {e}");
        }
    }
    tracing::debug!("lsif pass: {} RefCall edges persisted", edges.len());
}

/// Collect LSIF RefCall edges without holding the store lock.
///
/// Returns an empty `Vec` when `tsconfig.json` is absent or the emitter fails.
/// The caller writes the edges under the store lock. This split lets
/// `run_background_phase_b` hold the lock only for the final write batch while
/// the expensive TS compiler runs lock-free.
fn run_lsif_pass_collect(repo_root: &Path, corpus: &str) -> Vec<travsr_core::Edge> {
    let tsconfig = repo_root.join("tsconfig.json");
    if !tsconfig.exists() {
        return Vec::new();
    }

    let dump = match run_lsif_emitter(&tsconfig) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("lsif emitter skipped: {e}");
            return Vec::new();
        }
    };

    match ingest_lsif(&dump, corpus) {
        Ok(out) => {
            tracing::debug!("lsif pass: collected {} RefCall edges", out.edges.len());
            out.edges
        }
        Err(e) => {
            tracing::warn!("lsif ingest error: {e}");
            Vec::new()
        }
    }
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
    use std::sync::Mutex;

    // Rust tests run in parallel; TRAVSR_DISABLE_REGISTRY and HOME are
    // process-global env vars. Serialize every test that mutates them through
    // this lock to prevent races on Windows and Linux multi-threaded test runs.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

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

    #[test]
    fn init_repo_skips_registry_when_env_var_set() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        std::fs::write(tmp.path().join("app.ts"), "export class App {}").unwrap();

        let home_tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", home_tmp.path());
        std::env::set_var("TRAVSR_DISABLE_REGISTRY", "1");

        let _ = init_repo(tmp.path()).unwrap();

        let registry_path = home_tmp.path().join(".travsr").join("registry.json");
        assert!(
            !registry_path.exists(),
            "registry.json must not be created when TRAVSR_DISABLE_REGISTRY=1"
        );

        std::env::remove_var("TRAVSR_DISABLE_REGISTRY");
        std::env::remove_var("HOME");
    }

    #[test]
    fn claude_directory_is_skipped_during_init() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());

        let claude_src = tmp
            .path()
            .join(".claude")
            .join("worktrees")
            .join("session-1");
        std::fs::create_dir_all(&claude_src).unwrap();
        std::fs::write(claude_src.join("agent.ts"), "export const x = 1;").unwrap();

        std::fs::write(tmp.path().join("real.ts"), "export class Real {}").unwrap();

        std::env::set_var("TRAVSR_DISABLE_REGISTRY", "1");
        let stats = init_repo(tmp.path()).unwrap();
        std::env::remove_var("TRAVSR_DISABLE_REGISTRY");

        assert_eq!(
            stats.files_indexed, 1,
            ".claude/ contents must be skipped; expected 1 file (real.ts), got {}",
            stats.files_indexed
        );
    }

    #[test]
    fn init_repo_purges_ghost_nodes_from_skip_dirs() {
        let _guard = ENV_LOCK.lock().unwrap();
        // Pre-populate the DB with a ghost node that looks like it came from a
        // previous run that indexed .claude/ before it was added to SKIP_DIRS.
        // Verifies that init_repo tombstones it even though the file no longer
        // changes (hash-delta loop would skip it).
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());

        let travsr_dir = tmp.path().join(".travsr");
        std::fs::create_dir_all(&travsr_dir).unwrap();
        let db_path = travsr_dir.join("graph.db");
        {
            let mut store = travsr_store::SqliteStore::open(&db_path).unwrap();
            store
                .set_signature_format_version(travsr_core::SIGNATURE_FORMAT_VERSION)
                .unwrap();
            let ghost = travsr_core::Node::new(
                travsr_core::VName::new(
                    "",
                    "",
                    ".claude/worktrees/session-x/agent.ts",
                    "typescript",
                    "fn:ghost",
                ),
                "function",
            );
            store.put_node(&ghost).unwrap();
            assert_eq!(
                store.node_count().unwrap(),
                1,
                "ghost node must exist before init"
            );
        }

        std::fs::write(tmp.path().join("real.ts"), "export class Real {}").unwrap();

        std::env::set_var("TRAVSR_DISABLE_REGISTRY", "1");
        init_repo(tmp.path()).unwrap();
        std::env::remove_var("TRAVSR_DISABLE_REGISTRY");

        let store = travsr_store::SqliteStore::open(&db_path).unwrap();
        let ghosts = store.search_nodes_by_name("ghost").unwrap();
        assert!(
            ghosts.is_empty(),
            "ghost node must be purged by init_repo, found: {ghosts:?}"
        );
    }

    #[test]
    fn init_repo_stamps_last_commit_on_rerun_when_no_files_changed() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        std::fs::write(tmp.path().join("app.ts"), "export class App {}").unwrap();

        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(tmp.path())
            .status()
            .unwrap();

        std::env::set_var("TRAVSR_DISABLE_REGISTRY", "1");
        let _ = init_repo(tmp.path()).unwrap();
        let _ = init_repo(tmp.path()).unwrap(); // re-run — all hashes match
        std::env::remove_var("TRAVSR_DISABLE_REGISTRY");

        let db_path = tmp.path().join(".travsr/graph.db");
        let store = travsr_store::SqliteStore::open(&db_path).unwrap();
        assert!(
            store.get_meta("last_commit").unwrap().is_some(),
            "last_commit must be stamped after re-init even when no files changed"
        );
    }

    #[test]
    fn init_repo_returns_nonzero_total_counts_on_rerun() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        std::fs::write(tmp.path().join("app.ts"), "export class App { run() {} }").unwrap();

        std::env::set_var("TRAVSR_DISABLE_REGISTRY", "1");
        let _ = init_repo(tmp.path()).unwrap();
        let stats = init_repo(tmp.path()).unwrap();
        std::env::remove_var("TRAVSR_DISABLE_REGISTRY");

        assert_eq!(stats.nodes_written, 0, "no new nodes on re-run");
        assert!(
            stats.total_nodes > 0,
            "total_nodes must reflect existing graph on re-run, got {}",
            stats.total_nodes
        );
    }

    // ── GC invariant tests ───────────────────────────────────────────────────

    /// A blank-line edit (no structural change) must preserve the node count
    /// after `reindex_replace`, and leave no orphan edges. (Edge count is not
    /// asserted because Tree-sitter uses byte-offset spans in some edge source
    /// locations, which can change when lines shift — node identity is what
    /// matters for graph correctness.)
    #[test]
    fn blank_line_preserves_node_count_and_no_orphans() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());

        let ts_path = tmp.path().join("svc.ts");
        std::fs::write(&ts_path, "export class Svc { run() {} }\n").unwrap();

        std::env::set_var("TRAVSR_DISABLE_REGISTRY", "1");
        init_repo(tmp.path()).unwrap();
        std::env::remove_var("TRAVSR_DISABLE_REGISTRY");

        let db_path = tmp.path().join(".travsr/graph.db");
        let mut store = travsr_store::SqliteStore::open(&db_path).unwrap();
        let nodes_before = store.node_count().unwrap();
        assert!(nodes_before > 0, "must have nodes after init");

        // Insert a blank line at the start — shifts all source positions.
        std::fs::write(&ts_path, "\nexport class Svc { run() {} }\n").unwrap();
        reindex_files(std::slice::from_ref(&ts_path), tmp.path(), &mut store).unwrap();

        let nodes_after = store.node_count().unwrap();
        assert_eq!(
            nodes_before, nodes_after,
            "blank-line edit must not change node count ({nodes_before} → {nodes_after})"
        );

        // No orphan edges must remain regardless of edge-count change.
        let orphans = store.sweep_orphans().unwrap();
        assert_eq!(
            orphans, 0,
            "blank-line reindex must leave no orphan edges; sweep found {orphans}"
        );
    }

    /// `delete_file` uses both-direction semantics: all edges whose src or dst
    /// lives in the deleted file are removed. After deletion, `sweep_orphans`
    /// must report 0 (the write path left no dangling edges).
    #[test]
    fn delete_file_leaves_no_orphan_edges() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());

        // Two files so the indexer records edges for both.
        std::fs::write(tmp.path().join("svc.ts"), "export class Svc { run() {} }").unwrap();
        std::fs::write(tmp.path().join("app.ts"), "export class App { go() {} }").unwrap();

        std::env::set_var("TRAVSR_DISABLE_REGISTRY", "1");
        init_repo(tmp.path()).unwrap();
        std::env::remove_var("TRAVSR_DISABLE_REGISTRY");

        let db_path = tmp.path().join(".travsr/graph.db");
        let mut store = travsr_store::SqliteStore::open(&db_path).unwrap();
        assert!(
            store.node_count().unwrap() > 0,
            "must have nodes after init"
        );

        let corpus = store.get_meta("corpus").unwrap().unwrap_or_default();
        store.delete_file(&corpus, "svc.ts").unwrap();

        // No nodes from svc.ts should remain.
        let svc_nodes = store.search_nodes_by_name("Svc").unwrap();
        assert!(
            svc_nodes.is_empty(),
            "delete_file must remove all nodes for svc.ts"
        );

        // Orphan sweep must find nothing — both-direction delete left no dangling edges.
        let orphans = store.sweep_orphans().unwrap();
        assert_eq!(
            orphans, 0,
            "delete_file must leave no orphan edges; sweep found {orphans}"
        );
    }

    /// §9 CI invariant: incremental delete + reindex must produce the same
    /// node and edge counts as a full rebuild on the mutated tree.
    ///
    /// This is the executable form of principle #4 ("full reindex and an
    /// incremental reindex of the same codebase produce identical graphs") and
    /// would have caught #402.
    #[test]
    fn incremental_delete_matches_full_rebuild() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());

        std::fs::write(tmp.path().join("svc.ts"), "export class Svc { run() {} }\n").unwrap();
        std::fs::write(tmp.path().join("app.ts"), "export class App { go() {} }\n").unwrap();

        std::env::set_var("TRAVSR_DISABLE_REGISTRY", "1");
        init_repo(tmp.path()).unwrap();
        std::env::remove_var("TRAVSR_DISABLE_REGISTRY");

        // Incremental path: delete svc.ts on disk, then run reindex_files.
        let svc_path = tmp.path().join("svc.ts");
        std::fs::remove_file(&svc_path).unwrap();
        {
            let db_path = tmp.path().join(".travsr/graph.db");
            let mut store = travsr_store::SqliteStore::open(&db_path).unwrap();
            reindex_files(std::slice::from_ref(&svc_path), tmp.path(), &mut store).unwrap();
        }
        let (inc_nodes, inc_edges) = {
            let db_path = tmp.path().join(".travsr/graph.db");
            let store = travsr_store::SqliteStore::open(&db_path).unwrap();
            (store.node_count().unwrap(), store.edge_count().unwrap())
        };

        // Full-rebuild path: wipe .travsr and re-init on the same (now mutated) tree.
        std::fs::remove_dir_all(tmp.path().join(".travsr")).unwrap();
        std::env::set_var("TRAVSR_DISABLE_REGISTRY", "1");
        init_repo(tmp.path()).unwrap();
        std::env::remove_var("TRAVSR_DISABLE_REGISTRY");
        let (full_nodes, full_edges) = {
            let db_path = tmp.path().join(".travsr/graph.db");
            let store = travsr_store::SqliteStore::open(&db_path).unwrap();
            (store.node_count().unwrap(), store.edge_count().unwrap())
        };

        assert_eq!(
            inc_nodes, full_nodes,
            "incremental delete + reindex must yield same node count as full rebuild \
             (incremental={inc_nodes}, full={full_nodes})"
        );
        assert_eq!(
            inc_edges, full_edges,
            "incremental delete + reindex must yield same edge count as full rebuild \
             (incremental={inc_edges}, full={full_edges})"
        );
    }

    /// Tier-0 propagation depth-1 bound: re-indexing a file that removes no
    /// exported symbols (body-only edit) must return an empty DirtySet.
    ///
    /// This is the invariant that prevents cascade: when a Tier-0 caller is
    /// re-indexed, its outbound edges are re-resolved but its own exported
    /// symbols are unchanged, so `removed = ∅` and it enqueues nothing further.
    #[test]
    fn tier0_propagation_is_depth_one_no_cascade() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());

        std::fs::write(tmp.path().join("svc.ts"), "export class Svc { run() {} }\n").unwrap();

        std::env::set_var("TRAVSR_DISABLE_REGISTRY", "1");
        init_repo(tmp.path()).unwrap();
        std::env::remove_var("TRAVSR_DISABLE_REGISTRY");

        let db_path = tmp.path().join(".travsr/graph.db");
        let mut store = travsr_store::SqliteStore::open(&db_path).unwrap();

        // Body-only edit: same exported symbol `Svc`, just different method body.
        // This simulates what happens when a Tier-0 caller is re-indexed — it
        // re-points its outbound edges but never renames its own symbols.
        std::fs::write(
            tmp.path().join("svc.ts"),
            "export class Svc { run() { return 42; } }\n",
        )
        .unwrap();
        let svc_path = tmp.path().join("svc.ts");
        let cascade =
            reindex_files(std::slice::from_ref(&svc_path), tmp.path(), &mut store).unwrap();

        assert!(
            cascade.is_empty(),
            "body-only edit (no symbols removed) must return empty DirtySet; \
             got {} caller(s) — Tier-0 cascade depth-1 bound would be violated",
            cascade.len()
        );
    }

    // ── maybe_spawn_embed tests ───────────────────────────────────────────────

    fn setup_embed_store(tmp: &std::path::Path) -> travsr_store::SqliteStore {
        std::fs::create_dir_all(tmp.join(".travsr")).unwrap();
        let db_path = tmp.join(".travsr/graph.db");
        travsr_store::SqliteStore::open(&db_path).unwrap()
    }

    fn set_phase_b_complete(store: &mut travsr_store::SqliteStore, sha: &str) {
        store.set_meta("last_commit", sha).unwrap();
        store.set_meta("phase_b_commit", sha).unwrap();
    }

    fn set_phase_b_pending(store: &mut travsr_store::SqliteStore) {
        store.set_meta("last_commit", "abc123").unwrap();
        // phase_b_commit deliberately absent — Phase B not yet run
    }

    #[test]
    fn maybe_spawn_embed_returns_early_when_db_absent() {
        let tmp = tempfile::tempdir().unwrap();
        // No graph.db at all — function must return without panic or spin
        let store = std::sync::Mutex::new(setup_embed_store(tmp.path()));
        let flag = std::sync::atomic::AtomicBool::new(false);
        // point repo_root at a path with no .travsr/graph.db
        let empty = tempfile::tempdir().unwrap();
        maybe_spawn_embed(empty.path(), &store, &flag);
        // reaching here is the assertion — no panic, no hang
    }

    #[test]
    fn maybe_spawn_embed_returns_early_when_backend_not_configured() {
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        let mut store = setup_embed_store(tmp.path());
        set_phase_b_complete(&mut store, "deadbeef");
        let store = std::sync::Mutex::new(store);
        let flag = std::sync::atomic::AtomicBool::new(false);
        // No embed.toml written — repo_backend_id returns None
        maybe_spawn_embed(tmp.path(), &store, &flag);
        // No spawn attempted; flag must stay false
        assert!(!flag.load(std::sync::atomic::Ordering::Relaxed));
    }

    /// Set shell_number = `shell` on every embeddable node (kind not in the
    /// exclusion list). This simulates Phase B having run and computed k-core
    /// without requiring the Phase B toolchain to be installed.
    fn inject_shell_numbers(store: &mut travsr_store::SqliteStore, shell: u32) {
        let nodes = store.nodes_missing_embed_text().unwrap_or_default();
        let pairs: Vec<(travsr_core::NodeId, u32)> = nodes.iter().map(|n| (n.id, shell)).collect();
        if !pairs.is_empty() {
            store.write_shell_numbers(&pairs).unwrap();
        }
    }

    /// Build a real graph.db with at least one indexed TS file, simulated Phase B
    /// shell_numbers, and embed backend configured. Returns (tmp, store, db_path).
    fn setup_repo_with_phase_b(
        shell: u32,
        phase_b_sha: Option<&str>,
    ) -> (tempfile::TempDir, travsr_store::SqliteStore) {
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        std::fs::write(
            tmp.path().join("app.ts"),
            "export class App { run() {} greet(name: string) { return name; } }",
        )
        .unwrap();
        std::env::set_var("TRAVSR_DISABLE_REGISTRY", "1");
        init_repo(tmp.path()).unwrap();
        std::env::remove_var("TRAVSR_DISABLE_REGISTRY");

        let db_path = tmp.path().join(".travsr/graph.db");
        // Write backend config after .travsr/ exists (created by init_repo)
        travsr_plugin_host::write_repo_backend_id(tmp.path(), "bge-small-en-v1.5").unwrap();
        let mut store = travsr_store::SqliteStore::open(&db_path).unwrap();
        inject_shell_numbers(&mut store, shell);
        if let Some(sha) = phase_b_sha {
            set_phase_b_complete(&mut store, sha);
        } else {
            set_phase_b_pending(&mut store);
        }
        (tmp, store)
    }

    #[test]
    fn maybe_spawn_embed_skips_phase1_catchup_when_phase_b_pending() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // shell=5, Phase B pending (last_commit set but phase_b_commit absent)
        let (tmp, store) = setup_repo_with_phase_b(5, None);
        let store = std::sync::Mutex::new(store);
        let flag = std::sync::atomic::AtomicBool::new(false);
        // phase1_total > 0 (nodes have shell_number=5 ≥ threshold),
        // but Phase B is NOT complete → catch-up must NOT fire.
        maybe_spawn_embed(tmp.path(), &store, &flag);
        // flag stays false — Phase 1 not done, Phase 2 not triggered
        assert!(!flag.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn maybe_spawn_embed_triggers_phase1_catchup_when_phase_b_complete() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // shell=5, Phase B complete — this is the "embed init after Phase B" scenario
        let (tmp, store) = setup_repo_with_phase_b(5, Some("deadbeef"));
        let store = std::sync::Mutex::new(store);
        let flag = std::sync::atomic::AtomicBool::new(false);
        // phase1_total > 0, Phase B complete, no sidecar, no embeddings yet
        // → catch-up fires and calls spawn_background_reindex_phase1.
        // The binary isn't installed so spawn returns false — but the catch-up
        // LOGIC branch is exercised and the function returns without setting
        // phase2_spawned (Phase 1 still not done).
        maybe_spawn_embed(tmp.path(), &store, &flag);
        // flag stays false — Phase 1 still not done, Phase 2 not triggered
        assert!(!flag.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn maybe_spawn_embed_does_not_trigger_phase2_when_phase2_flag_set() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (tmp, store) = setup_repo_with_phase_b(5, Some("deadbeef"));
        let store = std::sync::Mutex::new(store);
        // phase2_spawned=true — function must return before calling spawn_phase2
        let flag = std::sync::atomic::AtomicBool::new(true);
        maybe_spawn_embed(tmp.path(), &store, &flag);
        // flag remains true — Phase 2 was not re-spawned
        assert!(flag.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn embed_reindex_in_flight_reflects_idle_state() {
        // With no sidecar currently running, the in-flight flag must be false.
        // After a spawn attempt with an unconfigured repo (binary not installed),
        // the flag must still be false (spawn_phase1 clears it on early-exit).
        assert!(!travsr_plugin_host::embed_reindex_in_flight());
        travsr_plugin_host::spawn_background_reindex_phase1(std::path::Path::new("/nonexistent"));
        assert!(!travsr_plugin_host::embed_reindex_in_flight());
    }

    #[test]
    fn maybe_spawn_embed_phase_b_complete_detection() {
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        let db_path = tmp.path().join(".travsr/graph.db");
        std::fs::create_dir_all(tmp.path().join(".travsr")).unwrap();
        let mut store = travsr_store::SqliteStore::open(&db_path).unwrap();

        // No commits yet — Phase B incomplete
        let (last, pb) = (
            store.get_meta("last_commit").unwrap(),
            store.get_meta("phase_b_commit").unwrap(),
        );
        assert!(last.is_none(), "last_commit absent initially");
        assert!(pb.is_none(), "phase_b_commit absent initially");

        // Simulate Phase B completion
        store.set_meta("last_commit", "abc123").unwrap();
        store.set_meta("phase_b_commit", "abc123").unwrap();
        let (last, pb) = (
            store.get_meta("last_commit").unwrap(),
            store.get_meta("phase_b_commit").unwrap(),
        );
        assert_eq!(last.as_deref(), Some("abc123"));
        assert_eq!(pb.as_deref(), Some("abc123"));

        // Simulate new commit without Phase B run
        store.set_meta("last_commit", "def456").unwrap();
        let (last, pb) = (
            store.get_meta("last_commit").unwrap(),
            store.get_meta("phase_b_commit").unwrap(),
        );
        assert_ne!(last, pb, "mismatch means Phase B stale");
    }
}

// ControlMessage and ControlResponse are now in travsr_ipc — no local defs needed.

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
    pub async fn run(repo_root: std::path::PathBuf, foreground: bool) -> anyhow::Result<()> {
        use fs2::FileExt as _;
        use std::sync::{Arc, Mutex};
        #[cfg(unix)]
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        #[cfg(unix)]
        use tokio::net::UnixListener;

        let travsr_dir = repo_root.join(".travsr");
        std::fs::create_dir_all(&travsr_dir).context("creating .travsr")?;

        let file_appender = tracing_appender::rolling::daily(&travsr_dir, "daemon.log");
        // Must be held for the daemon's lifetime — dropping flushes and closes daemon.log.
        let (non_blocking, _appender_guard) = tracing_appender::non_blocking(file_appender);
        use tracing_subscriber::layer::SubscriberExt as _;
        use tracing_subscriber::util::SubscriberInitExt as _;
        let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
        let file_layer = tracing_subscriber::fmt::layer()
            .with_writer(non_blocking)
            .with_ansi(false);
        let init_result = if foreground {
            tracing_subscriber::registry()
                .with(file_layer)
                .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
                .with(env_filter)
                .try_init()
        } else {
            tracing_subscriber::registry()
                .with(file_layer)
                .with(env_filter)
                .try_init()
        };
        if let Err(e) = init_result {
            eprintln!("travsr daemon: could not init file logger: {e}");
        }

        // Acquire exclusive lockfile — OS releases the lock on process death.
        let lock_path = travsr_dir.join("daemon.lock");
        // NB: intentionally NO `.truncate(true)`. O_TRUNC would leave the file
        // observably empty between this open and the PID write below, during which
        // the CLI's lock-PID fallback reads nothing and wrongly concludes "no
        // daemon" — spawning a doomed child. We overwrite + trim after acquiring
        // the lock instead (below), so the file is never empty while it is held.
        #[allow(clippy::suspicious_open_options)]
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
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
        // Write our PID into the lockfile for `daemon status`. Overwrite from the
        // start and trim any older, longer PID (we did not truncate on open).
        use std::io::{Seek as _, SeekFrom, Write as _};
        let pid = std::process::id().to_string();
        (&lock_file).seek(SeekFrom::Start(0))?;
        (&lock_file).write_all(pid.as_bytes())?;
        lock_file.set_len(pid.len() as u64)?;

        let db_path = travsr_dir.join("graph.db");
        let store = Arc::new(Mutex::new(
            SqliteStore::open(&db_path).context("opening graph.db")?,
        ));
        // R5 (#342): separate read-only connection so Query messages do not
        // hold the write mutex while the indexer worker needs it.
        let read_store = Arc::new(Mutex::new(
            SqliteStore::open_read_only(&db_path).context("opening graph.db read-only")?,
        ));

        // Wire Step 4 (semantic ANN) into the query store. Must happen after
        // both stores are open so the sidecar handshake can read the DB.
        {
            let mut rs = read_store.lock().unwrap_or_else(|e| e.into_inner());
            let mut ws = store.lock().unwrap_or_else(|e| e.into_inner());
            try_inject_embed_hook(&mut rs, &mut ws, &db_path);
        }

        // Migration: repos initialised before per-repo embed config was introduced
        // have an `embed_text_model_id` meta key in graph.db but no
        // `<repo>/.travsr/embed.toml`. Write the local config from the meta key so
        // the user's existing embed setup continues to work without re-running
        // `travsr embed init`.
        {
            let repo_embed_cfg = travsr_dir.join("embed.toml");
            if !repo_embed_cfg.exists() {
                let s = store.lock().unwrap_or_else(|e| e.into_inner());
                if let Ok(Some(model_id)) = s.get_meta("embed_text_model_id") {
                    if travsr_plugin_host::lookup_embed_backend(&model_id).is_some() {
                        if let Err(e) =
                            travsr_plugin_host::write_repo_backend_id(&repo_root, &model_id)
                        {
                            tracing::warn!("embed config migration failed: {e}");
                        } else {
                            tracing::info!(
                                model_id = %model_id,
                                "migrated embed config to per-repo embed.toml"
                            );
                        }
                    }
                }
            }
        }

        // #318 O2: LRU result cache for read-only queries served off the warm
        // store. Keyed on (tool, args, last_commit, phase_b_commit), so commits
        // and background Phase B refreshes invalidate it structurally.
        let query_cache = Arc::new(Mutex::new(query_cache::QueryCache::new(256)));

        // #318 O3: debounced, single-flight background Phase B refresh. A commit
        // arms it; the event loop's phase_b_tick claims a run once the debounce
        // window settles.
        let phase_b_scheduler = Arc::new(phase_b_sched::PhaseBScheduler::new(
            std::time::Duration::from_secs(30),
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
        let sock_path = travsr_ipc::ControlAddr::for_repo(&repo_root).socket_path(&travsr_dir);
        #[cfg(unix)]
        let _ = std::fs::remove_file(&sock_path);

        // watcher::spawn() blocks until the initial kqueue/inotify scan is fully
        // established and .travsr/ is unwatched. Only then is it safe to create
        // the socket — otherwise kqueue opens the socket file and gets ENOTSUP,
        // crashing the entire watch setup.
        let _watcher_handle =
            watcher::spawn(&repo_root, tx.clone(), start_time).context("starting file watcher")?;

        // Control socket bound AFTER watcher scan completes (see above).
        #[cfg(unix)]
        let mut listener = UnixListener::bind(&sock_path).context("binding control socket")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o600));
        }

        let mut gc_tick = tokio::time::interval(std::time::Duration::from_secs(300));
        // #318 O3: poll the Phase B scheduler often enough to honour the debounce
        // window without busy-waiting. The tick is cheap (a single mutex peek);
        // it only spawns work when a re-run is actually due.
        let mut phase_b_tick = tokio::time::interval(std::time::Duration::from_secs(5));
        // Polls every 60 s to spawn embed Phase 2 once Phase 1 is complete.
        // Phase 2 must not overlap Phase 1 — both write node_embeddings in embed.db.
        let mut embed_tick = tokio::time::interval(std::time::Duration::from_secs(60));
        let embed_phase2_spawned = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut shutdown = std::pin::pin!(tokio::signal::ctrl_c());

        tracing::info!(repo = %repo_root.display(), "travsr daemon started");
        #[cfg(unix)]
        tracing::info!(transport = "unix", sock = %sock_path.display(), "control socket bound");

        // Windows Named Pipe setup — resolved address for use in the accept task.
        #[cfg(windows)]
        let pipe_name = travsr_ipc::ControlAddr::for_repo(&repo_root).pipe_name();
        #[cfg(windows)]
        tracing::info!(transport = "named_pipe", pipe = %pipe_name, "control pipe bound");

        let repo_root_arc = Arc::new(repo_root.clone());
        // Notify used for socket-initiated shutdown signal.
        #[cfg(unix)]
        let sock_shutdown = Arc::new(tokio::sync::Notify::new());
        // Notify used for pipe-initiated shutdown signal (Windows).
        #[cfg(not(unix))]
        let pipe_shutdown = Arc::new(tokio::sync::Notify::new());

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
        // worker_stop lets shutdown signal the worker to exit even though
        // index_tx_worker (the clone held by the closure) keeps the channel
        // alive after drop(index_tx). Without this, recv() never returns Err,
        // indexer_worker.await hangs, and the tokio runtime's blocking-thread
        // pool stalls on macOS/Windows.
        //
        // worker_stop_guard sets the flag whenever Daemon::run exits — whether
        // via graceful shutdown, tokio task abort() (Windows test path), or
        // panic. Tokio runs Drop on abort() before considering the task done.
        let worker_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let _worker_stop_guard = {
            struct Guard(Arc<std::sync::atomic::AtomicBool>);
            impl Drop for Guard {
                fn drop(&mut self) {
                    self.0.store(true, std::sync::atomic::Ordering::Release);
                }
            }
            Guard(Arc::clone(&worker_stop))
        };
        let indexer_worker = {
            let store_worker = Arc::clone(&store);
            let repo_worker = Arc::clone(&repo_root_arc);
            let index_tx_worker = index_tx.clone();
            let worker_stop_inner = Arc::clone(&worker_stop);
            tokio::task::spawn_blocking(move || {
                loop {
                    match index_rx.recv_timeout(std::time::Duration::from_millis(50)) {
                        Ok(ev) => {
                            handle_watch_event(ev, &repo_worker, &store_worker, &index_tx_worker);
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            if worker_stop_inner.load(std::sync::atomic::Ordering::Acquire) {
                                break;
                            }
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                tracing::debug!("indexer worker exiting");
            })
        };

        // Windows: spawn named pipe accept loop. Runs until the runtime is shut
        // down or until the accept fails (which happens when the daemon exits).
        #[cfg(windows)]
        {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
            use tokio::net::windows::named_pipe::ServerOptions;

            let store_win = Arc::clone(&store);
            let read_store_win = Arc::clone(&read_store);
            let repo_win = Arc::clone(&repo_root_arc);
            let sd_win = Arc::clone(&pipe_shutdown);
            let cache_win = Arc::clone(&query_cache);
            let sched_win = Arc::clone(&phase_b_scheduler);
            let index_tx_win = index_tx.clone();
            let pipe_name_accept = pipe_name.clone();
            tokio::spawn(async move {
                let mut first_instance = true;
                loop {
                    let server = {
                        let mut opts = ServerOptions::new();
                        if first_instance {
                            opts.first_pipe_instance(true);
                            first_instance = false;
                        }
                        match opts.create(&pipe_name_accept) {
                            Ok(s) => s,
                            Err(e) => {
                                tracing::error!("named pipe server create failed: {e}");
                                return;
                            }
                        }
                    };
                    if server.connect().await.is_err() {
                        // Runtime is shutting down or pipe was forcibly closed.
                        break;
                    }
                    let store = Arc::clone(&store_win);
                    let read_store = Arc::clone(&read_store_win);
                    let repo = Arc::clone(&repo_win);
                    let sd = Arc::clone(&sd_win);
                    let cache = Arc::clone(&cache_win);
                    let sched = Arc::clone(&sched_win);
                    let index_tx_conn = index_tx_win.clone();
                    tokio::spawn(async move {
                        let (reader, mut writer) = tokio::io::split(server);
                        let mut lines = BufReader::new(reader).lines();
                        if let Ok(Some(line)) = lines.next_line().await {
                            let (resp, shutdown_requested) =
                                tokio::task::spawn_blocking(move || {
                                    handle_control_message(
                                        &line,
                                        repo.as_path(),
                                        &store,
                                        &read_store,
                                        &cache,
                                        &sched,
                                        &index_tx_conn,
                                    )
                                })
                                .await
                                .unwrap_or_else(|_| {
                                    (travsr_ipc::ControlResponse::err("internal error"), false)
                                });
                            let _ = writer
                                .write_all(
                                    format!(
                                        "{}\n",
                                        serde_json::to_string(&resp).unwrap_or_default()
                                    )
                                    .as_bytes(),
                                )
                                .await;
                            if shutdown_requested {
                                sd.notify_one();
                            }
                        }
                    });
                }
            });
        }

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
                        let read_store = Arc::clone(&read_store);
                        let repo = Arc::clone(&repo_root_arc);
                        let sd_notify = Arc::clone(&sock_shutdown);
                        let cache = Arc::clone(&query_cache);
                        let sched = Arc::clone(&phase_b_scheduler);
                        let index_tx_conn = index_tx.clone();
                        tokio::spawn(async move {
                            let (reader, mut writer) = conn.into_split();
                            let mut lines = BufReader::new(reader).lines();
                            if let Ok(Some(line)) = lines.next_line().await {
                                // reindex_files blocks on I/O and the store mutex —
                                // run on the blocking thread pool so we don't stall
                                // the Tokio executor.
                                let (resp, shutdown_requested) =
                                    tokio::task::spawn_blocking(move || {
                                        handle_control_message(
                                            &line,
                                            repo.as_path(),
                                            &store,
                                            &read_store,
                                            &cache,
                                            &sched,
                                            &index_tx_conn,
                                        )
                                    })
                                    .await
                                    .unwrap_or_else(|_| (
                                        travsr_ipc::ControlResponse::err("internal error"),
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
                    _ = phase_b_tick.tick() => {
                        // C3: .travsr is in SKIP_DIRS so the file watcher never fires
                        // for graph.db deletions. Poll every 5 s as the only trigger.
                        if !db_path.exists() {
                            eprintln!(
                                "travsr daemon: graph.db removed — exiting. Re-run `travsr init` to rebuild."
                            );
                            std::process::exit(0);
                        }
                        // M9: .travsr is in SKIP_DIRS so the watcher never sees a
                        // socket deletion. Poll here (same 5 s tick) and re-bind if
                        // the control socket vanished, so the daemon self-heals into
                        // reachability instead of running blind.
                        #[cfg(unix)]
                        if !sock_path.exists() {
                            let _ = std::fs::remove_file(&sock_path);
                            match tokio::net::UnixListener::bind(&sock_path) {
                                Ok(l) => {
                                    use std::os::unix::fs::PermissionsExt;
                                    let _ = std::fs::set_permissions(
                                        &sock_path,
                                        std::fs::Permissions::from_mode(0o600),
                                    );
                                    listener = l;
                                    tracing::warn!(sock = %sock_path.display(),
                                        "control socket was missing, re-bound");
                                }
                                Err(e) => tracing::warn!(sock = %sock_path.display(),
                                    "control socket missing and re-bind failed: {e}; retrying next tick"),
                            }
                        }
                        // Auto-arm when Phase B is pending (deferred init, or daemon
                        // restarted after a crash mid-Phase-B).
                        arm_phase_b_if_pending(&store, &phase_b_scheduler);
                        // #318 O3: start a background Phase B refresh iff one is
                        // due (armed + past debounce) and none is already running.
                        if phase_b_scheduler.try_claim() {
                            let store_bg = Arc::clone(&store);
                            let repo_bg = Arc::clone(&repo_root_arc);
                            let sched_bg = Arc::clone(&phase_b_scheduler);
                            let p2_flag = Arc::clone(&embed_phase2_spawned);
                            // Reset the Phase 2 flag so the embed_tick re-evaluates
                            // after the new Phase B + Phase 1 run completes.
                            p2_flag.store(false, std::sync::atomic::Ordering::Relaxed);
                            tokio::task::spawn_blocking(move || {
                                run_background_phase_b(repo_bg.as_path(), &store_bg, &sched_bg);
                            });
                        }
                    }
                    _ = embed_tick.tick() => {
                        let store_bg = Arc::clone(&store);
                        let repo_bg = Arc::clone(&repo_root_arc);
                        let p2_flag = Arc::clone(&embed_phase2_spawned);
                        tokio::task::spawn_blocking(move || {
                            maybe_spawn_embed(repo_bg.as_path(), &store_bg, &p2_flag);
                        });
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
                let pipe_shutdown_wait = pipe_shutdown.notified();
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
                    _ = phase_b_tick.tick() => {
                        // C3: poll every 5 s since .travsr is in SKIP_DIRS.
                        if !db_path.exists() {
                            eprintln!(
                                "travsr daemon: graph.db removed — exiting. Re-run `travsr init` to rebuild."
                            );
                            std::process::exit(0);
                        }
                        // Auto-arm when Phase B is pending (deferred init, or daemon
                        // restarted after a crash mid-Phase-B).
                        arm_phase_b_if_pending(&store, &phase_b_scheduler);
                        // #318 O3: start a background Phase B refresh iff one is
                        // due (armed + past debounce) and none is already running.
                        if phase_b_scheduler.try_claim() {
                            let store_bg = Arc::clone(&store);
                            let repo_bg = Arc::clone(&repo_root_arc);
                            let sched_bg = Arc::clone(&phase_b_scheduler);
                            let p2_flag = Arc::clone(&embed_phase2_spawned);
                            p2_flag.store(false, std::sync::atomic::Ordering::Relaxed);
                            tokio::task::spawn_blocking(move || {
                                run_background_phase_b(repo_bg.as_path(), &store_bg, &sched_bg);
                            });
                        }
                    }
                    _ = embed_tick.tick() => {
                        let store_bg = Arc::clone(&store);
                        let repo_bg = Arc::clone(&repo_root_arc);
                        let p2_flag = Arc::clone(&embed_phase2_spawned);
                        tokio::task::spawn_blocking(move || {
                            maybe_spawn_embed(repo_bg.as_path(), &store_bg, &p2_flag);
                        });
                    }
                    _ = &mut shutdown => {
                        tracing::info!("travsr daemon shutting down");
                        break;
                    }
                    _ = pipe_shutdown_wait => {
                        tracing::info!("travsr daemon shutting down (control pipe)");
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
        // 2. Signal the indexer worker: set stop flag first (so the worker can
        //    exit via the recv_timeout path even though index_tx_worker keeps
        //    the channel open), then drop index_tx as a belt-and-suspenders
        //    close signal for the Disconnected path.
        worker_stop.store(true, std::sync::atomic::Ordering::Release);
        drop(index_tx);
        // 3. Wait for the indexer worker to finish draining index_rx and exit.
        //    Without this await, the tokio runtime would wait for the detached
        //    spawn_blocking task at shutdown, which caused the test hang.
        //    30s is generous; in practice the worker exits in milliseconds once
        //    index_tx is dropped.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(30), indexer_worker).await;

        // L8: SIGTERM any in-flight embed reindex sidecar so it is not orphaned.
        travsr_plugin_host::terminate_inflight_reindex();

        #[cfg(unix)]
        let _ = std::fs::remove_file(&sock_path);
        drop(lock_file);
        tracing::info!("travsr daemon stopped");
        Ok(())
    }
}

/// Hard cap on Tier-0 dirty callers enqueued per reindex operation.
///
/// Overflow drops the excess; the next-commit Phase B or `travsr init`
/// reconcile covers missed re-resolutions.
const DIRTY_QUEUE_CAP: usize = 100_000;

/// Send Tier-0 re-resolve requests for `callers` into the indexer channel.
///
/// Each caller is sent as a `WatchEvent::Upsert` so it runs through the same
/// `reindex_files` path as a normal watcher event. Callers whose path is
/// absent on disk are skipped (a concurrent delete will arrive as a Remove
/// event separately). The cap enforces the `MAX_PENDING` bound from §4.
fn enqueue_dirty_callers(
    callers: travsr_core::DirtySet,
    repo_root: &std::path::Path,
    index_tx: &std::sync::mpsc::Sender<watcher::WatchEvent>,
) {
    if callers.is_empty() {
        return;
    }
    let total = callers.len();
    let mut enqueued = 0usize;
    for caller in callers.into_iter().take(DIRTY_QUEUE_CAP) {
        let abs = repo_root.join(&caller);
        if !abs.exists() {
            continue;
        }
        if index_tx.send(watcher::WatchEvent::Upsert(abs)).is_err() {
            tracing::warn!("Tier-0: indexer channel closed, dropping remaining callers");
            break;
        }
        enqueued += 1;
    }
    if total > DIRTY_QUEUE_CAP {
        tracing::warn!(
            total,
            "Tier-0: dirty caller set ({total}) exceeded cap {DIRTY_QUEUE_CAP}; \
             excess deferred to Phase B / next reconcile"
        );
    } else {
        tracing::debug!(
            enqueued,
            "Tier-0: enqueued {enqueued} dirty caller(s) for re-resolution"
        );
    }
}

fn handle_watch_event(
    ev: watcher::WatchEvent,
    repo_root: &std::path::Path,
    store: &std::sync::Mutex<SqliteStore>,
    index_tx: &std::sync::mpsc::Sender<watcher::WatchEvent>,
) {
    use watcher::WatchEvent;

    match ev {
        WatchEvent::Upsert(path) => {
            let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
            match reindex_files(std::slice::from_ref(&path), repo_root, &mut s) {
                Ok(callers) => enqueue_dirty_callers(callers, repo_root, index_tx),
                Err(e) => tracing::warn!(path=%path.display(), err=%e, "watcher reindex failed"),
            }
        }
        WatchEvent::Remove(path) => {
            let vname_path = path
                .strip_prefix(repo_root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
            // Read corpus from meta so delete_file uses the correct VName scope.
            let corpus = s.get_meta("corpus").ok().flatten().unwrap_or_default();
            match s.delete_file(&corpus, &vname_path) {
                Ok(callers) => enqueue_dirty_callers(callers, repo_root, index_tx),
                Err(e) => {
                    tracing::warn!(path=%path.display(), err=%e, "watcher delete_file failed")
                }
            }
        }
    }
}

/// Normalize the `query` field of NL tool args so punctuation variants like
/// `"work?"` and `"work ?"` collapse to the same cache key and embedding input.
/// Only applied to the `"ask"` tool — symbol-name tools are left unchanged.
fn normalize_nl_query_args(tool: &str, mut args: serde_json::Value) -> serde_json::Value {
    if tool == "ask" {
        if let Some(q) = args.get("query").and_then(|v| v.as_str()) {
            let normalized = travsr_store::fts_tokenize::normalize_nl_query(q);
            args["query"] = serde_json::Value::String(normalized);
        }
    }
    args
}

/// Returns `(response, should_shutdown)`.
///
/// Called from the Unix domain-socket accept loop and the Windows Named Pipe
/// accept loop. Gated to the two supported control-plane platforms so the
/// compiler does not emit dead_code on exotic targets.
#[cfg(any(unix, windows))]
fn handle_control_message(
    line: &str,
    repo_root: &std::path::Path,
    store: &std::sync::Mutex<SqliteStore>,
    read_store: &std::sync::Mutex<SqliteStore>,
    cache: &std::sync::Mutex<query_cache::QueryCache>,
    phase_b_scheduler: &phase_b_sched::PhaseBScheduler,
    index_tx: &std::sync::mpsc::Sender<watcher::WatchEvent>,
) -> (travsr_ipc::ControlResponse, bool) {
    use travsr_ipc::{ControlMessage, ControlResponse};

    match serde_json::from_str::<ControlMessage>(line) {
        Ok(ControlMessage::ReindexCommit { sha }) => {
            tracing::info!(sha=%sha, "control: reindex-commit");
            let paths = match changed_files_from_git(repo_root) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(err=%e, "changed_files_from_git failed — reindexing all tracked files");
                    vec![]
                }
            };
            let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
            let dirty = match reindex_files(&paths, repo_root, &mut s) {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(err=%e, "control reindex failed");
                    return (ControlResponse::err(e.to_string()), false);
                }
            };
            drop(s);
            enqueue_dirty_callers(dirty, repo_root, index_tx);
            // #318 O3: Phase A is now fresh for this commit; arm a debounced
            // background Phase B refresh so the semantic layer catches up too.
            phase_b_scheduler.mark_dirty();
            (
                ControlResponse::ok(Some(format!("reindexed {} paths", paths.len()))),
                false,
            )
        }
        Ok(ControlMessage::ReindexPaths { paths }) => {
            let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
            let dirty = match reindex_files(&paths, repo_root, &mut s) {
                Ok(d) => d,
                Err(e) => return (ControlResponse::err(e.to_string()), false),
            };
            drop(s);
            enqueue_dirty_callers(dirty, repo_root, index_tx);
            (ControlResponse::ok(None), false)
        }
        Ok(ControlMessage::Status) => {
            let s = read_store.lock().unwrap_or_else(|e| e.into_inner());
            let last_commit = s.get_meta("last_commit").ok().flatten().unwrap_or_default();
            let phase_b_commit = s
                .get_meta("phase_b_commit")
                .ok()
                .flatten()
                .unwrap_or_default();
            let nodes = s.node_count().unwrap_or(0);
            let edges = s.edge_count().unwrap_or(0);

            // Live Phase B activity from the scheduler.
            let phase_b_activity = if phase_b_scheduler.is_running() {
                "running".to_string()
            } else if phase_b_scheduler.is_pending() {
                "pending (debounce)".to_string()
            } else if last_commit.is_empty() {
                "not run (no commits yet)".to_string()
            } else if phase_b_commit.is_empty() {
                "pending".to_string()
            } else if phase_b_commit == last_commit {
                "up-to-date".to_string()
            } else {
                "stale (new commits since last run)".to_string()
            };

            // Embed progress — per-repo configured model only.
            let embed_line = if let Some(backend_id) =
                travsr_plugin_host::repo_backend_id(repo_root)
            {
                // Derive the same Phase-1 threshold `embed status` and the actual
                // embed logic use, so the phase split here isn't a different number.
                let threshold = travsr_plugin_host::derive_phase1_threshold_for_status(
                    &repo_root.join(".travsr/graph.db"),
                )
                .unwrap_or(3);
                match s.embed_progress(&backend_id, threshold) {
                    Ok((total, embedded, phase1_total, phase1_done)) => {
                        let phase2_done = embedded.saturating_sub(phase1_done);
                        let phase2_total = total.saturating_sub(phase1_total);
                        let pct = (embedded * 100).checked_div(total).unwrap_or(100);
                        format!(
                            "embedding ({backend_id}): {embedded}/{total} ({pct}%) — \
                             Phase 1: {phase1_done}/{phase1_total} · Phase 2: {phase2_done}/{phase2_total}"
                        )
                    }
                    Err(_) => format!("embedding ({backend_id}): progress unavailable"),
                }
            } else {
                "embedding: no backend active".to_string()
            };

            let msg = format!(
                "nodes: {nodes} | edges: {edges} | last_commit: {last_commit}\n\
                 phase B : {phase_b_activity}\n\
                 {embed_line}"
            );
            (ControlResponse::ok(Some(msg)), false)
        }
        Ok(ControlMessage::Shutdown) => (ControlResponse::ok(None), true),
        // WS3 (#420): pause auto-reindex and gracefully cancel any in-flight run.
        Ok(ControlMessage::StopEmbed) => {
            tracing::info!("control: stop-embed — pausing auto-reindex + cancelling in-flight");
            travsr_plugin_host::pause_embed();
            let was_running = travsr_plugin_host::embed_reindex_in_flight();
            travsr_plugin_host::terminate_inflight_reindex();
            let msg = if was_running {
                "embed auto-reindex paused; in-flight reindex cancelled (partial embeddings preserved)"
            } else {
                "embed auto-reindex paused (nothing was running)"
            };
            (ControlResponse::ok(Some(msg.to_string())), false)
        }
        Ok(ControlMessage::ResumeEmbed) => {
            tracing::info!("control: resume-embed — clearing auto-reindex pause");
            travsr_plugin_host::resume_embed();
            (
                ControlResponse::ok(Some("embed auto-reindex resumed".to_string())),
                false,
            )
        }
        // #318 O1: read-only CLI queries served from the daemon's warm store —
        // skips the per-command store open that dominates CLI latency.
        Ok(ControlMessage::Query {
            protocol,
            tool,
            args,
        }) => {
            if protocol != travsr_ipc::QUERY_PROTOCOL_VERSION {
                // Version skew: answer with an error so the CLI falls back to
                // its direct-open path instead of mis-rendering a payload.
                return (
                    ControlResponse::err(format!(
                        "query protocol v{protocol} != daemon v{} — falling back",
                        travsr_ipc::QUERY_PROTOCOL_VERSION
                    )),
                    false,
                );
            }
            // Normalize the NL query field before cache lookup and dispatch so
            // "work?" and "work ?" share the same cache entry and produce
            // identical embedding vectors. Only applied to NL tools ("ask");
            // symbol-name tools ("graph") are left as-is.
            let args = normalize_nl_query_args(&tool, args);
            // R5 (#342): use the dedicated read-only connection so this lock
            // does not block the indexer worker from acquiring the write store.
            let s = read_store.lock().unwrap_or_else(|e| e.into_inner());
            // #318 O2: serve from the warm result cache when the graph has not
            // moved. The commit markers are part of the key, so a stale entry can
            // never be returned — a Phase A reindex or background Phase B refresh
            // shifts the key and the next query recomputes.
            let last_commit = s.get_meta("last_commit").ok().flatten().unwrap_or_default();
            let phase_b_commit = s
                .get_meta("phase_b_commit")
                .ok()
                .flatten()
                .unwrap_or_default();
            {
                let mut c = cache.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(cached) = c.get(&tool, &args, &last_commit, &phase_b_commit) {
                    return (ControlResponse::query_result(cached), false);
                }
            }
            match run_query(&s, &tool, args.clone()) {
                Ok(value) => {
                    let mut c = cache.lock().unwrap_or_else(|e| e.into_inner());
                    c.put(&tool, &args, &last_commit, &phase_b_commit, value.clone());
                    (ControlResponse::query_result(value), false)
                }
                Err(e) => (ControlResponse::err(format!("query failed: {e:#}")), false),
            }
        }
        Err(e) => (ControlResponse::err(format!("parse error: {e}")), false),
    }
}

/// Dispatch one CLI query against the warm store (#318 O1).
#[cfg(any(unix, windows))]
fn run_query(
    store: &SqliteStore,
    tool: &str,
    args: serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    use travsr_mcp::query;
    match tool {
        "graph" => {
            let args: query::GraphQueryArgs =
                serde_json::from_value(args).context("invalid graph query args")?;
            Ok(serde_json::to_value(query::graph_query(store, &args)?)?)
        }
        "ask" => {
            let q = args
                .get("query")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("ask query missing 'query' arg"))?;
            let knn = store.embed_knn_fn();
            let knn_ref = knn
                .as_ref()
                .map(|f| f as &dyn Fn(&str, u32) -> Vec<(travsr_core::NodeId, f32)>);
            Ok(serde_json::to_value(query::ask_query(store, q, knn_ref)?)?)
        }
        "status" => Ok(serde_json::to_value(query::status_query(store)?)?),
        other => anyhow::bail!("unknown query tool '{other}'"),
    }
}

/// Try to start an embed plugin supervisor and inject its KNN hook into `store`.
///
/// Wire the embed KNN hook into the daemon stores.
///
/// `query_store` is the read-only connection used by MCP query dispatch — the
/// hook must live here so every `search_nodes_fuzzy` call reaches Step 4.
/// `write_store` is the write connection used for meta reads and writes.
///
/// No-op when the active backend binary is not installed.
/// Called once at daemon startup AFTER both stores are open and migrated.
pub fn try_inject_embed_hook(
    query_store: &mut SqliteStore,
    write_store: &mut SqliteStore,
    db_path: &Path,
) {
    use travsr_plugin_host::{
        active_backend_id, embed_backends, lookup_embed_backend, EmbedSupervisor,
    };

    let Some(home) = dirs::home_dir() else {
        tracing::debug!("embed hook: HOME not set — skipping");
        return;
    };

    // Prefer the user's active backend from ~/.travsr/embed.toml; fall back to
    // the catalog default so a fresh install without `travsr embed switch` still works.
    let backend = active_backend_id()
        .as_deref()
        .and_then(lookup_embed_backend)
        .or_else(|| embed_backends().first())
        .cloned();
    let Some(backend) = backend else {
        return;
    };

    let binary = home.join(".travsr").join("bin").join(&backend.binary_name);
    let supervisor = EmbedSupervisor::try_start(&binary, db_path, &backend.id);
    if !supervisor.is_active() {
        return;
    }

    let model_id = match supervisor.model_id() {
        Some(id) => id.to_string(),
        None => return,
    };

    // Guard: if a model_id was previously recorded it must match the plugin's.
    // When absent (first run after `travsr embed reindex`) we proceed and write it below.
    if let Ok(Some(stored)) = write_store.get_meta("current_embed_model") {
        if stored != model_id {
            tracing::warn!(
                stored_model = %stored,
                plugin_model = %model_id,
                "embed model_id mismatch — Step 4 disabled. \
                 Run `travsr embed reindex` to rebuild embeddings with the installed model."
            );
            return;
        }
    }

    if let Some(hook) = supervisor.knn_hook(model_id.clone()) {
        // Warm the sidecar (ONNX + HNSW load) BEFORE arming the hook so the
        // daemon's first query never pays the cold-start cost that would trip
        // the 600 ms KNN breaker and degrade to FTS. Blocking, but this runs
        // once at startup before the daemon serves any request.
        supervisor.prewarm();
        query_store.set_embed_knn_hook(hook);
        // RFC-019: inject the direct-cosine oracle beside the KNN hook. Reuses the
        // same warm sidecar for the query embedding; candidate vectors are read from
        // embed.db by travsr-store. `None` query-hook → no score hook → the FTS-only
        // path is unchanged.
        if let Some(qhook) = supervisor.embed_query_hook() {
            let embed_db = db_path.with_file_name("embed.db");
            let model = model_id.clone();
            let score: travsr_store::EmbedScoreHook =
                std::sync::Arc::new(move |q: &str, ids: &[travsr_core::NodeId]| {
                    let blob = qhook(q)?;
                    match travsr_store::decode_embedding(&blob) {
                        Some(qv) => travsr_store::score_candidates(&qv, &embed_db, &model, ids),
                        None => Ok(vec![]),
                    }
                });
            query_store.set_embed_score_hook(score);
        }
        // Persist the active model_id so future startups detect backend switches.
        if let Err(e) = write_store.set_meta("current_embed_model", &model_id) {
            tracing::warn!("embed: failed to persist current_embed_model: {e}");
        }
        tracing::info!(model_id = %model_id, "embed plugin active — Step 4 (semantic ANN) enabled");
    }
}

/// Cold-path variant of [`try_inject_embed_hook`] for read-only CLI queries —
/// intentionally a no-op.
///
/// `travsr ask` with no running daemon has no long-lived host to own a warm
/// sidecar. Spawning one per invocation cannot help: an unwarmed first KNN takes
/// ~0.6 s (model load), which overruns the host's 600 ms `ask_query` circuit-
/// breaker, so the seeds are discarded and the query falls back to FTS anyway —
/// while the process churns a throwaway 127 MB-model sidecar each call. The only
/// way to beat the breaker is a per-`ask` *blocking* prewarm, i.e. exactly the
/// duplicate-process anti-pattern we are avoiding.
///
/// So cold standalone `ask` uses FTS seeds (fast, correct, no spawn). Embedding-
/// enhanced seeds come from a long-lived host: when a daemon is running, `ask`
/// routes through it (`daemon_client::try_query`) and gets the daemon's warm,
/// prewarmed sidecar; the MCP server likewise prewarms its own singleton (see
/// [`try_inject_embed_hook`] and the MCP `embed-hook-init` thread).
pub fn try_inject_embed_hook_readonly(_store: &mut SqliteStore, _db_path: &std::path::Path) {}
