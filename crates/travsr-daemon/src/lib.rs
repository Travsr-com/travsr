//! travsr-daemon — long-running orchestrator for Travsr.
//!
//! Owns git hook installation, file watching, incremental reindexing, and
//! will host the MCP server in Sprint 3. "Always fresh" is the daemon's
//! core mandate — see CLAUDE.md principle #2.

#![forbid(unsafe_code)]

mod hook;
pub mod logfile;
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

pub use hook::{
    changed_files_from_git, install_hook, tracked_files_from_git, try_dispatch_to_daemon,
};

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

/// The user-facing product version, set once by the `travsr` binary at startup.
static BUILD_VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Record the product/build version (UX-019).
///
/// The `travsr-daemon` crate carries the workspace version (`0.7.0`), which is
/// deliberately decoupled from the user-facing `travsr --version` (`1.0.0`, the
/// npm-release line). Logging `env!("CARGO_PKG_VERSION")` therefore made the
/// daemon's session-start line disagree with `--version`. The `travsr` binary
/// (including the background daemon, which is a re-exec of the same binary) calls
/// this early with its own `CARGO_PKG_VERSION` so [`build_version`] reports one
/// consistent number everywhere.
pub fn set_build_version(version: &str) {
    let _ = BUILD_VERSION.set(version.to_string());
}

/// The product/build version to report in logs. Falls back to this crate's own
/// version when the binary did not set one (e.g. a direct library embedder).
pub fn build_version() -> &'static str {
    BUILD_VERSION
        .get()
        .map(String::as_str)
        .unwrap_or(env!("CARGO_PKG_VERSION"))
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
    /// UX-023: number of nodes swept because their file no longer exists on disk
    /// (deleted/moved upstream). Surfaced in the CLI summary — the tracing event
    /// alone is invisible under the default `error` stderr filter (see UX-002).
    pub ghosts_pruned: u64,
    /// UX-023: the ghost sweep tripped the mass-delete circuit breaker and pruned
    /// nothing. Without surfacing this the failure is silent (the warning no
    /// longer passes the default filter), so the CLI points at `fsck --fix --force`.
    pub ghost_prune_aborted: bool,
}

/// Per-language Phase B outcome, surfaced in [`InitStats`] so the CLI can
/// tell the user which analyzers ran and which were absent.
#[derive(Debug, Default, Clone)]
pub struct PhaseBReport {
    /// The corpus these outcomes were evaluated against, so the trust hint can
    /// name the exact `travsr lang add <lang> --corpus <corpus>` invocation
    /// instead of a placeholder (#414 follow-up). Empty when unknown.
    pub corpus: String,
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
    /// Registered non-builtin languages whose corpus lacks a trust grant
    /// (ADR-017 Rule 3 — #414). Shown to the user with a
    /// `travsr lang add <lang> --corpus <corpus>` call-to-action.
    pub skipped_untrusted_corpus: Vec<String>,
    /// Languages that need a `compile_commands.json` at the repo root
    /// (scip-clang, for `c`/`cpp`) but don't have one (L5a).
    pub skipped_no_compdb: Vec<String>,
    /// Languages that are RequiresElevated but have no PSE approval in lang.toml.
    /// Shown to the user with a `travsr lang approve` call-to-action.
    pub skipped_needs_approval: Vec<String>,
    /// Languages whose analyzer spawned but died or errored mid-invoke.
    pub crashed: Vec<String>,
    /// #712: languages whose analyzer ran cleanly but produced zero nodes despite
    /// the language being present in the repo. Shown to the user so a build-free
    /// tool that silently indexed nothing is not mistaken for success.
    pub produced_no_nodes: Vec<String>,
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
    /// Cargo workspace dependency markers (A2), resolved in the repo-level pass.
    workspace_dep_markers: Vec<travsr_analysis::data_format::WorkspaceDepMarker>,
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

    // L5b: `.h` is ambiguous between C and Obj-C. `paths` is already the full
    // indexable file list for this run, so checking it in memory is free (no
    // extra walk) and gives an accurate repo-wide signal.
    let objc_signal = paths.iter().any(|p| {
        matches!(
            p.extension().and_then(|e| e.to_str()),
            Some("m") | Some("mm")
        )
    });

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
                            tracing::warn!(event = "file.skipped", path = %abs_path.display(), err = %e, "hash failed, skipping");
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
                            workspace_dep_markers: vec![],
                            unchanged: true,
                        }));
                        continue;
                    }

                    // Parse Phase A. L5b: route `.h` to the Obj-C parser instead of
                    // the default C dispatch — the C grammar cannot parse
                    // `@interface`/`@protocol` headers. #610: decided per header
                    // from its own text, not from one repo-wide flag applied to all.
                    let ext = abs_path.extension().and_then(|e| e.to_str()).unwrap_or("");
                    let parsed = if ext == "h" && header_parses_as_objc(abs_path, objc_signal) {
                        travsr_analysis::objc::parse(&corpus, abs_path, &vname_path)
                    } else {
                        indexer
                            .parse_file_with_vname(abs_path, &vname_path)
                            .map_err(anyhow::Error::from)
                    };
                    let out = match parsed {
                        Ok(o) => o,
                        Err(e) => {
                            tracing::warn!(event = "file.parse_failed", path = %abs_path.display(), err = %e, "parse error, skipping");
                            continue;
                        }
                    };

                    // Import resolution (read-only FS, no store access).
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
                        workspace_dep_markers: out.workspace_dep_markers,
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
        let mut all_ws_markers: Vec<travsr_analysis::data_format::WorkspaceDepMarker> = Vec::new();

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
            all_ws_markers.extend(pr.workspace_dep_markers);
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

        // Repo-level Cargo workspace dependency resolution (A2): resolve member
        // `{ workspace = true }` entries against the root's
        // `[workspace.dependencies]` versions. Runs once the whole batch is
        // parsed so the root and all members are visible together.
        if !all_ws_markers.is_empty() {
            let indexer = PluginIndexer::new(corpus);
            let (nodes, edges) = indexer.resolve_workspace_deps(&all_ws_markers);
            for node in &nodes {
                match store.put_node(node) {
                    Ok(_) => counts.nodes_upserted += 1,
                    Err(e) => tracing::warn!(err=%e, "workspace dep node write error"),
                }
            }
            for edge in &edges {
                match store.put_edge(edge) {
                    Ok(_) => counts.edges_upserted += 1,
                    Err(e) => tracing::warn!(err=%e, "workspace dep edge write error"),
                }
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
    // Standard source root for static-site generators (Hugo, Jekyll, Gatsby,
    // Next.js content collections) — the entire doc corpus of a docs-only
    // repo commonly lives here. Without this, `travsr init` on such a repo
    // auto-excludes essentially all of it as a false-positive "large dep
    // dir", silently (non-TTY runs only log the decision via tracing::info!,
    // never in the command's own visible output) — found indexing
    // kubernetes/website while measuring #376 lifecycle plan L4.
    "content",
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

/// File count above which an embed pass announces itself before starting.
///
/// An incremental pass touches the handful of files a commit changed and lands
/// in tens of milliseconds, so announcing it only doubles the most frequent
/// line in the log. A first pass on a large repo runs for minutes, where a
/// silent log is indistinguishable from a hung daemon. The threshold sits well
/// above any incremental pass and well below a whole-repo one.
const ANNOUNCE_PASS_ABOVE_FILES: usize = 200;

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
        if node.vname.path.is_empty() {
            // Only `package` nodes have a signature to derive text from; `crate`
            // and `go-pkg` do not, and admitting them here left them queried on
            // every pass with nothing to write.
            if node.kind == "package" {
                pathless.push(node);
            }
            continue;
        }
        // Drop the structurally unfillable before grouping, not inside the
        // parse: a `file` node cannot produce text, but keeping it here is what
        // pulled its whole file into the read-and-parse set to yield nothing.
        if !travsr_analysis::skeleton::can_have_embed_text(node) {
            continue;
        }
        by_file
            .entry(node.vname.path.as_str())
            .or_default()
            .push(node);
    }
    let files: Vec<(&str, Vec<&travsr_core::Node>)> = by_file.into_iter().collect();
    // Gate on `fillable`, never on `files.is_empty()`: pathless package nodes
    // (path="") are handled below, and keying the return off `files` alone would
    // strand them without embed_text whenever they are the only nodes missing it.
    let fillable = files.iter().map(|(_, ns)| ns.len()).sum::<usize>() + pathless.len();
    if fillable == 0 {
        // Every node the query returned is unfillable. Running the pass anyway
        // is what made the log read `count=1009` five times in 90 seconds while
        // writing nothing, re-reading 5.9 MB across 476 files each time.
        tracing::debug!(
            unfillable = nodes.len(),
            "embed_text: nothing fillable this pass, skipping parse"
        );
        return;
    }
    // Announce the pass only when it is big enough that silence would read as a
    // hang. A start line for the common case doubles the volume of the most
    // frequent line in the log, which is the failure this PR exists to fix; but
    // the first pass on a large repo runs for minutes, and a log that says
    // nothing for minutes is its own bug. `unfillable` rides along here rather
    // than on the routine line: it is a property of the index that barely moves
    // between passes, so repeating it every tick is noise.
    if files.len() > ANNOUNCE_PASS_ABOVE_FILES {
        tracing::info!(
            count = fillable,
            unfillable = nodes.len().saturating_sub(fillable),
            files = files.len(),
            ?richness,
            "regenerating embed_text (parse-once-per-file, parallel)"
        );
    }
    let started = std::time::Instant::now();

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
    // `pathless` holds only `package` nodes — gated where it is built, so the
    // `fillable` count above cannot claim a node this loop then skips.
    let mut pairs = pairs;
    for node in &pathless {
        let text = node
            .vname
            .signature
            .trim_start_matches("pkg:")
            .replace(['@', '/', ':'], " ");
        pairs.push((node.id, text));
    }

    // Write path is single-threaded SQLite; batch to keep transactions small.
    let mut written = 0usize;
    for chunk in pairs.chunks(500) {
        match store.write_embed_texts_batch(chunk) {
            Ok(()) => written += chunk.len(),
            Err(e) => tracing::warn!("update_embed_texts: batch write failed: {e}"),
        }
    }

    // `written` is the field that makes a stalled pass legible. A pass that
    // reports the same `count` on every tick and `written=0` is doing the work
    // twice for nothing; without this the repetition looked like progress.
    // One line for the pass, carrying only what changed between passes.
    // `missed` is the field that makes a stall legible: a pass that writes
    // nothing while reporting work to do is doing it twice for nothing, and
    // without this the repetition read as progress. It is omitted when zero so
    // the healthy line stays short. `saturating_sub` because `written` cannot
    // exceed `fillable`, but a subtraction that can render as nonsense has no
    // place in the one output we are asking a reader to trust (cf.
    // `missing=-299`).
    let missed = fillable.saturating_sub(written);
    let elapsed_ms = started.elapsed().as_millis();
    if missed > 0 {
        tracing::info!(
            event = "embed.text.updated",
            written,
            missed,
            elapsed_ms,
            "embed_text updated"
        );
    } else {
        tracing::info!(
            event = "embed.text.updated",
            written,
            elapsed_ms,
            "embed_text updated"
        );
    }
}

/// Populate `embed_text` for doc-chunk nodes that have none (#376 W1).
///
/// [`regenerate_embed_texts_if_stale`] (#512) now runs [`update_embed_texts`]
/// on every tick, which already re-derives doc-chunk prose through the same
/// `chunk_markdown` path this function uses — so on the daemon tick this call
/// is normally a fast no-op. It stays as the backstop for callers that reach
/// the sidecar without going through #512 first (e.g. paths that spawn the
/// sidecar directly). A doc chunk that reaches the sidecar without prose used
/// to be embedded from a synthesized heading-and-path fallback, permanently —
/// the vector exists, so presence-only candidacy never revisits it. The
/// sidecar now rejects those nodes instead, which turns the silent corruption
/// into a silent *absence* unless someone fills the text in. This is that
/// someone.
///
/// Affordable on every tick: markdown chunks are re-derived by a pure
/// function of the file's bytes (no tree-sitter, no grammar load), and the
/// doc corpus is ~10³ chunks where the code corpus is ~10⁵ nodes. The store
/// lock is taken twice, briefly, never across the parse.
///
/// Returns the number of chunks filled in.
fn ensure_doc_embed_texts(store: &std::sync::Mutex<SqliteStore>, repo_root: &Path) -> usize {
    let nodes = {
        let s = store.lock().unwrap_or_else(|e| e.into_inner());
        match s.doc_nodes_missing_embed_text() {
            Ok(n) => n,
            Err(e) => {
                tracing::debug!("ensure_doc_embed_texts: query failed: {e}");
                return 0;
            }
        }
    };
    if nodes.is_empty() {
        return 0;
    }

    let canon_root = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf());
    let mut by_file: std::collections::HashMap<&str, Vec<&travsr_core::Node>> =
        std::collections::HashMap::new();
    for node in &nodes {
        if !node.vname.path.is_empty() {
            by_file
                .entry(node.vname.path.as_str())
                .or_default()
                .push(node);
        }
    }
    let richness = richness_from_meta(repo_root);
    let pairs: Vec<(travsr_core::NodeId, String)> = by_file
        .into_iter()
        .flat_map(|(path, fnodes)| {
            embed_texts_for_file(repo_root, canon_root.as_path(), path, &fnodes, richness)
        })
        .collect();
    if pairs.is_empty() {
        return 0;
    }

    let written = {
        let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
        let mut n = 0usize;
        for chunk in pairs.chunks(500) {
            match s.write_embed_texts_batch(chunk) {
                Ok(()) => n += chunk.len(),
                Err(e) => tracing::warn!("ensure_doc_embed_texts: batch write failed: {e}"),
            }
        }
        n
    };
    if written > 0 {
        tracing::info!(
            written,
            "regenerated embed_text for doc chunks before embedding"
        );
    }
    written
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
    let configured = travsr_plugin_host::repo_backend_id(repo_root)
        .and_then(|id| travsr_plugin_host::lookup_embed_backend(&id).map(|b| (id, b)));

    let mut store = travsr_store::SqliteStore::open(db_path)
        .context("opening store for embed_text regeneration")?;

    // RFC-022 D1 (RC-1): one-time FTS-content widening backfill for indexes built
    // before the `embed_text`-in-FTS change landed. Idempotent (meta-gated), so it
    // is a cheap no-op after the first run and preserves the always-fresh invariant.
    match store.backfill_fts_embed_text() {
        Ok(n) if n > 0 => tracing::info!(
            event = "embed.text.fts_backfill",
            nodes = n,
            "backfilled embed_text into FTS content"
        ),
        Ok(_) => {}
        Err(e) => tracing::warn!("embed-text FTS backfill failed: {e}"),
    }

    // No model configured for this repo. Only the *tier* decision below needs
    // one; generating the text does not, and returning here made this a silent
    // no-op on every path that goes through it. `travsr embed reindex` printed
    // "Preparing embed text for ..." and prepared nothing, and `travsr init`
    // left the same gap. The daemon's own reindex path never had it: it calls
    // `update_embed_texts` unconditionally with the same `Compact` fallback, so
    // the two disagreed about whether this work needs a model at all.
    //
    // Text with no model still earns its keep: RFC-022 D1 widens FTS content
    // with `embed_text`, so it feeds lexical retrieval whether or not a vector
    // ever gets built from it.
    let Some((active_id, backend)) = configured else {
        update_embed_texts(&mut store, repo_root, EmbedRichness::Compact);
        return Ok(false);
    };

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

    // #636: the read-only report half (node/edge counts, ghost detection,
    // orphan/self-ref-edge counts, lexical parity) is extracted onto
    // `SqliteStore` so `travsr-mcp`'s `get_graph_health` tool can answer it
    // without depending on travsr-daemon or opening the store read-write.
    // See `SqliteStore::integrity_report`'s doc comment for why ghosts are
    // detected by statting DB paths rather than re-walking disk.
    let report = store.integrity_report(repo_root)?;

    if !fix {
        return Ok(report);
    }

    // `reconcile` takes the on-disk set (walked_paths), not the ghost set.
    // Derive it as `db_paths \ report.ghost_paths` instead of re-statting
    // every path a second time (#636 review): the removed code built both
    // sets in a single `.exists()` pass specifically so the report and the
    // `--fix` deletion agree exactly ("`.exists()` matches the TOCTOU
    // re-check inside `reconcile`, and `on_disk` feeds it so the report and
    // the `--fix` deletion agree exactly"). A second independent stat pass
    // can disagree with `report.ghost_paths` if a file appears or
    // disappears in between; deriving from the report's own ghost set can't.
    // `reconcile`'s own TOCTOU re-check is still the safety net against a
    // file changing between this point and the actual delete.
    let db_paths: std::collections::HashSet<String> =
        store.get_all_file_hashes()?.into_keys().collect();
    let ghost_set: std::collections::HashSet<&str> =
        report.ghost_paths.iter().map(String::as_str).collect();
    let on_disk: std::collections::HashSet<String> = db_paths
        .into_iter()
        .filter(|path| !ghost_set.contains(path.as_str()))
        .collect();

    let policy = if force {
        travsr_core::SafetyPolicy {
            mass_delete_ceiling_pct: 1.0,
            ..Default::default()
        }
    } else {
        travsr_core::SafetyPolicy::default()
    };

    // reconcile enforces the circuit breaker, TOCTOU re-check, and batched deletes.
    let mut fix_report = store.reconcile(&on_disk, &policy, repo_root, &corpus)?;

    // Sweep any residual orphan edges (Tier-4 defect indicator).
    let orphans = store.sweep_orphans()?;
    fix_report.orphan_edges_swept = orphans;
    if orphans > 0 {
        tracing::warn!(
            orphans,
            "fsck: non-zero orphan edge count — write-path invariant violated"
        );
    }

    // #650: sweep pre-guard self-referential `ref/call` edges (edge + sites).
    let self_loops = store.sweep_self_ref_call_edges()?;
    fix_report.self_ref_call_edges_swept = self_loops;
    if self_loops > 0 {
        tracing::warn!(
            self_loops,
            "fsck: swept self-referential ref/call edges — pre-guard DB or bypassed write choke point (#650)"
        );
    }

    Ok(fix_report)
}

pub fn init_repo(repo_root: &Path) -> anyhow::Result<InitStats> {
    init_repo_with_progress(repo_root, None, false, false, &mut |_| {})
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
///
/// `force` (UX-004) bypasses the incremental up-to-date short-circuit: it purges
/// the existing graph so every file is re-parsed from scratch, even when no file
/// content changed. Needed because config that affects *semantic* output (e.g.
/// `--allow-unsandboxed-lsif` toggling whether Rust LSIF edges are built) is not
/// part of the per-file hash delta, so a plain re-init would report "up to date"
/// without actually rebuilding those edges.
pub fn init_repo_with_progress(
    repo_root: &Path,
    jobs: Option<usize>,
    semantic: bool,
    force: bool,
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
    match store.prune_tombstones(TOMBSTONE_MAX_AGE_SECS, TOMBSTONE_MAX_ROWS) {
        Ok((_total, at_risk)) if at_risk > 0 => {
            // L3: the only signal that freshness coverage degraded — a
            // tombstone was pruned before the embed sidecar ever consumed it,
            // for a node that still exists and still has an embedding row.
            tracing::warn!(
                at_risk,
                "pruned {at_risk} tombstone(s) for nodes that still have an \
                 embedding — their vector may now be stale with nothing left \
                 to invalidate it (CDC log pruning window: {}s)",
                TOMBSTONE_MAX_AGE_SECS
            );
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("tombstone GC failed (non-fatal): {e}"),
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

    // UX-004: `--force` bypasses the incremental up-to-date short-circuit by
    // purging the existing graph so every file is re-parsed below (node_count then
    // reads 0, which also re-activates the fast staging path). Config that changes
    // *semantic* output but not file content — e.g. `--allow-unsandboxed-lsif` —
    // is not part of the per-file hash delta, so without this a re-run would say
    // "up to date" while never rebuilding those edges. Uses a 100%-ceiling policy
    // because wiping the whole graph is the explicit, user-requested intent here.
    if force && store.node_count().unwrap_or(0) > 0 {
        tracing::info!("--force: purging graph for a full rebuild");
        let empty_walked = std::collections::HashSet::<String>::new();
        let purge_policy = travsr_core::SafetyPolicy {
            mass_delete_ceiling_pct: 1.0,
            ..Default::default()
        };
        store
            .reconcile(&empty_walked, &purge_policy, repo_root, &corpus)
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context("--force full-graph purge")?;
    }

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
        } else if travsr_core::is_manifest_file(
            p.file_name().and_then(|n| n.to_str()).unwrap_or(""),
        ) {
            // Name-recognized manifest (go.mod, *.csproj): index it even though
            // its extension is unmapped. No Phase B language to record.
            indexable_paths.push(p);
        }
    }
    reclassify_objc_headers(&mut present_languages, &indexable_paths);

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
                } else if travsr_core::is_manifest_file(
                    p.file_name().and_then(|n| n.to_str()).unwrap_or(""),
                ) {
                    // Name-recognized manifest (go.mod, *.csproj): unmapped ext.
                    indexable_paths.push(p);
                }
            }
            reclassify_objc_headers(&mut present_languages, &indexable_paths);
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
        .filter(|e| travsr_core::is_indexable_path(e.path()))
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

    // UX-023: sweep nodes for files that no longer exist on disk. The incremental
    // hash-delta path above only re-indexes files that are still present, so nodes
    // for files deleted or moved upstream would otherwise survive as ghosts until
    // a manual `travsr fsck --fix`. Reconcile the DB's file set against the freshly
    // walked working tree so a completed `init` is genuinely fresh ("Always fresh
    // — staleness is a bug"). This is a no-op on a first init (every walked file is
    // present) and only does work on a re-init over a changed tree. The default
    // SafetyPolicy's mass-delete circuit breaker still guards against a bad walk
    // wiping the whole graph; if it trips, nothing is deleted and we say so.
    let mut ghosts_pruned: u64 = 0;
    let mut ghost_prune_aborted = false;
    {
        let walked: std::collections::HashSet<String> = indexable_paths
            .iter()
            .map(|p| {
                p.strip_prefix(repo_root)
                    .unwrap_or(p)
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        match store.reconcile(
            &walked,
            &travsr_core::SafetyPolicy::default(),
            repo_root,
            &corpus,
        ) {
            Ok(report) if report.aborted => {
                ghost_prune_aborted = true;
                tracing::warn!(
                    reason = report.abort_reason.as_deref().unwrap_or(""),
                    "init reconcile: ghost prune tripped the mass-delete circuit breaker — \
                     deleted nothing; run `travsr fsck --fix --force` to override"
                );
            }
            Ok(report) if !report.ghost_paths.is_empty() => {
                ghosts_pruned = report.ghost_paths.len() as u64;
                tracing::info!(
                    event = "init.reconcile.pruned",
                    pruned = report.ghost_paths.len(),
                    "init reconcile: pruned nodes for files no longer on disk"
                );
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(err = %e, "init reconcile: ghost prune failed (non-fatal)"),
        }
    }

    let nodes_after = store.node_count().context("counting nodes after init")? as i64;

    // E1: reconcile Phase A edge languages now so the graph is correctly
    // labelled even when Phase B is deferred or unavailable (no analyzer). The
    // Phase B paths re-run this after adding their own edges.
    if let Err(e) = store.reconcile_edge_languages() {
        tracing::warn!("reconciling edge languages after Phase A: {e:#}");
    }

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
            let (pb_nodes, pb_edges, mut pb_refs, pb_unresolved, pb_positional, pb_outcome) =
                phase_b_indexer.invoke_phase_b_all(&inputs);
            // E3 W3b: resolve rust-analyzer LSIF positional refs against the full
            // store (cross-file + incremental-safe) into attributable ScipRefs.
            // Fail closed — a callee that resolves to no node is dropped here, so
            // no dangling ref/call edge is ever written.
            let mut lsif_covered: std::collections::HashSet<(String, u32, String)> =
                std::collections::HashSet::new();
            match store.resolve_lsif_positional_refs(&corpus, &pb_positional) {
                Ok(resolved) => {
                    tracing::debug!(
                        positional_in = pb_positional.len(),
                        resolved = resolved.len(),
                        "semantic analysis: rust-analyzer positional refs resolved"
                    );
                    // E7: remember which call sites LSIF positionally resolved,
                    // keyed by the resolved callee's leaf name too — a suppression
                    // keyed only on (path, line) would drop a second real call
                    // sharing the line with the one LSIF actually resolved (#I2).
                    lsif_covered.extend(lsif_covered_keys(&store, &resolved));
                    pb_refs.extend(resolved);
                }
                Err(e) => tracing::warn!("positional ref resolution: {e:#}"),
            }
            let (resolved, resolved_sites) = resolve_unresolved_calls(
                &store,
                &pb_unresolved,
                &pb_nodes,
                &pb_edges,
                &lsif_covered,
            );
            tracing::debug!(
                resolved_cross_crate_edges = resolved.len(),
                "semantic analysis cross-reference resolution complete"
            );
            let (report, alias_map) =
                write_phase_b_results(&mut store, &corpus, pb_nodes, pb_edges, pb_refs, pb_outcome);
            // WS-2: flag Dart packages indexed without resolved dependencies.
            record_dart_resolution_state(&mut store, repo_root, present_languages.contains("dart"));
            // E1: edges resolved by native leaf-name heuristics are tree-sitter,
            // not compiler-derived — write them separately with truthful
            // provenance instead of folding them into the SCIP batch as 'lsif'.
            if let Err(e) = store.write_phase_b_batch(&[], &resolved, "tree-sitter") {
                tracing::warn!("semantic analysis native resolved edges write error: {e:#}");
            }
            // #299 WS-4: record cross-crate call occurrence lines after the edges
            // (and their callee nodes) are in the store. #299 F2: remap dst ids
            // through the unification alias map first so a site never points at a
            // SCIP node that `write_phase_b_results` dropped.
            let resolved_sites = remap_resolved_sites(resolved_sites, &alias_map);
            if let Err(e) = store.record_edge_sites(&resolved_sites) {
                tracing::warn!("recording cross-crate edge_sites: {e:#}");
            }
            // E1: label-only reconcile of edge languages to their endpoints
            // (the schema default 'typescript' otherwise mislabels every edge).
            if let Err(e) = store.reconcile_edge_languages() {
                tracing::warn!("reconciling edge languages: {e:#}");
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
        // #712 (supersedes C4): stamp phase_b_commit when Phase B ran inline and
        // made progress — either every language was clean, or at least one
        // produced results while another crashed. A partial result marks the
        // healthy languages complete and queryable at HEAD; the crashed language
        // is surfaced via `phase_b_warnings`, not by pinning the whole repo's
        // marker behind HEAD forever. Only a total failure (nothing ran) leaves
        // the marker absent so the background scheduler retries. On the deferred
        // path we also leave it absent so `phase_b_tick` auto-arms the scheduler.
        let phase_b_made_progress = phase_b_report
            .as_ref()
            .map(|r| r.crashed.is_empty() || !r.ran.is_empty())
            .unwrap_or(false);
        if run_phase_b_inline && phase_b_made_progress {
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
        ghosts_pruned,
        ghost_prune_aborted,
    })
}

/// #521 F1: crate directory → transitively-reachable crate directories,
/// derived from the `crate:*` nodes + `Depends` edges that `extract_cargo_deps`
/// (travsr-analysis) produces as part of the *same* Phase B pass that
/// produced `unresolved`.
///
/// Built from `pb_nodes`/`pb_edges` directly rather than queried back out of
/// the store: `resolve_unresolved_calls` runs *before*
/// `write_phase_b_results` persists this Phase B pass's own output (see both
/// call sites in this file), so a store query at this point would see
/// whatever crate graph an *earlier* pass left behind — nothing, on a first
/// `init`. Empty when the corpus has no `crate:` nodes (non-Rust repos), in
/// which case every lookup returns `None` and [`call_target_reachable`]
/// degrades to a no-op.
struct CrateGraph {
    /// Crate directories, longest first, for longest-prefix path matching.
    dirs_by_len_desc: Vec<String>,
    /// Each crate directory's transitive `Depends` closure, including itself.
    reachable: std::collections::HashMap<String, std::collections::HashSet<String>>,
}

impl CrateGraph {
    fn build(pb_nodes: &[travsr_core::Node], pb_edges: &[travsr_core::Edge]) -> Self {
        // Manifest path → crate directory, e.g.
        // "crates/travsr-daemon/Cargo.toml" → "crates/travsr-daemon", bare
        // "Cargo.toml" (single-crate repo) → "". External dependencies have
        // no resolvable manifest path and are skipped — they can never be the
        // crate of a real caller or a Phase-A-indexed candidate.
        fn crate_dir(path: &str) -> Option<String> {
            if path == "Cargo.toml" {
                Some(String::new())
            } else {
                path.strip_suffix("/Cargo.toml").map(str::to_string)
            }
        }

        let mut dir_by_id: std::collections::HashMap<travsr_core::NodeId, String> =
            std::collections::HashMap::new();
        for n in pb_nodes {
            if n.kind == "crate" {
                if let Some(dir) = crate_dir(&n.vname.path) {
                    dir_by_id.insert(n.id, dir);
                }
            }
        }

        let mut direct: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for e in pb_edges {
            if e.kind != travsr_core::EdgeKind::Depends {
                continue;
            }
            let (Some(src_dir), Some(dst_dir)) = (dir_by_id.get(&e.src), dir_by_id.get(&e.dst))
            else {
                continue;
            };
            direct
                .entry(src_dir.clone())
                .or_default()
                .push(dst_dir.clone());
        }

        let mut reachable: std::collections::HashMap<String, std::collections::HashSet<String>> =
            std::collections::HashMap::new();
        for dir in dir_by_id.values() {
            if reachable.contains_key(dir) {
                continue;
            }
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            let mut queue: std::collections::VecDeque<String> = std::collections::VecDeque::new();
            seen.insert(dir.clone());
            queue.push_back(dir.clone());
            while let Some(cur) = queue.pop_front() {
                for dep in direct.get(&cur).into_iter().flatten() {
                    if seen.insert(dep.clone()) {
                        queue.push_back(dep.clone());
                    }
                }
            }
            reachable.insert(dir.clone(), seen);
        }

        let mut dirs_by_len_desc: Vec<String> = dir_by_id.into_values().collect();
        dirs_by_len_desc.sort_unstable_by_key(|d| std::cmp::Reverse(d.len()));
        dirs_by_len_desc.dedup();

        Self {
            dirs_by_len_desc,
            reachable,
        }
    }

    /// Longest-prefix match of `path` against known crate directories.
    fn crate_dir_for_path(&self, path: &str) -> Option<&str> {
        for dir in &self.dirs_by_len_desc {
            if dir.is_empty() || path == dir.as_str() || path.starts_with(&format!("{dir}/")) {
                return Some(dir.as_str());
            }
        }
        None
    }
}

/// #521 F1/F2: reject a resolved call target the caller could not possibly
/// reach, applied uniformly regardless of how the candidate was matched
/// (exact signature, leaf-name fallback, or `hint_crate` substring match).
///
/// F2 (`tests/`) is checked first — cheap, and catches same-package
/// integration-test targets that F1's crate check alone would wave through
/// (same package name, but `tests/` is a separate compilation unit with no
/// `[dependencies]` entry either way).
///
/// F1 permissively returns `true` when the caller's or candidate's crate
/// can't be resolved (no `crate:` nodes at all — non-Rust repos), so this
/// gate is a no-op outside Rust rather than a silent edge-dropper for other
/// languages that also flow through this function.
fn call_target_reachable(caller_path: &str, candidate_path: &str, crates: &CrateGraph) -> bool {
    let candidate_under_tests = candidate_path.split('/').any(|seg| seg == "tests");
    let caller_under_tests = caller_path.split('/').any(|seg| seg == "tests");
    if candidate_under_tests && !caller_under_tests {
        return false;
    }

    let Some(caller_crate) = crates.crate_dir_for_path(caller_path) else {
        return true;
    };
    let Some(candidate_crate) = crates.crate_dir_for_path(candidate_path) else {
        return true;
    };
    if caller_crate == candidate_crate {
        return true;
    }
    crates
        .reachable
        .get(caller_crate)
        .map(|set| set.contains(candidate_crate))
        .unwrap_or(false)
}

/// Leaf identifier from a `kind:Qualified.Name` signature, e.g. `"filter"`
/// from `"fn:Session.filter"` or `"method:Type.method"`. Shared by the E7
/// LSIF per-callee suppression key and the native leaf-uniqueness resolver
/// below, so both sides agree on what "the same callee" means.
///
/// The splitting rule itself lives in [`travsr_core::ident::leaf_of`], which
/// the store's fuzzy symbol correction uses too. This owned-`String` wrapper
/// stays because every caller below needs an owned key.
fn leaf_of(sig: &str) -> String {
    travsr_core::ident::leaf_of(sig).to_string()
}

/// E7 (#I2): build the `(caller_path, caller_line, callee_leaf)` suppression
/// keys for a batch of positionally-resolved `ScipRef`s. Batch-fetches each
/// resolved callee's own node to recover its signature leaf, so suppression
/// is scoped to the specific callee LSIF resolved rather than the whole
/// source line — a second real call sharing that line is not also dropped.
/// A ref whose callee node lookup fails is skipped (fail open on suppression,
/// never fail closed on emission — the native heuristic may then duplicate
/// it, which is the pre-existing worst case, not a new one).
fn lsif_covered_keys(
    store: &SqliteStore,
    resolved: &[travsr_core::ScipRef],
) -> Vec<(String, u32, String)> {
    let callee_ids: Vec<travsr_core::NodeId> = {
        let mut ids: Vec<travsr_core::NodeId> = resolved.iter().map(|r| r.callee_id).collect();
        ids.sort_unstable_by_key(|id| id.0);
        ids.dedup();
        ids
    };
    let callee_leaves: std::collections::HashMap<travsr_core::NodeId, String> = store
        .get_nodes(&callee_ids)
        .unwrap_or_else(|e| {
            tracing::warn!("lsif_covered: callee node lookup failed: {e}");
            Vec::new()
        })
        .into_iter()
        .map(|n| (n.id, leaf_of(&n.vname.signature)))
        .collect();
    resolved
        .iter()
        .filter_map(|r| {
            callee_leaves
                .get(&r.callee_id)
                .map(|leaf| (r.caller_path.clone(), r.caller_line, leaf.clone()))
        })
        .collect()
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
    pb_nodes: &[travsr_core::Node],
    pb_edges: &[travsr_core::Edge],
    // E7: call sites (caller_path, caller_line, callee_leaf) already resolved
    // by E3's positional rust-analyzer LSIF path. The native heuristic defers
    // to them, per callee — not per line, so a second real call sharing the
    // line with an LSIF-covered one is not also dropped (#I2).
    lsif_covered: &std::collections::HashSet<(String, u32, String)>,
) -> (
    Vec<travsr_core::Edge>,
    Vec<(travsr_core::NodeId, travsr_core::NodeId, u32)>,
) {
    if unresolved.is_empty() {
        return (Vec::new(), Vec::new());
    }

    let sigs: Vec<String> = {
        let mut s: Vec<String> = unresolved.iter().map(|u| u.callee_sig.clone()).collect();
        // Alternates go in the same batch, or the exact fallback below could
        // never fire: `by_sig` only ever holds what this query asked for.
        s.extend(unresolved.iter().filter_map(|u| u.alt_callee_sig.clone()));
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

    // sig → Vec<(NodeId, path, language)>
    let mut by_sig: std::collections::HashMap<&str, Vec<(travsr_core::NodeId, &str, &str)>> =
        std::collections::HashMap::new();
    for (id, sig, path, lang) in &candidates {
        by_sig
            .entry(sig.as_str())
            .or_default()
            .push((*id, path.as_str(), lang.as_str()));
    }

    // #299 R1: leaf-name fallback for method calls. `recv.method()` can't be
    // resolved to the receiver's type syntactically, so the Rust extractor emits
    // a bare `fn:method` sig; when the definition is a qualified `fn:Type.method`
    // node the exact pass above misses it. Look up the still-unmatched sigs by
    // leaf identifier and resolve only when the leaf is unique (precision).
    let unmatched_leaves: Vec<String> = {
        let mut v: Vec<String> = unresolved
            .iter()
            .filter(|u| !by_sig.contains_key(u.callee_sig.as_str()))
            .map(|u| leaf_of(&u.callee_sig))
            .filter(|l| !l.is_empty())
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    let leaf_candidates = if unmatched_leaves.is_empty() {
        Vec::new()
    } else {
        store
            .fn_nodes_by_leaf_name(&unmatched_leaves)
            .unwrap_or_else(|e| {
                tracing::warn!("resolve_unresolved_calls: leaf fallback query failed: {e}");
                Vec::new()
            })
    };
    // leaf → Vec<(NodeId, path, signature, language)>. Signature is retained
    // (unlike the exact `by_sig` pass, where it always equals the lookup key) so
    // #521 F3 can tell a qualified `fn:Type.method` candidate apart from a bare
    // `fn:name` one — a method call can only ever target the former. Language is
    // retained so the caller's language can scope the candidate pool (E4).
    let mut by_leaf: std::collections::HashMap<
        String,
        Vec<(travsr_core::NodeId, &str, &str, &str)>,
    > = std::collections::HashMap::new();
    for (id, sig, path, lang) in &leaf_candidates {
        by_leaf.entry(leaf_of(sig)).or_default().push((
            *id,
            path.as_str(),
            sig.as_str(),
            lang.as_str(),
        ));
    }

    // #529: batch-resolve qualified `fn:T.method` candidates and graph-type
    // existence for every method call whose receiver type extraction
    // recovered (`UnresolvedCall::recv_type`). Two purposes:
    //   (1) an exact `fn:T.method` match resolves the call precisely,
    //       bypassing the unique-leaf ambiguity gate below entirely — the
    //       receiver type already disambiguated it.
    //   (2) when no such node exists AND `T` itself is not any node in the
    //       graph, the call is almost certainly into a std/external type
    //       (`HashSet::insert`, `Iterator::filter`) that collided with an
    //       unrelated same-named user method under #521's leaf-uniqueness
    //       rule. Emit nothing instead of guessing (the #529 fix). When `T`
    //       IS a graph type but just doesn't have this method (trait impls,
    //       `Deref` targets the extractor can't see), fall through to
    //       today's leaf-pool resolution unchanged — see the three-way
    //       branch below.
    let recv_qualified_sigs: Vec<String> = {
        let mut v: Vec<String> = unresolved
            .iter()
            .filter_map(|u| {
                u.recv_type
                    .as_ref()
                    .map(|t| format!("method:{t}.{}", leaf_of(&u.callee_sig)))
            })
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    let recv_qualified_candidates = store
        .nodes_by_signatures(&recv_qualified_sigs)
        .unwrap_or_else(|e| {
            tracing::warn!("resolve_unresolved_calls: recv_type qualified lookup failed: {e}");
            Vec::new()
        });
    let mut by_recv_sig: std::collections::HashMap<&str, Vec<(travsr_core::NodeId, &str, &str)>> =
        std::collections::HashMap::new();
    for (id, sig, path, lang) in &recv_qualified_candidates {
        by_recv_sig
            .entry(sig.as_str())
            .or_default()
            .push((*id, path.as_str(), lang.as_str()));
    }

    let recv_type_probe_sigs: Vec<String> = {
        let mut types: Vec<&str> = unresolved
            .iter()
            .filter_map(|u| u.recv_type.as_deref())
            .collect();
        types.sort_unstable();
        types.dedup();
        types
            .iter()
            .flat_map(|t| {
                // E4: `class:` makes the receiver-type existence gate
                // language-general — Python/TypeScript/Java/Go receivers are
                // `class:T`, not Rust's `struct:`/`enum:`/`trait:`. Without it a
                // real graph class was treated as an external type (#529 branch
                // 2), dropping legitimate cross-file method edges.
                [
                    format!("struct:{t}"),
                    format!("enum:{t}"),
                    format!("trait:{t}"),
                    format!("class:{t}"),
                ]
            })
            .collect()
    };
    let known_graph_types: std::collections::HashSet<String> = store
        .nodes_by_signatures(&recv_type_probe_sigs)
        .unwrap_or_else(|e| {
            tracing::warn!("resolve_unresolved_calls: recv_type existence lookup failed: {e}");
            Vec::new()
        })
        .iter()
        .filter_map(|(_, sig, _, _)| sig.split_once(':').map(|(_, name)| name.to_string()))
        .collect();

    // #521 F1: crate dependency graph, built from this Phase B pass's own
    // in-memory output (see [`CrateGraph`] for why not the store).
    let crates = CrateGraph::build(pb_nodes, pb_edges);

    // #521 F1: batch-fetch every caller's own path so `call_target_reachable`
    // can determine its crate. `u.src` is always a same-file node (the caller
    // function), so its path is the call site's own file.
    let caller_ids: Vec<travsr_core::NodeId> = {
        let mut ids: Vec<travsr_core::NodeId> = unresolved.iter().map(|u| u.src).collect();
        ids.sort_unstable_by_key(|id| id.0);
        ids.dedup();
        ids
    };
    let caller_nodes = store.get_nodes(&caller_ids).unwrap_or_else(|e| {
        tracing::warn!("resolve_unresolved_calls: caller path lookup failed: {e}");
        Vec::new()
    });
    let caller_paths: std::collections::HashMap<travsr_core::NodeId, String> = caller_nodes
        .iter()
        .map(|n| (n.id, n.vname.path.clone()))
        .collect();
    // E4: each caller's own language, so candidate resolution can be scoped to
    // it — a Python/TS/Rust call must never resolve to a same-signature
    // definition in another language (mixed-repo cross-language collision).
    let caller_langs: std::collections::HashMap<travsr_core::NodeId, String> = caller_nodes
        .into_iter()
        .map(|n| (n.id, n.vname.language))
        .collect();

    let mut edges: Vec<travsr_core::Edge> = Vec::new();
    let mut sites: Vec<(travsr_core::NodeId, travsr_core::NodeId, u32)> = Vec::new();
    for u in unresolved {
        // E7: E3's positional rust-analyzer LSIF path resolves the same call
        // sites at 99.89% precision; where it already covered this exact site,
        // its `scip` edge supersedes the native leaf-guess heuristic (~8% precise
        // on the overlap — the `Session::filter` vs `Iterator::filter` class of
        // fabrication). Defer to it and keep only the residual LSIF did not
        // cover. An empty set (LSIF absent or degraded) leaves the native
        // heuristic as the full fallback — its intended E7 role.
        if !lsif_covered.is_empty() {
            let caller_path = caller_paths.get(&u.src).map(String::as_str).unwrap_or("");
            let key = (
                caller_path.to_owned(),
                u.caller_line,
                leaf_of(&u.callee_sig),
            );
            if lsif_covered.contains(&key) {
                continue;
            }
        }
        // Exact signature first; fall back to a leaf-name match (R1). #521 F3:
        // a method call's `callee_sig` is always the bare `fn:{name}` form
        // (extraction never sets `is_method_call` with a qualified sig), so an
        // exact `by_sig` hit there is by construction a free function — never
        // a valid method-call target. Skip straight to the leaf pool and keep
        // only qualified (`Type.method`) candidates from it.
        let matches: Vec<(travsr_core::NodeId, &str, &str)> = if u.is_method_call {
            match &u.recv_type {
                // #529: receiver type recovered — resolve against it instead
                // of guessing by leaf uniqueness. Three-way split (§4.3):
                Some(t) => {
                    let qsig = format!("method:{t}.{}", leaf_of(&u.callee_sig));
                    match by_recv_sig.get(qsig.as_str()) {
                        // (1) `fn:T.method` exists — exact resolution.
                        Some(m) => m.clone(),
                        // (2) `T` is not any node in the graph at all — the
                        // receiver is a std/external type; emit nothing
                        // rather than let it fall into the unique-leaf pool
                        // and collide with an unrelated method (the #529 bug
                        // itself, e.g. `Session::filter` vs `Iterator::filter`).
                        None if !known_graph_types.contains(t.as_str()) => continue,
                        // (3) #606: `T` is a real graph type but doesn't have
                        // this method. Pre-#606 this fell through to leaf-name
                        // uniqueness to recover trait-impl / `Deref` calls the
                        // extractor can't see — but a same-named *graph* type
                        // is not evidence the call targets it: the receiver is
                        // just as likely a std/external type whose name
                        // collides with a user type (`std::process::Command`
                        // vs the CLI's clap `enum Command`). Measured against
                        // rust-analyzer LSIF over a full native index of this
                        // repo: 1 false : 0 real — the only edge this branch
                        // emitted was `Command.stdin` fabricated onto
                        // `SandboxedSpawn.stdin`, and zero genuine trait-impl/
                        // `Deref` recoveries survived the CO-A1 uniqueness
                        // gate. Same verdict as #604 (201:2): unique-in-graph
                        // is an artifact of std/external types not being
                        // indexed. Fail closed; only an exact `by_recv_sig`
                        // hit may resolve a receiver-recovered method call.
                        // Genuine sites remain covered by the E7 LSIF/scip
                        // path.
                        None => continue,
                    }
                }
                // #604: receiver type not recovered — no evidence of a call
                // target. The pre-#529 behavior here resolved by leaf-name
                // uniqueness ("exactly one `method:*.output` in the graph, so
                // the call must be it"), which fabricated a false `ref/call`
                // edge whenever a ubiquitous std/library method name
                // (`.output()`, `.status()`, `.filter()`, `.insert()`) collided
                // with a lone same-named user method — measured 99% false
                // against rust-analyzer LSIF ground truth (201 false : 2 real).
                // Uniqueness in-graph is an artifact of std/external types not
                // being indexed, never proof of the target. Emit nothing; the
                // E7 LSIF/scip path covers genuine receiver-less sites. This
                // extends #529 (which only fail-closed the *recovered*-receiver
                // branch) to the receiver-less residual it left behind.
                None => continue,
            }
        } else {
            match by_sig.get(u.callee_sig.as_str()) {
                Some(m) => m.clone(),
                // #716 review: a second *exact* name for a call site whose kind
                // the extractor could not determine. Python's bare `Foo()` is
                // emitted as `class:Foo` on the PEP 8 convention, but a
                // PascalCase free function is legal, so `fn:Foo` is carried
                // alongside. Tried exactly and only here, before the leaf pool:
                // the aim is to find the other real definition, not to widen the
                // net, and a miss on both must still fail closed.
                None if u
                    .alt_callee_sig
                    .as_deref()
                    .is_some_and(|alt| by_sig.contains_key(alt)) =>
                {
                    by_sig[u.alt_callee_sig.as_deref().unwrap_or_default()].clone()
                }
                None => {
                    // #604 (non-method paths): the leaf fallback exists only to
                    // resolve a name whose type we could NOT determine. Two
                    // fabrication routes through it are closed here, both demanding
                    // positive type evidence:
                    //
                    // (a) A qualified callee (`method:Type.leaf`, i.e. an
                    //     associated call `Type::method`) already names its type.
                    //     If the exact `Type.method` node is absent from the store
                    //     (the `by_sig` miss that got us here), `Type` is external
                    //     (`rusqlite::Connection::open`, `File::open`) — matching a
                    //     different type's same-named method is a fabrication.
                    //     Fail closed. Only the exact `by_sig` hit above may
                    //     resolve an associated call.
                    //
                    // (b) A bare `fn:name` call (`std::io::stderr()` extracts as
                    //     `fn:stderr`) must not resolve into a qualified
                    //     `Type.method` node — a free function can only target a
                    //     free function. Keep only bare `fn:` definitions from the
                    //     leaf pool (#521 F3, applied symmetrically to the method
                    //     path, which conversely keeps only qualified candidates).
                    let callee_qualified = u
                        .callee_sig
                        .split_once(':')
                        .map(|(_, body)| body)
                        .unwrap_or(u.callee_sig.as_str())
                        .contains('.');
                    if callee_qualified {
                        Vec::new()
                    } else {
                        by_leaf
                            .get(&leaf_of(&u.callee_sig))
                            .map(|v| {
                                v.iter()
                                    .filter(|(_, _, s, _)| {
                                        let body = s.split_once(':').map(|(_, b)| b).unwrap_or(s);
                                        !body.contains('.')
                                    })
                                    .map(|(id, path, _, lang)| (*id, *path, *lang))
                                    .collect()
                            })
                            .unwrap_or_default()
                    }
                }
            }
        };
        if matches.is_empty() {
            continue;
        }
        // E4: scope candidates to the caller's own language. This is the
        // fuzzy leaf-name resolver; a cross-language name match here (e.g.
        // Python `fn:parse` → a Rust `fn:parse`) is a coincidental collision,
        // never real linkage — it is a false `ref/call` edge and also inflates
        // the candidate count, breaking the CO-A1 uniqueness gate below.
        // Genuine cross-language linkage (FFI: cgo / N-API / PyO3 / JNI) is
        // modelled separately as `ffi/call` by the RFC-005 marker resolver
        // (`ffi_resolver.rs`), which this filter does not touch — so no real
        // cross-language edge is ever suppressed here.
        let caller_lang = caller_langs.get(&u.src).map(String::as_str).unwrap_or("");
        // #I3: an empty caller_lang would filter every candidate to nothing and
        // silently drop a real call. No Phase A node-construction path leaves
        // `vname.language` empty for a function/method node (verified across
        // 16 fixture languages plus this repo's own multi-language index), but
        // guard anyway — a future gap then degrades to pre-E4 behavior
        // (candidates unfiltered by language) instead of dropping the call.
        let matches: Vec<(travsr_core::NodeId, &str, &str)> = if caller_lang.is_empty() {
            matches
        } else {
            matches
                .into_iter()
                .filter(|(_, _, lang)| *lang == caller_lang)
                .collect()
        };
        if matches.is_empty() {
            continue;
        }
        let filtered: Vec<_> = if let Some(hint) = &u.hint_crate {
            let hint_dash = hint.replace('_', "-");
            matches
                .iter()
                .filter(|(_, path, _)| path.contains(hint.as_str()) || path.contains(&*hint_dash))
                .copied()
                .collect()
        } else {
            matches
        };
        // CO-A1: bare calls with no crate hint resolve to ALL same-named functions
        // across all crates → false edges that flood get_callers / blast_radius.
        // Only emit a RefCall when the match is unambiguous (exactly one candidate).
        if u.hint_crate.is_none() && filtered.len() != 1 {
            continue;
        }
        let caller_path = caller_paths.get(&u.src).map(String::as_str).unwrap_or("");
        for (dst, path, _lang) in filtered {
            // #521 F1/F2: never emit an edge the caller's own crate could not
            // possibly reach.
            if u.src != dst && call_target_reachable(caller_path, path, &crates) {
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
/// WS-2 (Dart production readiness): record whether Dart Phase B ran with
/// resolved dependencies, so `travsr status` and `find_references` never report
/// the resulting partial index as a confident zero. Mirrors `rust_lsif_degraded`.
///
/// Empty meta = resolved (or no Dart in the repo). A comma-separated package
/// list = Dart packages missing `.dart_tool/package_config.json` — the user can
/// restore cross-package references by running `dart pub get` there. We never
/// run it ourselves (local-first: it mutates the tree and needs the network).
fn record_dart_resolution_state(store: &mut SqliteStore, repo_root: &Path, dart_present: bool) {
    let unresolved = if dart_present {
        travsr_analysis::phase_b_dart::unresolved_dep_packages(repo_root)
    } else {
        Vec::new()
    };
    let _ = store.set_meta("dart_deps_unresolved", &unresolved.join(","));
}

/// A2 follow-up: enrich `markers` with the `Provider` markers a member manifest
/// inherits from its workspace root, so a member-only incremental reindex keeps
/// inherited deps at the root version instead of the `@workspace` sentinel.
///
/// For each member `Cargo.toml` that declared an inherited dependency, walks up
/// to its workspace root and reads `[workspace.dependencies]`. Only providers
/// for a name not already present are added, so this never overrides an in-batch
/// root (root + member reindexed together) and de-duplicates a root shared by
/// several members. All filesystem work is skipped when every `Consumer` already
/// has a matching `Provider`.
fn enrich_workspace_providers(
    markers: &mut Vec<travsr_analysis::data_format::WorkspaceDepMarker>,
    member_manifests: &[PathBuf],
    repo_root: &Path,
) {
    use travsr_analysis::data_format::WorkspaceDepMarker;

    if member_manifests.is_empty() {
        return;
    }
    let mut have: std::collections::HashSet<String> = markers
        .iter()
        .filter_map(|m| match m {
            WorkspaceDepMarker::Provider { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    let unresolved = markers
        .iter()
        .any(|m| matches!(m, WorkspaceDepMarker::Consumer { name, .. } if !have.contains(name)));
    if !unresolved {
        return;
    }
    let mut added = Vec::new();
    for member in member_manifests {
        for pm in
            travsr_analysis::data_format::workspace_provider_markers_for_member(member, repo_root)
        {
            if let WorkspaceDepMarker::Provider { ref name, .. } = pm {
                if have.insert(name.clone()) {
                    added.push(pm);
                }
            }
        }
    }
    if !added.is_empty() {
        tracing::debug!(
            count = added.len(),
            "A2: enriched workspace providers from root for member-only reindex"
        );
        markers.extend(added);
    }
}

/// C1: cross-link a language's own dependency node (`crate:serde`, kind
/// `crate`) to the manifest-derived package node (`pkg:serde@ver`, kind
/// `package`) when their bare names match, using the existing `ResolvesTo` edge
/// so bare-name graph queries and traversal see the manifest data alongside the
/// module graph. Package lookup is scoped to the `crates.io` registry so a
/// same-named npm/pypi package can never link to a Rust crate. The
/// `(kind, prefix, corpus)` shape generalises to other ecosystems as their
/// symbol nodes gain stable dependency names.
///
/// Idempotent: `put_edge` dedupes on `(src, dst, kind)`, so re-running on every
/// Phase B write never accumulates duplicates. Returns the edge count.
fn cross_link_manifest_deps(store: &mut SqliteStore) -> usize {
    use std::collections::HashMap;

    let pkg_nodes = match store.nodes_by_kind("package") {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("C1 cross-link: package node scan failed: {e}");
            return 0;
        }
    };
    let mut pkg_by_name: HashMap<String, Vec<travsr_core::NodeId>> = HashMap::new();
    for n in &pkg_nodes {
        if n.vname.corpus != "crates.io" {
            continue;
        }
        if let Some(rest) = n.vname.signature.strip_prefix("pkg:") {
            let name = rest.rsplit_once('@').map(|(nm, _)| nm).unwrap_or(rest);
            if !name.is_empty() {
                pkg_by_name.entry(name.to_string()).or_default().push(n.id);
            }
        }
    }
    if pkg_by_name.is_empty() {
        return 0;
    }

    let crate_nodes = match store.nodes_by_kind("crate") {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("C1 cross-link: crate node scan failed: {e}");
            return 0;
        }
    };
    let mut linked = 0usize;
    for n in &crate_nodes {
        let Some(name) = n.vname.signature.strip_prefix("crate:") else {
            continue;
        };
        if let Some(pkg_ids) = pkg_by_name.get(name) {
            for &pkg_id in pkg_ids {
                if n.id == pkg_id {
                    continue;
                }
                match store.put_edge(&travsr_core::Edge::new(
                    n.id,
                    pkg_id,
                    travsr_core::EdgeKind::ResolvesTo,
                )) {
                    Ok(_) => linked += 1,
                    Err(e) => tracing::warn!("C1 cross-link edge write error: {e}"),
                }
            }
        }
    }
    if linked > 0 {
        tracing::info!(
            count = linked,
            "C1: cross-linked crate <-> manifest package nodes"
        );
    }
    linked
}

fn write_phase_b_results(
    store: &mut SqliteStore,
    corpus: &str,
    pb_nodes: Vec<travsr_core::Node>,
    pb_edges: Vec<travsr_core::Edge>,
    pb_refs: Vec<travsr_core::ScipRef>,
    pb_outcome: travsr_plugin_host::PhaseBOutcome,
) -> (
    PhaseBReport,
    std::collections::HashMap<travsr_core::NodeId, travsr_core::NodeId>,
) {
    let pb_node_count = pb_nodes.len();
    let pb_edge_count = pb_edges.len();
    // B: gate the C1 manifest cross-link (two unindexed full `nodes` scans) on
    // whether this cycle actually wrote any `crate` node. Computed before
    // `pb_nodes` is consumed below. C1 only creates value when `crate:*` nodes
    // exist to link: on init every crate node is written this cycle, and on an
    // incremental Rust change the touched crate's nodes are re-written. CAVEAT: a
    // manifest-only edit (a new `pkg:` node from Phase A, no crate node this
    // cycle) will not re-link to a pre-existing crate until the next Rust change
    // — acceptable for a best-effort surfacing feature, and it avoids paying the
    // scan on every non-Rust commit (e.g. a Go/k8s repo with zero crate nodes).
    let cycle_wrote_crate = pb_nodes.iter().any(|n| n.kind == "crate");
    // #299 F2: the alias map (SCIP id → unified TS id) produced by `unify_all`
    // must be returned so the caller can remap `resolved_sites.dst` — those sites
    // were resolved against the pre-unify store and may point at a SCIP node that
    // unification drops. Empty for the no-attribution (old-style sidecar) path.
    let mut alias_map: std::collections::HashMap<travsr_core::NodeId, travsr_core::NodeId> =
        std::collections::HashMap::new();
    // E6: SCIP def-unification attempt/miss counts, surfaced on the degradation
    // channel below so silent non-unification (orphaned SCIP twins) is visible.
    let mut scip_unify_attempted: usize = 0;
    let mut scip_unify_missed: usize = 0;
    if pb_refs.is_empty() {
        // Old-style sidecar: no G2 attribution data — write nodes+edges directly.
        // These are analyzer/SCIP-derived structural edges (E1: provenance 'scip').
        if let Err(e) = store.write_phase_b_batch(&pb_nodes, &pb_edges, "scip") {
            tracing::warn!("semantic analysis batch write error: {e:#}");
        }
    } else {
        // G1: unify SCIP nodes (all languages) onto tree-sitter nodes before
        // writing. Mutates pb_refs in-place to redirect callee_id to unified
        // TS nodes, and returns the alias map (SCIP id → TS id).
        let mut pb_refs_mut = pb_refs;
        let unify = crate::scip_unifier::unify_all(store, corpus, &pb_nodes, &mut pb_refs_mut);
        scip_unify_attempted = unify.attempted;
        scip_unify_missed = unify.attempted.saturating_sub(unify.unified);
        alias_map = unify.alias_map;
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
            tracing::warn!("semantic analysis attributed write error: {e:#}");
        }
        // Structural edges from SCIP relationships (Pass 2 in scip-reader) still
        // need to be written — they are not represented in ScipRef records.
        if !pb_edges.is_empty() {
            if let Err(e) = store.write_phase_b_batch(&[], &pb_edges, "scip") {
                tracing::warn!("semantic analysis structural edges write error: {e:#}");
            }
        }
    }
    if pb_node_count > 0 || pb_edge_count > 0 {
        tracing::info!(
            event = "phase_b.indexed",
            nodes = pb_node_count,
            structural_edges = pb_edge_count,
            "semantic indexing complete"
        );
    }

    // C1: now that the module graph's `crate:*` nodes are written, cross-link
    // them to the manifest-derived `pkg:*` package nodes by name so a bare-name
    // query (`graph serde`) surfaces the manifest dependency alongside the
    // language's own crate node. Skipped when this cycle wrote no crate node —
    // see `cycle_wrote_crate` above (avoids two full `nodes` scans per commit
    // for non-Rust repos and manifest-untouched cycles).
    if cycle_wrote_crate {
        cross_link_manifest_deps(store);
    }

    // H3: stamp phase_b_warnings in the meta table so `travsr status` can surface
    // actionable issues without the user having to re-read init output.
    let mut warnings: Vec<String> = Vec::new();
    for lang in &pb_outcome.crashed {
        warnings.push(format!("crashed:{lang}"));
    }
    // #712: a language whose analyzer ran but produced no nodes over its source
    // files. Surfaced so a silent zero-node "success" (e.g. scip-ruby invoked
    // without an input path) is visible and actionable in `travsr status`.
    for lang in &pb_outcome.produced_no_nodes {
        warnings.push(format!("zero_nodes:{lang}"));
    }
    for (lang, expected, got) in &pb_outcome.version_mismatch {
        warnings.push(format!("version_mismatch:{lang}:{expected}:{got}"));
    }
    for lang in &pb_outcome.skipped_needs_approval {
        warnings.push(format!("needs_approval:{lang}"));
    }
    // #449: a language present in the repo whose sidecar is not installed or
    // not registered used to be skipped silently, and the user saw "0 references"
    // with no hint that Phase B never ran. Surface both skip classes so
    // `travsr status` can print the exact `travsr lang install <lang>` fix.
    for lang in &pb_outcome.skipped_unregistered {
        warnings.push(format!("skipped_unregistered:{lang}"));
    }
    // #414 (ADR-017 Rule 3): a registered language whose corpus has no trust
    // grant is skipped before spawn — surface the exact `travsr lang add
    // <lang> --corpus <corpus>` fix.
    for lang in &pb_outcome.skipped_untrusted_corpus {
        warnings.push(format!("untrusted_corpus:{lang}"));
    }
    for lang in &pb_outcome.skipped_no_analyzer {
        warnings.push(format!("skipped_no_analyzer:{lang}"));
    }
    // L5a: scip-clang (c/cpp) needs a compile_commands.json — surface it the
    // same way as the other user-actionable skip classes above.
    for lang in &pb_outcome.skipped_no_compdb {
        warnings.push(format!("skipped_no_compdb:{lang}"));
    }
    // E6: surface SCIP def-unification misses (orphaned twins). Positional
    // span-containment makes this near-zero; a non-zero rate means Phase A
    // nodes the compiler defined were not matched, so their ref/call edges
    // landed on a duplicate SCIP node instead. Format: missed/attempted.
    if scip_unify_missed > 0 {
        warnings.push(format!(
            "scip_unification_misses:{scip_unify_missed}/{scip_unify_attempted}"
        ));
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

    let report = PhaseBReport {
        corpus: corpus.to_string(),
        ran: pb_outcome.ran,
        skipped_not_in_repo: pb_outcome.skipped_not_in_repo,
        skipped_no_analyzer: pb_outcome.skipped_no_analyzer,
        skipped_unregistered: pb_outcome.skipped_unregistered,
        skipped_untrusted_corpus: pb_outcome.skipped_untrusted_corpus,
        skipped_no_compdb: pb_outcome.skipped_no_compdb,
        skipped_needs_approval: pb_outcome.skipped_needs_approval,
        crashed: pb_outcome.crashed,
        produced_no_nodes: pb_outcome.produced_no_nodes,
        version_mismatch: pb_outcome.version_mismatch,
    };
    (report, alias_map)
}

/// #299 F2: remap `resolved_sites.dst` through the unification alias map and
/// drop any site that collapses to a self-loop (`src == dst`).
///
/// The sites were resolved against the pre-`unify_all` store, so a `dst` that
/// unification redirected to a tree-sitter node would otherwise record an
/// occurrence against a node that no longer exists (mixed-language repos). A
/// site whose `dst` remaps onto its own `src` mirrors the self-loop edge that
/// `write_phase_b_results` already drops, so it is filtered here too.
fn remap_resolved_sites(
    sites: Vec<(travsr_core::NodeId, travsr_core::NodeId, u32)>,
    alias_map: &std::collections::HashMap<travsr_core::NodeId, travsr_core::NodeId>,
) -> Vec<(travsr_core::NodeId, travsr_core::NodeId, u32)> {
    if alias_map.is_empty() {
        return sites;
    }
    sites
        .into_iter()
        .filter_map(|(src, dst, line)| {
            let dst = alias_map.get(&dst).copied().unwrap_or(dst);
            (src != dst).then_some((src, dst, line))
        })
        .collect()
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
            // Data formats and prose (#376) index in Phase A only — exclude
            // from the Phase B present_languages set so P1 never attempts a
            // nonexistent tool.
            if !lang.is_phase_a_only() {
                langs.insert(lang.as_str().to_string());
            }
            paths.push(p);
        } else if travsr_core::is_manifest_file(
            p.file_name().and_then(|n| n.to_str()).unwrap_or(""),
        ) {
            // Name-recognized manifest (go.mod, *.csproj): Phase A only.
            paths.push(p);
        }
    }

    reclassify_objc_headers(&mut langs, &paths);

    (langs, paths)
}

/// L5b: `.h` is ambiguous between C and Obj-C headers. `Language::from_extension`
/// has no repo context (deliberately — it stays a pure ext→lang map), so every
/// file-discovery walk in this module always attributes `.h` to C first.
/// Disambiguate after the fact using a repo-level signal (any `.m`/`.mm`
/// present) that can only be known once every file in the walk has been seen.
/// Shared by every `present_languages` builder (`collect_present_languages_and_paths`
/// and both walk loops in `init_repo_with_progress`) so the reclassification is
/// never applied to only one of them.
fn reclassify_objc_headers(langs: &mut std::collections::HashSet<String>, paths: &[PathBuf]) {
    let has_objc_source = paths.iter().any(|p| {
        matches!(
            p.extension().and_then(|e| e.to_str()),
            Some("m") | Some("mm")
        )
    });
    let has_header = paths
        .iter()
        .any(|p| p.extension().and_then(|e| e.to_str()) == Some("h"));
    if has_objc_source && has_header {
        langs.insert(Language::ObjectiveC.as_str().to_string());
        // Only drop "c" when no genuine .c source exists — a mixed C+Obj-C repo
        // must still enroll the C analyzer for its real .c files.
        let has_c_source = paths
            .iter()
            .any(|p| p.extension().and_then(|e| e.to_str()) == Some("c"));
        if !has_c_source {
            langs.remove(Language::C.as_str());
        }
    }
}

/// Whether a `.h` header should be parsed as Objective-C rather than C.
///
/// `repo_has_objc` alone used to decide this for every header at once, so a
/// single `.m` anywhere claimed every `.h` in the repo — including C++ headers
/// in unrelated directories, which the Objective-C grammar cannot parse. Those
/// headers indexed to a file node and nothing else, silently losing every
/// symbol they declare.
///
/// The header's own text is consulted first and is conclusive when it carries a
/// dialect marker. The repo-level signal remains the tiebreak for a plain
/// declarations header that says nothing either way, which is both the previous
/// behaviour and the right default in a repo already known to be Objective-C.
///
/// Only reached when `repo_has_objc` is already true, so a repo with no
/// Objective-C in it never pays for the read.
fn header_parses_as_objc(abs_path: &Path, repo_has_objc: bool) -> bool {
    if !repo_has_objc {
        return false;
    }
    match std::fs::read_to_string(abs_path) {
        Ok(src) => travsr_analysis::objc::header_is_objc(&src).unwrap_or(true),
        // Unreadable or non-UTF-8: fall back to the repo signal rather than
        // guessing. The parse below will surface any real read failure.
        Err(_) => true,
    }
}

/// L5b: early-exit repo-wide scan for any `.m`/`.mm` file — the signal used to
/// disambiguate a changed `.h` file's language on the commit-hook path, where
/// `reindex_files`'s own `paths` batch is a diff and may not include a sibling
/// `.m`/`.mm` that already establishes the repo as Obj-C. Callers gate this
/// behind "batch touches a `.h` file" so it costs nothing on other commits.
fn repo_has_objc_sources(repo_root: &Path) -> bool {
    use ignore::WalkBuilder;
    WalkBuilder::new(repo_root)
        .hidden(false)
        .git_ignore(true)
        .follow_links(false)
        .add_custom_ignore_filename(".travsrignore")
        .build()
        .flatten()
        .any(|entry| {
            entry.file_type().is_some_and(|t| t.is_file())
                && matches!(
                    entry.path().extension().and_then(|e| e.to_str()),
                    Some("m") | Some("mm")
                )
        })
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

    // #512: this auto-embed path used to spawn the sidecar without ever
    // reconciling `embed_text` against the active model. A model switch
    // applied by daemon restart alone (no explicit `travsr embed reindex`)
    // then embedded every symbol from `embed_text` still written at the
    // *previous* model's richness tier — vectors valid enough to reach 100%
    // coverage but too degraded to clear the new model's recall floor, so
    // `embed status` read healthy while `ask`'s KNN silently contributed
    // nothing. Two meta reads in the common case (model unchanged); the
    // expensive full-repo re-parse only runs on the tick right after a
    // genuine switch, the same self-healing precedent as
    // `maybe_spawn_invalidation_pass` for tombstones below.
    if let Err(e) = regenerate_embed_texts_if_stale(&db_path) {
        tracing::warn!("embed_tick: embed_text regen check failed (non-fatal): {e}");
    }

    // #376 W1: doc chunks are ineligible until they carry prose. Fill them in
    // before anything below reads coverage, so a freshly-indexed doc is counted
    // as pending work rather than skipped.
    ensure_doc_embed_texts(store, repo_root);

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
                tracing::debug!(
                    "embed_tick: Phase 1 pending — waiting for semantic analysis to complete"
                );
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
        // #376 W2: coverage is presence-only, so "100 % embedded" says nothing
        // about whether those vectors still match their nodes. Pending
        // tombstones are the only signal that content changed or files were
        // deleted, and nothing else in this function can see them: before this
        // branch existed, an edit-only workload never triggered a pass, so
        // stale vectors stayed live in the HNSW indefinitely.
        maybe_spawn_invalidation_pass(&db_path, store);
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

/// Pending-tombstone count observed at the last invalidation-driven spawn.
///
/// Re-arm guard for [`maybe_spawn_invalidation_pass`]. A pass does not
/// necessarily drain the log to zero: a tombstone whose node has no `embed_text`
/// yet is deliberately *deferred* by the sidecar (its current text is not
/// knowable, so deleting the vector would re-embed from a degraded fallback).
/// Without this guard the tick would see the same undrainable backlog every
/// 60 s and spawn forever. Storing the count rather than a bool means genuinely
/// new invalidation — the count moves — still spawns immediately.
///
/// Process-global because a daemon serves one repo.
static LAST_INVALIDATION_PENDING: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(u64::MAX);

/// Decide whether an invalidation-only pass should be launched this tick.
///
/// Split out from [`maybe_spawn_invalidation_pass`] so the re-arm rule is
/// testable without a sidecar binary. `last` is `u64::MAX` when no
/// invalidation-driven pass has run since the log was last empty.
fn should_spawn_invalidation(pending: u64, last: u64, in_flight: bool) -> bool {
    pending > 0 && !in_flight && last != pending
}

/// Spawn a full embed pass purely to apply pending invalidation (#376 W2).
///
/// Called only when embedding coverage is complete — there is nothing new to
/// embed, but changed or deleted content still has to be verified out of
/// `embed.db` and out of the live HNSW index.
fn maybe_spawn_invalidation_pass(db_path: &Path, store: &std::sync::Mutex<SqliteStore>) {
    use std::sync::atomic::Ordering;

    let pending = {
        let s = store.lock().unwrap_or_else(|e| e.into_inner());
        s.pending_tombstones().unwrap_or(0)
    };
    if pending == 0 {
        LAST_INVALIDATION_PENDING.store(u64::MAX, Ordering::Relaxed);
        return;
    }
    if !should_spawn_invalidation(
        pending,
        LAST_INVALIDATION_PENDING.load(Ordering::Relaxed),
        travsr_plugin_host::embed_reindex_in_flight(),
    ) {
        tracing::debug!(
            pending,
            "embed_tick: invalidation pass not re-armed (in flight, or backlog unchanged)"
        );
        return;
    }
    tracing::info!(
        pending,
        "embed_tick: fully embedded but invalidation pending — spawning verification pass"
    );
    if travsr_plugin_host::spawn_background_reindex_all(db_path) {
        LAST_INVALIDATION_PENDING.store(pending, Ordering::Relaxed);
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
    // #583: deliberately not armed by `phase_b_dirty`. Phase B is whole-project
    // (a full `rust-analyzer lsif .` plus the SCIP sidecars, ~40-60s regardless
    // of which file changed), so arming it per watcher reindex means
    // near-continuous whole-project analysis while editing, on possibly
    // non-compiling code that yields degraded LSIF. Phase B stays commit-gated;
    // `phase_b_dirty` reports the degradation instead of acting on it.
    if !last.is_empty() && last != pb {
        sched.arm_immediate();
    }
}

fn run_background_phase_b(
    repo_root: &Path,
    store: &std::sync::Mutex<SqliteStore>,
    sched: &phase_b_sched::PhaseBScheduler,
) {
    // Delegate to an inner fn so we can capture its outcome. The finish guard
    // (#404) releases the scheduler's single-flight slot on ALL exits — early
    // returns and panics alike. A bare `finish_with_outcome` after the call is
    // skipped when the inner unwinds (plugin-host sidecar, k-core, store batch
    // writes); the panic is then swallowed by the fire-and-forget
    // `spawn_blocking` join handle and `running` stays `true` forever, freezing
    // every future Phase B refresh with no error surfaced to the user.
    run_with_phase_b_finish_guard(sched, || run_background_phase_b_inner(repo_root, store));
}

/// Run `work` and release the scheduler's single-flight slot via
/// [`finish_with_outcome`](phase_b_sched::PhaseBScheduler::finish_with_outcome)
/// even if `work` panics (#404). A drop guard reports the real outcome on a
/// normal return and [`AllCrashed`](phase_b_sched::RunOutcome::AllCrashed) on an
/// unwind, so a panicking run still advances the `MAX_FAILURES` back-off instead
/// of silently wedging the slot. The panic itself is re-raised after the slot is
/// released.
fn run_with_phase_b_finish_guard(
    sched: &phase_b_sched::PhaseBScheduler,
    work: impl FnOnce() -> phase_b_sched::RunOutcome,
) {
    struct FinishGuard<'a> {
        sched: &'a phase_b_sched::PhaseBScheduler,
        outcome: phase_b_sched::RunOutcome,
    }
    impl Drop for FinishGuard<'_> {
        fn drop(&mut self) {
            self.sched.finish_with_outcome(self.outcome);
        }
    }
    let mut guard = FinishGuard {
        sched,
        outcome: phase_b_sched::RunOutcome::AllCrashed,
    };
    guard.outcome = work();
}

/// Inner worker for [`run_background_phase_b`].
///
/// Returns [`Success`](phase_b_sched::RunOutcome::Success) when no language
/// crashed (`phase_b_commit` advanced, or was already fresh),
/// [`Partial`](phase_b_sched::RunOutcome::Partial) when some semantic work
/// succeeded but ≥1 sidecar crashed (the marker did NOT advance, so the
/// scheduler applies its partial-crash back-off instead of re-running every
/// tick), and [`AllCrashed`](phase_b_sched::RunOutcome::AllCrashed) when every
/// sidecar crashed, which increments the retry-cap counter in
/// `PhaseBScheduler`.
fn run_background_phase_b_inner(
    repo_root: &Path,
    store: &std::sync::Mutex<SqliteStore>,
) -> phase_b_sched::RunOutcome {
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
            return phase_b_sched::RunOutcome::Success;
        }
        (corpus, last)
    };

    tracing::info!(
        event = "phase_b.start",
        commit = %target_sha,
        "semantic call and reference indexing starting"
    );

    // ── LSIF pass (TypeScript compiler — expensive, runs lock-free) ───────────
    // Collect edges into a Vec first; write them under the store lock below.
    // This mirrors the SCIP sidecar pattern and keeps queries warm throughout.
    let lsif_edges = run_lsif_pass_collect(repo_root, &corpus);

    // ── SCIP sidecar pass (all languages in parallel, lock-free) ─────────────
    // P6 (#329): single walk yields both present_languages and indexable_paths
    // so Phase B runners skip their own directory walks.
    let (present_languages, indexable_paths) = collect_present_languages_and_paths(repo_root);
    // WS-2: captured before `present_languages` is moved into PhaseBInputs below.
    let dart_present = present_languages.contains("dart");
    // R1: reset per-Phase-B skip latch before the run (same as init_repo_with_progress).
    travsr_indexer::sandbox::reset_ra_lsif_sandbox_skip();
    let indexer = travsr_plugin_host::PluginIndexer::new(&corpus);
    let inputs = travsr_plugin_host::PhaseBInputs {
        repo_root,
        present_languages,
        indexable_paths: &indexable_paths,
    };
    let (pb_nodes, pb_edges, mut pb_refs, pb_unresolved, pb_positional, pb_outcome) =
        indexer.invoke_phase_b_all(&inputs);

    // ── Single write batch under the lock ─────────────────────────────────────
    let mut s = store.lock().unwrap_or_else(|e| e.into_inner());

    // E3 W3b: resolve rust-analyzer LSIF positional refs against the full store
    // (cross-file + incremental-safe). Fail closed — unresolved callees dropped,
    // never dangled.
    let mut lsif_covered: std::collections::HashSet<(String, u32, String)> =
        std::collections::HashSet::new();
    match s.resolve_lsif_positional_refs(&corpus, &pb_positional) {
        Ok(resolved) => {
            tracing::debug!(
                positional_in = pb_positional.len(),
                resolved = resolved.len(),
                "semantic analysis: rust-analyzer positional refs resolved"
            );
            // E7: remember which call sites LSIF positionally resolved, keyed
            // by the resolved callee's leaf name too (#I2) — see lsif_covered_keys.
            lsif_covered.extend(lsif_covered_keys(&s, &resolved));
            pb_refs.extend(resolved);
        }
        Err(e) => tracing::warn!("positional ref resolution: {e:#}"),
    }

    let (resolved, resolved_sites) =
        resolve_unresolved_calls(&s, &pb_unresolved, &pb_nodes, &pb_edges, &lsif_covered);
    tracing::debug!(
        resolved_cross_crate_edges = resolved.len(),
        "semantic analysis cross-reference resolution complete"
    );

    // Write LSIF edges first (pre-collected lock-free above).
    for edge in &lsif_edges {
        if let Err(e) = s.put_edge_lsif(edge) {
            tracing::warn!("lsif edge write error: {e}");
        }
    }

    let (report, alias_map) =
        write_phase_b_results(&mut s, &corpus, pb_nodes, pb_edges, pb_refs, pb_outcome);
    // WS-2: flag Dart packages indexed without resolved dependencies.
    record_dart_resolution_state(&mut s, repo_root, dart_present);
    // E1: native leaf-name resolved edges are tree-sitter-heuristic — truthful
    // provenance, not the SCIP batch's 'lsif'.
    if let Err(e) = s.write_phase_b_batch(&[], &resolved, "tree-sitter") {
        tracing::warn!("semantic analysis native resolved edges write error: {e:#}");
    }
    // #299 WS-4: record cross-crate call occurrence lines after their edges land.
    // #299 F2: remap dst ids through the unification alias map so a site never
    // points at a SCIP node that unification dropped.
    let resolved_sites = remap_resolved_sites(resolved_sites, &alias_map);
    if let Err(e) = s.record_edge_sites(&resolved_sites) {
        tracing::warn!("recording cross-crate edge_sites: {e:#}");
    }
    // E1: reconcile edge languages to their endpoints (label-only).
    if let Err(e) = s.reconcile_edge_languages() {
        tracing::warn!("reconciling edge languages: {e:#}");
    }

    // #712 (supersedes C4): a single crashing language must not hold the whole
    // repo's semantic layer behind HEAD forever. Advance the completion marker
    // whenever ANY language produced results (or the LSIF pass did): those
    // languages are now complete and queryable at HEAD, and the crashed language
    // is recorded in `phase_b_warnings` (stamped by write_phase_b_results) so
    // `travsr status` reports `partial (crashed: <lang>)` and the query tools
    // stop emitting the "building in the background" note that previously never
    // resolved. The marker is left behind ONLY when a language crashed AND nothing
    // else made progress (`!crashed.is_empty()` with both `ran` and `lsif_edges`
    // empty), so the all-crash retry cap can keep trying that broken sidecar until
    // its tool is fixed. The no-op case — nothing ran and nothing crashed, e.g. no
    // analyzer is installed for any language in this repo — stamps the marker,
    // because there is nothing to wait for. A persistently crashing language is
    // retried on the next commit or an explicit `travsr reindex --semantic
    // --force`, not on an endless background loop.
    let made_progress =
        report.crashed.is_empty() || !report.ran.is_empty() || !lsif_edges.is_empty();
    if made_progress {
        let _ = s.set_meta("phase_b_commit", &target_sha);
        // #583: the semantic layer now matches the working tree again.
        let _ = s.set_meta("phase_b_dirty", "0");
    }

    let outcome = if report.crashed.is_empty() {
        phase_b_sched::RunOutcome::Success
    } else if made_progress {
        // Healthy languages advanced to HEAD, so the next scheduler tick early-
        // returns (last_commit == phase_b_commit) instead of re-running: the
        // loop settles after one back-off cycle rather than retrying a
        // persistently broken sidecar forever (#464 follow-up, #712).
        phase_b_sched::RunOutcome::Partial
    } else {
        phase_b_sched::RunOutcome::AllCrashed
    };
    let succeeded = outcome != phase_b_sched::RunOutcome::AllCrashed;

    tracing::info!(
        commit = %target_sha,
        event = "phase_b.complete",
        ran = report.ran.len(),
        lsif_edges = lsif_edges.len(),
        crashed = report.crashed.len(),
        outcome = ?outcome,
        "semantic call and reference indexing complete"
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
                    tracing::warn!(
                        "kcore: failed to update shell numbers after semantic analysis: {e}"
                    );
                } else {
                    tracing::info!(event = "kcore.updated", "graph centrality updated");
                }
            }
            Err(e) => tracing::warn!("kcore: computation failed after semantic analysis: {e}"),
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
    //
    // The `repo_backend_id` gate is the auto-embed opt-in, and it has to be
    // *here*: `spawn_background_reindex_phase1` only checks `resolve_backend`,
    // which falls back to the machine-global model when a repo has none. So
    // without this, a repo that never ran `travsr embed init` would get a full
    // embed pass merely because some *other* repo installed a backend. That is
    // the same gate `maybe_spawn_embed` applies to the tick-driven catch-up.
    if succeeded && travsr_plugin_host::repo_backend_id(repo_root).is_some() {
        let db_path = repo_root.join(".travsr/graph.db");
        if travsr_plugin_host::spawn_background_reindex_phase1(&db_path) {
            tracing::info!("triggered post-phase-B embed Phase 1");
        }
    }

    outcome
}

/// Bring the graph in line with the whole tracked tree: reindex every tracked
/// file, then delete what git no longer tracks.
///
/// This is the shape a *tree* change needs, as opposed to a commit's delta. A
/// commit is described exactly by `git diff-tree HEAD`, so the hook reindexes
/// that and nothing else. A branch checkout and a multi-commit fast-forward are
/// not described by any single commit's delta: the tip's own diff says nothing
/// about the files that differ between the two trees, so reindexing it leaves
/// the rest of the graph describing a tree that is no longer checked out.
///
/// Two halves, and both are needed. The reindex updates and adds; it cannot
/// remove, because `reindex_files` only visits the paths it is given and a file
/// that vanished is absent from that list by definition. So the prune runs
/// afterwards, over `db_file_paths − tracked`, which is the same reconcile
/// `fsck --fix` uses and carries the same mass-delete circuit breaker.
///
/// Returns the Tier-0 callers the reindex left dirty, and how many files were
/// visited.
pub fn reconcile_tracked_tree(
    repo_root: &Path,
    store: &mut SqliteStore,
) -> anyhow::Result<(travsr_core::DirtySet, usize)> {
    let mut paths = tracked_files_from_git(repo_root)
        .context("enumerating tracked files for a whole-tree reconcile")?;
    // Same ignore rules as init / the watcher / the hook path (#403).
    let ignore = watcher::build_ignore_matcher(repo_root);
    paths.retain(|p| !watcher::should_skip_all(p, repo_root, &ignore));

    let dirty = reindex_files(&paths, repo_root, store)?;

    // `walked` must match the stored VName key format exactly — relative to
    // repo_root, forward slashes — or every tracked file reads as a ghost and
    // the prune tries to delete the entire graph. Windows load-bearing;
    // `paths` are absolute PathBufs, rebuilt here the way reindex_files does.
    let walked: std::collections::HashSet<String> = paths
        .iter()
        .map(|p| {
            p.strip_prefix(repo_root)
                .unwrap_or(p)
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    let corpus = store.get_meta("corpus").ok().flatten().unwrap_or_default();
    match store.reconcile(
        &walked,
        &travsr_core::SafetyPolicy::default(),
        repo_root,
        &corpus,
    ) {
        Ok(report) if report.aborted => tracing::warn!(
            reason = report.abort_reason.as_deref().unwrap_or(""),
            "tree reconcile: ghost prune tripped the mass-delete circuit breaker — \
             deleted nothing; run `travsr fsck --fix --force` to override"
        ),
        Ok(report) if !report.ghost_paths.is_empty() => tracing::info!(
            event = "tree.reconcile.pruned",
            pruned = report.ghost_paths.len(),
            "tree reconcile: pruned files git no longer tracks"
        ),
        Ok(_) => {}
        Err(e) => tracing::warn!(err = %e, "tree reconcile: ghost prune failed"),
    }

    Ok((dirty, paths.len()))
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
    let mut all_ws_markers: Vec<travsr_analysis::data_format::WorkspaceDepMarker> = Vec::new();
    // A2 follow-up: `Cargo.toml` files in this batch that declare an inherited
    // (`{ workspace = true }`) dependency. Used after the loop to walk up to the
    // workspace root and supply the versions the root would provide, so a
    // member-only incremental reindex keeps inherited deps at the root version
    // instead of degrading them to the `@workspace` sentinel.
    let mut member_manifests: Vec<PathBuf> = Vec::new();
    let mut any_changed = false;
    // Accumulate Tier-0 dirty callers across all files in this batch.
    let mut callers_all = travsr_core::DirtySet::default();

    // L5b: unlike `index_paths_parallel`'s full-repo `paths`, this commit-hook
    // batch only contains changed files — a `.m`/`.mm` sibling may not be in it
    // even when the repo has Obj-C sources. Only pay for a repo-wide scan when
    // this batch actually touches a `.h` file (the ambiguous case); every other
    // commit costs nothing extra.
    let objc_signal = paths
        .iter()
        .any(|p| p.extension().and_then(|e| e.to_str()) == Some("h"))
        && repo_has_objc_sources(repo_root);

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
                tracing::warn!(event = "file.skipped", path = %abs_path.display(), err = %err, "hash failed, skipping");
                continue;
            }
        };
        let new_hex = hex_encode(&new_hash);

        let old_hex = store.get_file_hash(&vname_path)?;
        if old_hex.as_deref() == Some(&new_hex) {
            continue; // unchanged — skip
        }

        // §2 GC keystone: parse FIRST — a syntax error never erases the old graph.
        // L5b: route `.h` to the Obj-C parser instead of the default C dispatch
        // — the C grammar cannot parse `@interface`/`@protocol` headers.
        // #610: decided per header from its own text, not from one repo-wide
        // flag applied to every header at once.
        let ext = abs_path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let parsed = if ext == "h" && header_parses_as_objc(abs_path, objc_signal) {
            travsr_analysis::objc::parse(&corpus, abs_path, &vname_path)
        } else {
            indexer
                .parse_file_with_vname(abs_path, &vname_path)
                .map_err(anyhow::Error::from)
        };
        let out = match parsed {
            Ok(o) => o,
            Err(err) => {
                tracing::warn!(event = "file.parse_failed", path = %abs_path.display(), err = %err, "parse error, skipping");
                continue; // keep old graph intact
            }
        };

        // Build import-resolver edges before the atomic reindex_replace call.
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
                // Collect Cargo workspace dep markers for the A2 repo-level pass.
                // Record any member manifest with an inherited dep so the A2
                // block can walk up to its root for the missing versions.
                if out.workspace_dep_markers.iter().any(|m| {
                    matches!(
                        m,
                        travsr_analysis::data_format::WorkspaceDepMarker::Consumer { .. }
                    )
                }) {
                    member_manifests.push(abs_path.clone());
                }
                all_ws_markers.extend(out.workspace_dep_markers);
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

    // Cargo workspace dependency resolution (A2) — same single-pass rationale as
    // FFI above. A member `{ workspace = true }` entry re-indexed alongside its
    // workspace root gets the root's version.
    //
    // A2 follow-up: when a member is re-indexed *alone* (the common incremental
    // case — edit a member, commit; the root is not in this batch) the root's
    // `Provider` markers are absent, so the resolver would degrade the edge to
    // the `@workspace` sentinel. Before resolving, walk up from each member
    // manifest to its workspace root and synthesize the missing `Provider`
    // markers from the root's `[workspace.dependencies]`. Only Consumers still
    // unresolved after this fall back to the sentinel (a genuinely detached
    // member whose root is not on disk).
    if !all_ws_markers.is_empty() {
        enrich_workspace_providers(&mut all_ws_markers, &member_manifests, repo_root);
        let (nodes, edges) = indexer.resolve_workspace_deps(&all_ws_markers);
        for node in &nodes {
            if let Err(err) = store.put_node(node) {
                tracing::warn!("workspace dep node write error: {err}");
            }
        }
        for edge in &edges {
            if let Err(err) = store.put_edge(edge) {
                tracing::warn!("workspace dep edge write error: {err}");
            }
        }
    }

    // E1: label-only reconcile of edge languages to their endpoints (the schema
    // default 'typescript' otherwise mislabels edges rewritten by this commit).
    if let Err(e) = store.reconcile_edge_languages() {
        tracing::warn!("reconciling edge languages: {e:#}");
    }

    // Record the current HEAD commit so `travsr status` can show freshness.
    // Only write when at least one file actually changed — avoids stamping
    // noise events (sockets, gitignored files, directories) as a real reindex.
    if any_changed {
        if let Ok(sha) = read_head_commit_sha(repo_root) {
            let _ = store.set_meta("last_commit", &sha);
        }

        // #583: rewriting a file's Phase A nodes drops that file's Phase B
        // `ref/call` edges. On the commit path `last_commit` moves and
        // `last != phase_b` catches it; on the watcher path HEAD never moves,
        // so the two markers stay equal while the graph is degraded below the
        // committed snapshot. Record that here so `travsr status` can say so
        // instead of reporting `complete`. Cleared by the next completed Phase
        // B run, which is still commit-gated on purpose.
        let _ = store.set_meta("phase_b_dirty", "1");

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
        // Runs whether or not a model is configured: `richness_from_meta` falls
        // back to `Compact`, and the text feeds FTS content as well as any
        // vector built from it later.
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

pub fn read_head_commit_sha(repo_root: &Path) -> anyhow::Result<String> {
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

/// #621: reconcile the stored `last_commit` marker against live `git rev-parse
/// HEAD`. History can move while the daemon is down (reset, checkout, branch
/// switch, rebase, pull); the watcher never saw those edits, so the graph
/// stays pinned to a stale — possibly discarded — commit while `travsr status`
/// reports `phase_b: complete`. The freshness markers only ever compared
/// against each other, never against the repository.
///
/// On mismatch, run the same recovery the post-commit hook triggers, except
/// over the full tracked set: the diff between the stale graph and the new
/// HEAD is unknowable (it is not the last commit's diff), and `reindex_files`
/// is hash-delta gated so unchanged files cost one hash each. Then stamp
/// `last_commit` to HEAD (same signature-format guard as the hook path) and
/// arm a whole-project Phase B rebuild.
///
/// Skips (returns `false`) when: HEAD is unreadable (no commits / not a git
/// repo), the marker is absent (init never stamped a commit — fabricating
/// freshness here would claim an index that never ran), or marker == HEAD.
fn reconcile_head_drift(
    repo_root: &Path,
    store: &std::sync::Mutex<SqliteStore>,
    phase_b_scheduler: &phase_b_sched::PhaseBScheduler,
    index_tx: &std::sync::mpsc::Sender<watcher::WatchEvent>,
) -> bool {
    let head = match read_head_commit_sha(repo_root) {
        Ok(h) if !h.is_empty() => h,
        _ => return false,
    };
    let stored = {
        let s = store.lock().unwrap_or_else(|e| e.into_inner());
        s.get_meta("last_commit").ok().flatten().unwrap_or_default()
    };
    if stored.is_empty() || stored == head {
        return false;
    }
    tracing::info!(
        event = "head.drift.detected",
        stored = %stored,
        head = %head,
        "last_commit does not match live HEAD (history moved during daemon downtime) — reconciling"
    );
    let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
    let (dirty, files) = match reconcile_tracked_tree(repo_root, &mut s) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(err = %e, "head reconcile: reindex failed");
            return false;
        }
    };

    // Same guard as the ReindexCommit path: never claim freshness for a HEAD
    // whose reindex was skipped due to a signature-format mismatch.
    if s.get_signature_format_version().ok() == Some(SIGNATURE_FORMAT_VERSION) {
        let _ = s.set_meta("last_commit", &head);
    }
    drop(s);
    enqueue_dirty_callers(dirty, repo_root, index_tx);
    // Phase A is now aligned with HEAD; the RefCall edge set is not. Arm the
    // debounced whole-project Phase B rebuild, same as the hook path.
    phase_b_scheduler.mark_dirty();
    tracing::info!(
        event = "head.reconcile.complete",
        head = %head,
        files,
        "head reconcile complete: semantic rebuild armed"
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;
    use std::sync::Mutex;

    #[test]
    fn remap_resolved_sites_redirects_and_drops_self_loops() {
        // #299 F2: a resolved site whose dst unification redirected must be
        // remapped; a site that collapses to src == dst after remapping is
        // dropped (mirrors the self-loop edge write_phase_b_results discards).
        use travsr_core::NodeId;
        let src = NodeId(1);
        let scip_dst = NodeId(2);
        let ts_dst = NodeId(3);
        let mut alias = std::collections::HashMap::new();
        alias.insert(scip_dst, ts_dst);
        // Also alias a node onto `src` to force a self-loop after remap.
        let collapses = NodeId(4);
        alias.insert(collapses, src);

        let sites = vec![
            (src, scip_dst, 10),  // dst remaps 2 → 3
            (src, NodeId(9), 11), // dst not in alias — unchanged
            (src, collapses, 12), // dst remaps 4 → 1 == src → dropped
        ];
        let out = remap_resolved_sites(sites, &alias);
        assert_eq!(out, vec![(src, ts_dst, 10), (src, NodeId(9), 11)]);
    }

    #[test]
    fn remap_resolved_sites_empty_alias_is_identity() {
        use travsr_core::NodeId;
        let sites = vec![(NodeId(1), NodeId(2), 5)];
        let out = remap_resolved_sites(sites.clone(), &std::collections::HashMap::new());
        assert_eq!(out, sites);
    }

    // L5b: `.h` must be reclassified as Obj-C — not C — when the repo has any
    // `.m`/`.mm` source, and "c" must be dropped from the enrolled language set
    // when no genuine `.c` file backs it (pure Obj-C headers-only signal).
    #[test]
    fn collect_present_languages_and_paths_routes_h_to_objc_when_m_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cat.h"), "@interface Cat\n@end\n").unwrap();
        std::fs::write(dir.path().join("Cat.m"), "@implementation Cat\n@end\n").unwrap();

        let (langs, paths) = collect_present_languages_and_paths(dir.path());
        assert!(
            langs.contains("objectivec"),
            "expected objectivec enrolled, got {langs:?}"
        );
        assert!(
            !langs.contains("c"),
            "c must not be enrolled from .h alone when no genuine .c file exists, got {langs:?}"
        );
        assert_eq!(paths.len(), 2, "both .h and .m must remain indexable paths");
    }

    /// The `.h` routing decision must come from the header's own text, not
    /// from one repo-wide flag applied to every header at once.
    ///
    /// One `.m` file anywhere used to reclassify every `.h` in the repo. The
    /// Obj-C grammar cannot parse C++ declarations, so a C++ header caught by
    /// that produced a file node and nothing else — every symbol it declared
    /// vanished from the graph, with no error to say why.
    #[test]
    fn header_parses_as_objc_judges_each_header_by_its_own_text() {
        let dir = tempfile::tempdir().unwrap();
        let objc_header = dir.path().join("Animal.h");
        let cpp_header = dir.path().join("animal_cpp.h");
        let plain_header = dir.path().join("util.h");
        std::fs::write(&objc_header, "@interface Animal\n@end\n").unwrap();
        std::fs::write(&cpp_header, "class Animal { public: void speak(); };\n").unwrap();
        std::fs::write(&plain_header, "void f(void);\n").unwrap();

        // The repo signal is true throughout — that is the whole point: it must
        // no longer be enough on its own to claim a header for Obj-C.
        assert!(
            header_parses_as_objc(&objc_header, true),
            "a header declaring @interface is Objective-C"
        );
        assert!(
            !header_parses_as_objc(&cpp_header, true),
            "a C++ header must not be claimed by Obj-C just because the repo has a .m somewhere"
        );
        assert!(
            header_parses_as_objc(&plain_header, true),
            "a header with no dialect marker still defers to the repo signal"
        );

        // With no Obj-C in the repo at all, nothing is routed to the Obj-C
        // parser and no file is read.
        assert!(!header_parses_as_objc(&objc_header, false));
        assert!(!header_parses_as_objc(&cpp_header, false));
    }

    // L5b: without any `.m`/`.mm` sibling, `.h` must keep its default C
    // classification — pure C repos must never be reclassified.
    #[test]
    fn collect_present_languages_and_paths_keeps_h_as_c_without_objc_signal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("util.h"), "void f(void);\n").unwrap();
        std::fs::write(dir.path().join("util.c"), "void f(void) {}\n").unwrap();

        let (langs, _paths) = collect_present_languages_and_paths(dir.path());
        assert!(langs.contains("c"), "expected c enrolled, got {langs:?}");
        assert!(
            !langs.contains("objectivec"),
            "must not enroll objectivec without any .m/.mm, got {langs:?}"
        );
    }

    // L5b: a mixed C + Obj-C repo (genuine .c files alongside .m) must enroll
    // both analyzers — the "c" removal only applies when nothing backs it.
    #[test]
    fn collect_present_languages_and_paths_keeps_c_when_genuine_c_file_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("shared.h"), "void f(void);\n").unwrap();
        std::fs::write(dir.path().join("shared.c"), "void f(void) {}\n").unwrap();
        std::fs::write(dir.path().join("Cat.m"), "@implementation Cat\n@end\n").unwrap();

        let (langs, _paths) = collect_present_languages_and_paths(dir.path());
        assert!(
            langs.contains("c") && langs.contains("objectivec"),
            "mixed repo must enroll both c and objectivec, got {langs:?}"
        );
    }

    #[test]
    fn repo_has_objc_sources_detects_mm_extension() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("view.mm"), "// objc++\n").unwrap();
        assert!(repo_has_objc_sources(dir.path()));
    }

    #[test]
    fn repo_has_objc_sources_false_for_pure_c_repo() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.c"), "int main(void) { return 0; }\n").unwrap();
        assert!(!repo_has_objc_sources(dir.path()));
    }

    // L5b regression: `reclassify_objc_headers` is the single shared
    // implementation called from all three `present_languages` builders
    // (`collect_present_languages_and_paths` plus both walk loops in
    // `init_repo_with_progress` — the second loop, gated behind the
    // large-dep-dir re-walk, initially lacked this call entirely, so a `.h`
    // header in a pure Obj-C repo still enrolled the crashing "c" scip-clang
    // pass on the full `travsr init` path even though the background-refresh
    // path was already fixed). Test the shared function directly so every
    // call site inherits the same, single-sourced correctness.
    #[test]
    fn reclassify_objc_headers_drops_c_for_header_only_objc_repo() {
        let mut langs: std::collections::HashSet<String> =
            ["c".to_string(), "objectivec".to_string()]
                .into_iter()
                .collect();
        let paths = vec![PathBuf::from("/repo/Cat.h"), PathBuf::from("/repo/Cat.m")];
        reclassify_objc_headers(&mut langs, &paths);
        assert!(langs.contains("objectivec"));
        assert!(
            !langs.contains("c"),
            "c must be dropped when no genuine .c file backs it"
        );
    }

    #[test]
    fn reclassify_objc_headers_keeps_c_when_genuine_c_file_present() {
        let mut langs: std::collections::HashSet<String> =
            ["c".to_string(), "objectivec".to_string()]
                .into_iter()
                .collect();
        let paths = vec![
            PathBuf::from("/repo/shared.h"),
            PathBuf::from("/repo/shared.c"),
            PathBuf::from("/repo/Cat.m"),
        ];
        reclassify_objc_headers(&mut langs, &paths);
        assert!(langs.contains("c") && langs.contains("objectivec"));
    }

    #[test]
    fn reclassify_objc_headers_noop_without_objc_signal() {
        let mut langs: std::collections::HashSet<String> = ["c".to_string()].into_iter().collect();
        let paths = vec![PathBuf::from("/repo/util.h"), PathBuf::from("/repo/util.c")];
        reclassify_objc_headers(&mut langs, &paths);
        assert!(langs.contains("c"));
        assert!(!langs.contains("objectivec"));
    }

    // ── #521: bare-name call resolution must never cross a crate boundary
    // the caller cannot compile against, or match a method call against an
    // unrelated free function. ──────────────────────────────────────────

    #[test]
    fn test_no_phantom_edges_on_dynamic_dispatch() {
        // Literal repro from issue #521: a method call (`r.get(0)` on some
        // receiver of unknown type) must never resolve to a same-named free
        // function defined in an unrelated crate — even when that free
        // function is the *only* node with that bare signature in the whole
        // store. The old CO-A1 "unique match" guard treated global
        // uniqueness as sufficient evidence; it isn't. #521 F3 fixes this at
        // the source: a method call's `callee_sig` is always bare, so an
        // exact `by_sig` hit for it is by construction the wrong symbol shape
        // and must never be consulted.
        use travsr_core::{Node, VName};

        let mut store = SqliteStore::open_in_memory().unwrap();

        // #521: `resolve_unresolved_calls` runs before this Phase B pass's
        // own output is written to the store (see both real call sites in
        // this file), so the crate graph must come in via `pb_nodes`, not a
        // store query — the store never sees it in time otherwise.
        let crate_a = Node::new(
            VName::new("", "", "crates/crate-a/Cargo.toml", "rust", "crate:crate-a"),
            "crate",
        );
        let crate_b = Node::new(
            VName::new("", "", "crates/crate-b/Cargo.toml", "rust", "crate:crate-b"),
            "crate",
        );
        let pb_nodes = vec![crate_a, crate_b];
        // No Depends edge crate-a -> crate-b.
        let pb_edges: Vec<travsr_core::Edge> = Vec::new();

        let caller = Node::new(
            VName::new(
                "",
                "",
                "crates/crate-a/src/store.rs",
                "rust",
                "method:Store.prune",
            ),
            "method",
        );
        store.put_node(&caller).unwrap();

        // The only `fn:get` node anywhere — an unrelated free function in a
        // crate `crate-a` does not depend on.
        let bare_get = Node::new(
            VName::new("", "", "crates/crate-b/src/lib.rs", "rust", "fn:get"),
            "function",
        );
        store.put_node(&bare_get).unwrap();

        let unresolved = vec![travsr_core::UnresolvedCall {
            src: caller.id,
            callee_sig: "fn:get".to_string(),
            alt_callee_sig: None,
            hint_crate: None,
            caller_line: 42,
            is_method_call: true,
            recv_type: None,
        }];

        let (edges, sites) = resolve_unresolved_calls(
            &store,
            &unresolved,
            &pb_nodes,
            &pb_edges,
            &std::collections::HashSet::new(),
        );
        assert!(
            edges.is_empty(),
            "method call must not resolve to an unrelated free function: {edges:?}"
        );
        assert!(sites.is_empty());
    }

    #[test]
    fn test_no_call_edges_across_non_dependent_crates() {
        // #521 F1: a bare (non-method) call with a globally-unique name match
        // must still be rejected when the caller's crate does not depend on
        // the candidate's crate — the crate dependency graph is already in
        // the store (`extract_cargo_deps` writes it before call extraction
        // runs); this is a lookup, not new data.
        use travsr_core::{Node, VName};

        let mut store = SqliteStore::open_in_memory().unwrap();

        let crate_a = Node::new(
            VName::new("", "", "crates/crate-a/Cargo.toml", "rust", "crate:crate-a"),
            "crate",
        );
        let crate_c = Node::new(
            VName::new("", "", "crates/crate-c/Cargo.toml", "rust", "crate:crate-c"),
            "crate",
        );
        let pb_nodes = vec![crate_a, crate_c];
        // No Depends edge crate-a -> crate-c.
        let pb_edges: Vec<travsr_core::Edge> = Vec::new();

        let caller = Node::new(
            VName::new("", "", "crates/crate-a/src/lib.rs", "rust", "fn:caller"),
            "function",
        );
        store.put_node(&caller).unwrap();

        let callee = Node::new(
            VName::new("", "", "crates/crate-c/src/lib.rs", "rust", "fn:helper"),
            "function",
        );
        store.put_node(&callee).unwrap();

        let unresolved = vec![travsr_core::UnresolvedCall {
            src: caller.id,
            callee_sig: "fn:helper".to_string(),
            alt_callee_sig: None,
            hint_crate: None,
            caller_line: 7,
            is_method_call: false,
            recv_type: None,
        }];

        let (edges, _sites) = resolve_unresolved_calls(
            &store,
            &unresolved,
            &pb_nodes,
            &pb_edges,
            &std::collections::HashSet::new(),
        );
        assert!(
            edges.is_empty(),
            "bare call must not cross into a non-dependency crate: {edges:?}"
        );
    }

    #[test]
    fn test_call_edges_allowed_within_dependent_crates() {
        // Sanity check alongside the two rejection tests above: a bare call
        // whose target crate IS a dependency of the caller's crate must
        // still resolve — #521's fixes must not turn into a blanket
        // cross-crate ban.
        use travsr_core::{Node, VName};

        let mut store = SqliteStore::open_in_memory().unwrap();

        let crate_a = Node::new(
            VName::new("", "", "crates/crate-a/Cargo.toml", "rust", "crate:crate-a"),
            "crate",
        );
        let crate_b = Node::new(
            VName::new("", "", "crates/crate-b/Cargo.toml", "rust", "crate:crate-b"),
            "crate",
        );
        let pb_edges = vec![travsr_core::Edge::new(
            crate_a.id,
            crate_b.id,
            travsr_core::EdgeKind::Depends,
        )];
        let pb_nodes = vec![crate_a, crate_b];

        let caller = Node::new(
            VName::new("", "", "crates/crate-a/src/lib.rs", "rust", "fn:caller"),
            "function",
        );
        store.put_node(&caller).unwrap();

        let callee = Node::new(
            VName::new("", "", "crates/crate-b/src/lib.rs", "rust", "fn:helper"),
            "function",
        );
        store.put_node(&callee).unwrap();

        let unresolved = vec![travsr_core::UnresolvedCall {
            src: caller.id,
            callee_sig: "fn:helper".to_string(),
            alt_callee_sig: None,
            hint_crate: None,
            caller_line: 3,
            is_method_call: false,
            recv_type: None,
        }];

        let (edges, sites) = resolve_unresolved_calls(
            &store,
            &unresolved,
            &pb_nodes,
            &pb_edges,
            &std::collections::HashSet::new(),
        );
        assert_eq!(
            edges.len(),
            1,
            "dependency-crate call should still resolve: {edges:?}"
        );
        assert_eq!(edges[0].src, caller.id);
        assert_eq!(edges[0].dst, callee.id);
        assert_eq!(sites, vec![(caller.id, callee.id, 3)]);
    }

    #[test]
    fn python_phase_b_records_class_and_method_occurrences() {
        // #709 (Bug 1 + Bug 2 regression guard): native Python Phase B must
        // record occurrence sites for BOTH a method call (`resp.render()`) and
        // a class/constructor reference (`HttpResponse(...)`) with zero external
        // tools, i.e. without the optional travsr-lsif-py emitter. Before the
        // #709 fix the constructor was emitted as `fn:HttpResponse`, never
        // resolved to the `class:HttpResponse` node, and every class reference
        // vanished on any machine without Node.js.
        use travsr_core::{Node, VName};

        let dir = std::env::temp_dir().join(format!("e2e_py709_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("http.py"),
            b"class HttpResponse:\n    def render(self):\n        return self\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("views.py"),
            b"def index(request):\n    resp = HttpResponse(\"hi\")\n    return resp.render()\n",
        )
        .unwrap();

        // Phase A nodes the resolver looks references up against. VNames must
        // match exactly what the extractor derives (corpus "c", python) so the
        // caller/target node ids line up.
        let mut store = SqliteStore::open_in_memory().unwrap();
        let class_node = Node::new(
            VName::new("c", "", "http.py", "python", "class:HttpResponse"),
            "class",
        );
        let render_node = Node::new(
            VName::new("c", "", "http.py", "python", "method:HttpResponse.render"),
            "method",
        );
        let index_node = Node::new(
            VName::new("c", "", "views.py", "python", "fn:index"),
            "function",
        );
        store.put_node(&class_node).unwrap();
        store.put_node(&render_node).unwrap();
        store.put_node(&index_node).unwrap();

        // Real extractor over the on-disk fixture, no hand-rolled shortcut.
        let (pb_nodes, pb_edges, unresolved) =
            travsr_indexer::phase_b_native_python("c", &dir, None).unwrap();

        let (edges, sites) = resolve_unresolved_calls(
            &store,
            &unresolved,
            &pb_nodes,
            &pb_edges,
            &std::collections::HashSet::new(),
        );

        // Class reference: `HttpResponse(...)` in views.py → class:HttpResponse.
        assert!(
            edges
                .iter()
                .any(|e| e.src == index_node.id && e.dst == class_node.id),
            "constructor call must resolve to class:HttpResponse: {edges:?}"
        );
        assert!(
            sites
                .iter()
                .any(|(s, d, _)| *s == index_node.id && *d == class_node.id),
            "constructor call must record an occurrence site for find_references: {sites:?}"
        );
        // Method reference: `resp.render()` → method:HttpResponse.render.
        assert!(
            sites
                .iter()
                .any(|(s, d, _)| *s == index_node.id && *d == render_node.id),
            "method call must record an occurrence site: {sites:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_no_call_edges_into_tests_from_src() {
        // #521 F2: a `src/` caller resolving onto a `tests/` target must be
        // rejected. Integration tests are a separate compilation unit —
        // Cargo never lists them as a `[dependencies]` entry, so F1's
        // crate-reachability check alone would not catch this (same package,
        // no Depends edge needed either way).
        use travsr_core::{Node, VName};

        let mut store = SqliteStore::open_in_memory().unwrap();

        let caller = Node::new(
            VName::new("", "", "crates/crate-a/src/lib.rs", "rust", "fn:caller"),
            "function",
        );
        store.put_node(&caller).unwrap();

        // Only candidate for the leaf lookup lives under tests/.
        let callee = Node::new(
            VName::new("", "", "crates/crate-a/tests/fixture.rs", "rust", "fn:run"),
            "function",
        );
        store.put_node(&callee).unwrap();

        let unresolved = vec![travsr_core::UnresolvedCall {
            src: caller.id,
            callee_sig: "fn:run".to_string(),
            alt_callee_sig: None,
            hint_crate: None,
            caller_line: 9,
            is_method_call: false,
            recv_type: None,
        }];

        let (edges, _sites) = resolve_unresolved_calls(
            &store,
            &unresolved,
            &[],
            &[],
            &std::collections::HashSet::new(),
        );
        assert!(
            edges.is_empty(),
            "src/ caller must not resolve into tests/: {edges:?}"
        );
    }

    // ── #529: receiver-type resolution (docs/plans/issue-529-method-call-
    // receiver-resolution.md §6.2) ───────────────────────────────────────

    #[test]
    fn t5_recv_type_exact_match_resolves_precisely() {
        use travsr_core::{Node, VName};
        let mut store = SqliteStore::open_in_memory().unwrap();

        let caller = Node::new(
            VName::new("", "", "crates/crate-a/src/lib.rs", "rust", "fn:caller"),
            "function",
        );
        store.put_node(&caller).unwrap();

        let callee = Node::new(
            VName::new(
                "",
                "",
                "crates/crate-a/src/session.rs",
                "rust",
                "method:Session.filter",
            ),
            "method",
        );
        store.put_node(&callee).unwrap();

        let unresolved = vec![travsr_core::UnresolvedCall {
            src: caller.id,
            callee_sig: "fn:filter".to_string(),
            alt_callee_sig: None,
            hint_crate: None,
            caller_line: 5,
            is_method_call: true,
            recv_type: Some("Session".to_string()),
        }];

        let (edges, sites) = resolve_unresolved_calls(
            &store,
            &unresolved,
            &[],
            &[],
            &std::collections::HashSet::new(),
        );
        assert_eq!(
            edges.len(),
            1,
            "recv_type exact match should resolve: {edges:?}"
        );
        assert_eq!(edges[0].dst, callee.id);
        assert_eq!(sites, vec![(caller.id, callee.id, 5)]);
    }

    #[test]
    fn t6_recv_type_not_a_graph_type_resolves_to_zero_edges() {
        // The literal #529 repro: a `.filter()` call on a std-typed receiver
        // (HashSet) must not fall back into the unique-leaf pool and collide
        // with an unrelated `Session.filter` — this is the test that fails
        // on pre-#529 master.
        use travsr_core::{Node, VName};
        let mut store = SqliteStore::open_in_memory().unwrap();

        let caller = Node::new(
            VName::new("", "", "crates/crate-a/src/lib.rs", "rust", "fn:caller"),
            "function",
        );
        store.put_node(&caller).unwrap();

        // The only `*.filter` node anywhere — same shape as real Session::filter.
        let callee = Node::new(
            VName::new(
                "",
                "",
                "crates/crate-b/src/session.rs",
                "rust",
                "method:Session.filter",
            ),
            "method",
        );
        store.put_node(&callee).unwrap();

        let unresolved = vec![travsr_core::UnresolvedCall {
            src: caller.id,
            callee_sig: "fn:filter".to_string(),
            alt_callee_sig: None,
            hint_crate: None,
            caller_line: 12,
            is_method_call: true,
            recv_type: Some("HashSet".to_string()),
        }];

        let (edges, sites) = resolve_unresolved_calls(
            &store,
            &unresolved,
            &[],
            &[],
            &std::collections::HashSet::new(),
        );
        assert!(
            edges.is_empty(),
            "HashSet.filter must not resolve to Session.filter: {edges:?}"
        );
        assert!(sites.is_empty());
    }

    #[test]
    fn t7_recv_type_is_a_graph_type_without_this_method_fails_closed_606() {
        // #606: `T` IS a real type in the graph but doesn't have `.filter`.
        // Pre-#606 this fell through to unique-leaf resolution to recover
        // trait-provided / `Deref` methods — measured 1 false : 0 real
        // against rust-analyzer LSIF on a full native index of this repo,
        // so it now fails closed like the other #529/#604 branches.
        //
        // This inverts the pre-#606 `t7_..._falls_through` assertion (which
        // required exactly this edge); that behavior was the residual bug:
        // `Session` having no `.filter` node means the call dispatches to a
        // trait/std method, and the lone same-leaf `Other.filter` is an
        // unrelated type's method, not the target.
        use travsr_core::{Node, VName};
        let mut store = SqliteStore::open_in_memory().unwrap();

        let caller = Node::new(
            VName::new("", "", "crates/crate-a/src/lib.rs", "rust", "fn:caller"),
            "function",
        );
        store.put_node(&caller).unwrap();

        // "Session" exists as a real type...
        let session_type = Node::new(
            VName::new(
                "",
                "",
                "crates/crate-a/src/session.rs",
                "rust",
                "struct:Session",
            ),
            "struct",
        );
        store.put_node(&session_type).unwrap();

        // ...but the only `*.filter` method belongs to a different type.
        let callee = Node::new(
            VName::new(
                "",
                "",
                "crates/crate-a/src/other.rs",
                "rust",
                "method:Other.filter",
            ),
            "method",
        );
        store.put_node(&callee).unwrap();

        let unresolved = vec![travsr_core::UnresolvedCall {
            src: caller.id,
            callee_sig: "fn:filter".to_string(),
            alt_callee_sig: None,
            hint_crate: None,
            caller_line: 7,
            is_method_call: true,
            recv_type: Some("Session".to_string()),
        }];

        let (edges, sites) = resolve_unresolved_calls(
            &store,
            &unresolved,
            &[],
            &[],
            &std::collections::HashSet::new(),
        );
        assert!(
            edges.is_empty(),
            "graph-type receiver without the method must not resolve by unique-leaf guess (#606): {edges:?}"
        );
        assert!(sites.is_empty());
    }

    #[test]
    fn t17_recv_type_collides_with_unrelated_graph_type_fails_closed_606() {
        // #606: the exact fabrication the measurement caught on this repo.
        // A call `cmd.stdin(..)` on `std::process::Command` recovers
        // `recv_type: Some("Command")`, which collides with an unrelated
        // user type of the same name (`enum Command` — the CLI's clap enum)
        // in a third crate, defeating the #529 branch-(2) external-type
        // gate. With no `method:Command.stdin` node, the pre-#606 fall-
        // through resolved it to the lone same-leaf `SandboxedSpawn.stdin`
        // in a crate the caller depends on — a fabricated cross-crate
        // ref/call edge. Must emit nothing.
        use travsr_core::{Node, VName};
        let mut store = SqliteStore::open_in_memory().unwrap();

        let caller = Node::new(
            VName::new("", "", "crates/crate-a/src/tools.rs", "rust", "fn:caller"),
            "function",
        );
        store.put_node(&caller).unwrap();

        // The colliding user type lives in an unrelated crate and is an
        // enum, exercising the `enum:` arm of the graph-type probe.
        let clap_command = Node::new(
            VName::new("", "", "crates/crate-c/src/main.rs", "rust", "enum:Command"),
            "enum",
        );
        store.put_node(&clap_command).unwrap();

        // The only `.stdin` method in the graph, on an unrelated type.
        let callee = Node::new(
            VName::new(
                "",
                "",
                "crates/crate-b/src/sandbox.rs",
                "rust",
                "method:SandboxedSpawn.stdin",
            ),
            "method",
        );
        store.put_node(&callee).unwrap();

        let unresolved = vec![travsr_core::UnresolvedCall {
            src: caller.id,
            callee_sig: "fn:stdin".to_string(),
            alt_callee_sig: None,
            hint_crate: None,
            caller_line: 9773,
            is_method_call: true,
            recv_type: Some("Command".to_string()),
        }];

        let (edges, sites) = resolve_unresolved_calls(
            &store,
            &unresolved,
            &[],
            &[],
            &std::collections::HashSet::new(),
        );
        assert!(
            edges.is_empty(),
            "std-type receiver colliding with a user type name must not fabricate an edge (#606): {edges:?}"
        );
        assert!(sites.is_empty());
    }

    #[test]
    fn t8_recv_type_exact_match_still_respects_crate_reachability() {
        // Invariant 4: an exactly-resolved `fn:T.method` in a crate the
        // caller cannot reach must still be rejected — the #529 fast path
        // does not bypass #521's call_target_reachable gate.
        use travsr_core::{Node, VName};
        let mut store = SqliteStore::open_in_memory().unwrap();

        let crate_a = Node::new(
            VName::new("", "", "crates/crate-a/Cargo.toml", "rust", "crate:crate-a"),
            "crate",
        );
        let crate_c = Node::new(
            VName::new("", "", "crates/crate-c/Cargo.toml", "rust", "crate:crate-c"),
            "crate",
        );
        let pb_nodes = vec![crate_a, crate_c];
        let pb_edges: Vec<travsr_core::Edge> = Vec::new(); // no Depends edge

        let caller = Node::new(
            VName::new("", "", "crates/crate-a/src/lib.rs", "rust", "fn:caller"),
            "function",
        );
        store.put_node(&caller).unwrap();

        let callee = Node::new(
            VName::new(
                "",
                "",
                "crates/crate-c/src/session.rs",
                "rust",
                "method:Session.filter",
            ),
            "method",
        );
        store.put_node(&callee).unwrap();

        let unresolved = vec![travsr_core::UnresolvedCall {
            src: caller.id,
            callee_sig: "fn:filter".to_string(),
            alt_callee_sig: None,
            hint_crate: None,
            caller_line: 3,
            is_method_call: true,
            recv_type: Some("Session".to_string()),
        }];

        let (edges, _sites) = resolve_unresolved_calls(
            &store,
            &unresolved,
            &pb_nodes,
            &pb_edges,
            &std::collections::HashSet::new(),
        );
        assert!(
            edges.is_empty(),
            "exact recv_type match must still respect crate reachability: {edges:?}"
        );
    }

    #[test]
    fn t9_recv_type_none_unique_leaf_is_fail_closed_604() {
        // #604: a method call whose receiver type could not be recovered must
        // NOT resolve by leaf-name uniqueness. This is the exact scenario the
        // native heuristic fabricated on: `something.filter()` on an unknown
        // receiver (a `Vec`, an `Iterator` — std types absent from the graph),
        // with a lone `method:Session.filter` as the only same-named user
        // method. "Unique in-graph" is not evidence of the target; measured
        // 99% false against rust-analyzer LSIF. Fail closed — emit nothing.
        //
        // This inverts the pre-#604 `t9_..._byte_identical_to_pre_529_behavior`
        // assertion (which required 1 edge here); that behavior was the bug.
        // `Session.filter` was the single largest offender in the #604
        // measurement (98 fabricated edges on this repo alone).
        use travsr_core::{Node, VName};
        let mut store = SqliteStore::open_in_memory().unwrap();

        let caller = Node::new(
            VName::new("", "", "crates/crate-a/src/lib.rs", "rust", "fn:caller"),
            "function",
        );
        store.put_node(&caller).unwrap();

        let callee = Node::new(
            VName::new(
                "",
                "",
                "crates/crate-a/src/session.rs",
                "rust",
                "method:Session.filter",
            ),
            "method",
        );
        store.put_node(&callee).unwrap();

        let unresolved = vec![travsr_core::UnresolvedCall {
            src: caller.id,
            callee_sig: "fn:filter".to_string(),
            alt_callee_sig: None,
            hint_crate: None,
            caller_line: 8,
            is_method_call: true,
            recv_type: None,
        }];

        let (edges, sites) = resolve_unresolved_calls(
            &store,
            &unresolved,
            &[],
            &[],
            &std::collections::HashSet::new(),
        );
        assert!(
            edges.is_empty(),
            "receiver-less method call must not resolve by unique-leaf guess (#604): {edges:?}"
        );
        assert!(sites.is_empty());
    }

    #[test]
    fn t14_bare_function_call_does_not_resolve_into_a_method_604() {
        // #604 non-method sibling: a bare free/scoped-function call must never
        // resolve into a qualified `Type.method` node by leaf uniqueness. The
        // real-world trigger is `writeln!(std::io::stderr(), ...)`, extracted as
        // a bare `fn:stderr` call (is_method_call = false); with a lone
        // `method:SandboxedSpawn.stderr` in the graph the unfiltered leaf pool
        // matched it and fabricated a `ref/call` into that method.
        use travsr_core::{Node, VName};
        let mut store = SqliteStore::open_in_memory().unwrap();

        let caller = Node::new(
            VName::new(
                "",
                "",
                "crates/crate-a/src/progress.rs",
                "rust",
                "fn:render",
            ),
            "function",
        );
        store.put_node(&caller).unwrap();

        // The only `stderr`-leaf node in the graph is a qualified method.
        let method = Node::new(
            VName::new(
                "",
                "",
                "crates/crate-a/src/sandbox.rs",
                "rust",
                "method:SandboxedSpawn.stderr",
            ),
            "method",
        );
        store.put_node(&method).unwrap();

        // `std::io::stderr()` — a bare (non-method) function call.
        let unresolved = vec![travsr_core::UnresolvedCall {
            src: caller.id,
            callee_sig: "fn:stderr".to_string(),
            alt_callee_sig: None,
            hint_crate: None,
            caller_line: 12,
            is_method_call: false,
            recv_type: None,
        }];

        let (edges, sites) = resolve_unresolved_calls(
            &store,
            &unresolved,
            &[],
            &[],
            &std::collections::HashSet::new(),
        );
        assert!(
            edges.is_empty(),
            "bare function call must not resolve into a qualified method (#604): {edges:?}"
        );
        assert!(sites.is_empty());
    }

    #[test]
    fn t15_associated_call_to_external_type_does_not_resolve_by_leaf_604() {
        // #604 associated-call path: `rusqlite::Connection::open(..)` extracts as
        // a non-method call with a qualified `callee_sig` (`method:Connection.open`).
        // `Connection` is external (no graph node), so the exact `by_sig` lookup
        // misses; the leaf fallback must NOT then match a lone same-leaf user
        // method (`method:SqliteStore.open`). The named type is evidence: a call
        // to `Connection::open` can only target `Connection.open`, never another
        // type's `open`. Fail closed.
        use travsr_core::{Node, VName};
        let mut store = SqliteStore::open_in_memory().unwrap();

        let caller = Node::new(
            VName::new("", "", "crates/crate-a/src/embed.rs", "rust", "fn:write_db"),
            "function",
        );
        store.put_node(&caller).unwrap();

        // Only qualified `.open` in the graph belongs to a DIFFERENT type.
        let other = Node::new(
            VName::new(
                "",
                "",
                "crates/crate-a/src/store.rs",
                "rust",
                "method:SqliteStore.open",
            ),
            "method",
        );
        store.put_node(&other).unwrap();

        let unresolved = vec![travsr_core::UnresolvedCall {
            src: caller.id,
            callee_sig: "method:Connection.open".to_string(),
            alt_callee_sig: None,
            hint_crate: None,
            caller_line: 45,
            is_method_call: false,
            recv_type: None,
        }];

        let (edges, sites) = resolve_unresolved_calls(
            &store,
            &unresolved,
            &[],
            &[],
            &std::collections::HashSet::new(),
        );
        assert!(
            edges.is_empty(),
            "associated call to an external type must not resolve to a same-leaf \
             user method (#604): {edges:?}"
        );
        assert!(sites.is_empty());
    }

    #[test]
    fn t16_associated_call_to_graph_type_still_resolves_604() {
        // #604 guard: the associated-call fix must not drop a real one. When the
        // named type IS in the graph, `Type::method` resolves via the exact
        // `by_sig` match — recall preserved.
        use travsr_core::{Node, VName};
        let mut store = SqliteStore::open_in_memory().unwrap();

        let caller = Node::new(
            VName::new("", "", "crates/crate-a/src/main.rs", "rust", "fn:run"),
            "function",
        );
        store.put_node(&caller).unwrap();

        let callee = Node::new(
            VName::new(
                "",
                "",
                "crates/crate-a/src/store.rs",
                "rust",
                "method:SqliteStore.open",
            ),
            "method",
        );
        store.put_node(&callee).unwrap();

        let unresolved = vec![travsr_core::UnresolvedCall {
            src: caller.id,
            callee_sig: "method:SqliteStore.open".to_string(),
            alt_callee_sig: None,
            hint_crate: None,
            caller_line: 7,
            is_method_call: false,
            recv_type: None,
        }];

        let (edges, _sites) = resolve_unresolved_calls(
            &store,
            &unresolved,
            &[],
            &[],
            &std::collections::HashSet::new(),
        );
        assert_eq!(
            edges.len(),
            1,
            "associated call to a graph type must still resolve exactly: {edges:?}"
        );
        assert_eq!(edges[0].dst, callee.id);
    }

    #[test]
    fn t13_recv_type_some_exact_match_still_resolves_after_604() {
        // #604 guard: fail-closing the receiver-less path must not touch the
        // #529 recovered-receiver path. A call whose receiver type IS known and
        // exactly matches a `method:T.method` node must still resolve — this is
        // the recall the whole design keeps. Same `.filter` leaf as t9, but here
        // the receiver is known to be `Session`, so it is real evidence.
        use travsr_core::{Node, VName};
        let mut store = SqliteStore::open_in_memory().unwrap();

        let caller = Node::new(
            VName::new("", "", "crates/crate-a/src/lib.rs", "rust", "fn:caller"),
            "function",
        );
        store.put_node(&caller).unwrap();

        let callee = Node::new(
            VName::new(
                "",
                "",
                "crates/crate-a/src/session.rs",
                "rust",
                "method:Session.filter",
            ),
            "method",
        );
        store.put_node(&callee).unwrap();

        let unresolved = vec![travsr_core::UnresolvedCall {
            src: caller.id,
            callee_sig: "fn:filter".to_string(),
            alt_callee_sig: None,
            hint_crate: None,
            caller_line: 8,
            is_method_call: true,
            recv_type: Some("Session".to_string()),
        }];

        let (edges, _sites) = resolve_unresolved_calls(
            &store,
            &unresolved,
            &[],
            &[],
            &std::collections::HashSet::new(),
        );
        assert_eq!(
            edges.len(),
            1,
            "recovered receiver type must still resolve exactly (#529 path intact): {edges:?}"
        );
        assert_eq!(edges[0].dst, callee.id);
    }

    #[test]
    fn t10_ambiguous_leaf_with_recv_type_resolves_where_master_drops() {
        // §4.4 recall win: two types share the same method leaf, which
        // makes today's CO-A1 uniqueness gate drop the call entirely. A
        // known recv_type disambiguates it directly.
        use travsr_core::{Node, VName};
        let mut store = SqliteStore::open_in_memory().unwrap();

        let caller = Node::new(
            VName::new("", "", "crates/crate-a/src/lib.rs", "rust", "fn:caller"),
            "function",
        );
        store.put_node(&caller).unwrap();

        let callee_a = Node::new(
            VName::new(
                "",
                "",
                "crates/crate-a/src/store.rs",
                "rust",
                "method:SqliteStore.get_nodes",
            ),
            "method",
        );
        store.put_node(&callee_a).unwrap();
        let callee_b = Node::new(
            VName::new(
                "",
                "",
                "crates/crate-a/src/other.rs",
                "rust",
                "method:OtherStore.get_nodes",
            ),
            "method",
        );
        store.put_node(&callee_b).unwrap();

        let unresolved = vec![travsr_core::UnresolvedCall {
            src: caller.id,
            callee_sig: "fn:get_nodes".to_string(),
            alt_callee_sig: None,
            hint_crate: None,
            caller_line: 4,
            is_method_call: true,
            recv_type: Some("SqliteStore".to_string()),
        }];

        let (edges, _sites) = resolve_unresolved_calls(
            &store,
            &unresolved,
            &[],
            &[],
            &std::collections::HashSet::new(),
        );
        assert_eq!(
            edges.len(),
            1,
            "recv_type should disambiguate a leaf that is globally ambiguous: {edges:?}"
        );
        assert_eq!(edges[0].dst, callee_a.id);

        // Sanity: without recv_type, master's ambiguity gate drops this same
        // call site entirely (0 edges) — the recall win is real, not a no-op.
        let unresolved_no_recv = vec![travsr_core::UnresolvedCall {
            recv_type: None,
            ..unresolved[0].clone()
        }];
        let (edges2, _sites2) = resolve_unresolved_calls(
            &store,
            &unresolved_no_recv,
            &[],
            &[],
            &std::collections::HashSet::new(),
        );
        assert!(
            edges2.is_empty(),
            "ambiguous leaf without recv_type must still be dropped: {edges2:?}"
        );
    }

    #[test]
    fn t11_e7_lsif_covered_call_site_suppresses_native_heuristic() {
        // E7: where E3's positional rust-analyzer LSIF already resolved a call
        // site, its precise `scip` edge supersedes the native leaf-guess
        // heuristic. A call that WOULD resolve by unique-leaf must be suppressed
        // when its (caller_path, caller_line) is in the covered set, and must
        // still resolve at any line the LSIF path did not cover (the residual).
        use travsr_core::{Node, VName};
        let mut store = SqliteStore::open_in_memory().unwrap();

        let caller = Node::new(
            VName::new("", "", "crates/crate-a/src/lib.rs", "rust", "fn:caller"),
            "function",
        );
        store.put_node(&caller).unwrap();

        let callee = Node::new(
            VName::new(
                "",
                "",
                "crates/crate-a/src/session.rs",
                "rust",
                "method:Session.filter",
            ),
            "method",
        );
        store.put_node(&callee).unwrap();

        // #604: the residual resolves via the recovered-receiver (#529) path,
        // not the unique-leaf guess (which now fails closed). E7 suppression is
        // orthogonal to how the uncovered site resolves — a known receiver is
        // the evidence that lets it resolve at all so the suppression assertion
        // stays meaningful.
        let call_at = |line: u32| travsr_core::UnresolvedCall {
            src: caller.id,
            callee_sig: "fn:filter".to_string(),
            alt_callee_sig: None,
            hint_crate: None,
            caller_line: line,
            is_method_call: true,
            recv_type: Some("Session".to_string()),
        };

        // The LSIF positional path covered the `filter` call on line 8 of the
        // caller's file, but not the one on line 12.
        let mut lsif_covered: std::collections::HashSet<(String, u32, String)> =
            std::collections::HashSet::new();
        lsif_covered.insert((
            "crates/crate-a/src/lib.rs".to_string(),
            8,
            "filter".to_string(),
        ));

        // Covered site → native heuristic must defer (0 edges) even though the
        // recovered-receiver match would otherwise fire on this line.
        let (edges_covered, _s) =
            resolve_unresolved_calls(&store, &[call_at(8)], &[], &[], &lsif_covered);
        assert!(
            edges_covered.is_empty(),
            "E7: LSIF-covered call site must suppress the native edge: {edges_covered:?}"
        );

        // Uncovered site → native heuristic still resolves (the residual E7 keeps).
        let (edges_residual, _s) =
            resolve_unresolved_calls(&store, &[call_at(12)], &[], &[], &lsif_covered);
        assert_eq!(
            edges_residual.len(),
            1,
            "E7: an uncovered call site must still resolve natively: {edges_residual:?}"
        );
        assert_eq!(edges_residual[0].dst, callee.id);
    }

    #[test]
    fn t12_e7_suppression_is_per_callee_not_per_line() {
        // I2: two calls on the same physical line, only one of which LSIF
        // positionally resolved. Suppression keyed on (path, line) alone would
        // drop both; keyed on (path, line, callee_leaf) it must drop only the
        // LSIF-covered callee and still resolve the other one natively.
        use travsr_core::{Node, VName};
        let mut store = SqliteStore::open_in_memory().unwrap();

        let caller = Node::new(
            VName::new("", "", "crates/crate-a/src/lib.rs", "rust", "fn:caller"),
            "function",
        );
        store.put_node(&caller).unwrap();

        let filter_callee = Node::new(
            VName::new(
                "",
                "",
                "crates/crate-a/src/session.rs",
                "rust",
                "method:Session.filter",
            ),
            "method",
        );
        store.put_node(&filter_callee).unwrap();

        let count_callee = Node::new(
            VName::new(
                "",
                "",
                "crates/crate-a/src/session.rs",
                "rust",
                "method:Session.count",
            ),
            "method",
        );
        store.put_node(&count_callee).unwrap();

        // Both calls are on line 8 (e.g. `s.filter(x).count()`), but LSIF only
        // resolved the `filter` callee on that line.
        let mut lsif_covered: std::collections::HashSet<(String, u32, String)> =
            std::collections::HashSet::new();
        lsif_covered.insert((
            "crates/crate-a/src/lib.rs".to_string(),
            8,
            "filter".to_string(),
        ));

        // #604: both carry a recovered receiver so the uncovered `count` can
        // resolve via the #529 exact-match path (the unique-leaf guess now
        // fails closed). The point under test is per-callee suppression, not
        // how the survivor resolves.
        let calls = vec![
            travsr_core::UnresolvedCall {
                src: caller.id,
                callee_sig: "fn:filter".to_string(),
                alt_callee_sig: None,
                hint_crate: None,
                caller_line: 8,
                is_method_call: true,
                recv_type: Some("Session".to_string()),
            },
            travsr_core::UnresolvedCall {
                src: caller.id,
                callee_sig: "fn:count".to_string(),
                alt_callee_sig: None,
                hint_crate: None,
                caller_line: 8,
                is_method_call: true,
                recv_type: Some("Session".to_string()),
            },
        ];

        let (edges, _sites) = resolve_unresolved_calls(&store, &calls, &[], &[], &lsif_covered);
        assert_eq!(
            edges.len(),
            1,
            "only the LSIF-covered callee (filter) must be suppressed: {edges:?}"
        );
        assert_eq!(
            edges[0].dst, count_callee.id,
            "the uncovered callee (count) sharing the same line must still resolve natively"
        );
    }

    #[test]
    fn e4_empty_caller_language_falls_through_to_unfiltered_matches() {
        // #I3: a caller node with an empty `vname.language` (should not occur
        // in practice — see the guard's comment at the E4 filter site) must
        // not have every candidate filtered to nothing by the language scope.
        // It must fall through to pre-E4 behavior and still resolve.
        use travsr_core::{Node, VName};
        let mut store = SqliteStore::open_in_memory().unwrap();

        let caller = Node::new(
            VName::new("", "", "src/caller.rs", "", "fn:run"),
            "function",
        );
        store.put_node(&caller).unwrap();

        let callee = Node::new(
            VName::new("", "", "src/helper.rs", "rust", "fn:helper"),
            "function",
        );
        store.put_node(&callee).unwrap();

        let unresolved = vec![travsr_core::UnresolvedCall {
            src: caller.id,
            callee_sig: "fn:helper".to_string(),
            alt_callee_sig: None,
            hint_crate: None,
            caller_line: 4,
            is_method_call: false,
            recv_type: None,
        }];

        let (edges, _sites) = resolve_unresolved_calls(
            &store,
            &unresolved,
            &[],
            &[],
            &std::collections::HashSet::new(),
        );
        assert_eq!(
            edges.len(),
            1,
            "empty caller_lang must not filter out the only candidate: {edges:?}"
        );
        assert_eq!(edges[0].dst, callee.id);
    }

    #[test]
    fn e4_class_receiver_is_a_graph_type_without_method_fails_closed_606() {
        // #606: a Python/TS receiver whose type is a real `class:T` in the
        // graph but whose `method:T.leaf` node does not exist (inherited
        // method) no longer falls through to unique-leaf resolution — the
        // language-general analog of the inverted t7. A same-leaf method on
        // an unrelated class is not evidence of inheritance; the branch-(3)
        // measurement (see the resolver) found the fall-through fabricates
        // whenever a receiver's type name collides with an in-graph type.
        // Genuine inherited-method sites are covered by the scip/LSIF path
        // (E7); the native heuristic emits nothing here.
        use travsr_core::{Node, VName};
        let mut store = SqliteStore::open_in_memory().unwrap();

        let caller = Node::new(
            VName::new("", "", "src/app.ts", "typescript", "fn:run"),
            "function",
        );
        store.put_node(&caller).unwrap();

        // `App` is a real class in the graph...
        let app = Node::new(
            VName::new("", "", "src/app.ts", "typescript", "class:App"),
            "class",
        );
        store.put_node(&app).unwrap();

        // ...but the only `*.helper` method belongs to a different class,
        // cross-file (inherited/mixed-in — the extractor can't see the link).
        let callee = Node::new(
            VName::new("", "", "src/base.ts", "typescript", "method:Base.helper"),
            "method",
        );
        store.put_node(&callee).unwrap();

        let unresolved = vec![travsr_core::UnresolvedCall {
            src: caller.id,
            callee_sig: "fn:helper".to_string(),
            alt_callee_sig: None,
            hint_crate: None,
            caller_line: 4,
            is_method_call: true,
            recv_type: Some("App".to_string()),
        }];

        let (edges, sites) = resolve_unresolved_calls(
            &store,
            &unresolved,
            &[],
            &[],
            &std::collections::HashSet::new(),
        );
        assert!(
            edges.is_empty(),
            "class receiver without the method must not resolve by unique-leaf guess (#606): {edges:?}"
        );
        assert!(sites.is_empty());
    }

    #[test]
    fn e4_chain_receiver_does_not_fabricate_false_self_class_edge() {
        // E4 acceptance: `this.other.run()` where the enclosing class ALSO
        // defines `run`. The receiver is an unrecoverable chain (recv_type =
        // None); with two `method:*.run` in the graph the leaf is ambiguous, so
        // NOTHING is emitted — no false self-class edge (the exact defect the
        // old in-extractor `method:{enclosing_class}.{leaf}` guess produced).
        use travsr_core::{Node, VName};
        let mut store = SqliteStore::open_in_memory().unwrap();

        let caller = Node::new(
            VName::new("", "", "src/app.ts", "typescript", "method:App.render"),
            "method",
        );
        store.put_node(&caller).unwrap();

        // Two distinct `run` methods → ambiguous leaf.
        for (path, sig) in [
            ("src/app.ts", "method:App.run"),
            ("src/other.ts", "method:Other.run"),
        ] {
            store
                .put_node(&Node::new(
                    VName::new("", "", path, "typescript", sig),
                    "method",
                ))
                .unwrap();
        }

        let unresolved = vec![travsr_core::UnresolvedCall {
            src: caller.id,
            callee_sig: "fn:run".to_string(),
            alt_callee_sig: None,
            hint_crate: None,
            caller_line: 5,
            is_method_call: true,
            recv_type: None, // chain receiver — unrecoverable
        }];

        let (edges, _sites) = resolve_unresolved_calls(
            &store,
            &unresolved,
            &[],
            &[],
            &std::collections::HashSet::new(),
        );
        assert!(
            edges.is_empty(),
            "ambiguous chain-receiver call must emit no edge (no false self-class): {edges:?}"
        );
    }

    #[test]
    fn e4_call_is_scoped_to_caller_language() {
        // E4: a call must resolve only within the caller's own language. A
        // Python `helper()` must NOT resolve to a Rust `fn:helper` even when
        // that Rust node is the unique bearer of the leaf — cross-language edges
        // are false. Adding a Python `fn:helper` then makes it resolve.
        use travsr_core::{Node, VName};
        let mut store = SqliteStore::open_in_memory().unwrap();

        let caller = Node::new(VName::new("", "", "app.py", "python", "fn:run"), "function");
        store.put_node(&caller).unwrap();
        // `helper` exists ONLY in Rust.
        store
            .put_node(&Node::new(
                VName::new("", "", "lib.rs", "rust", "fn:helper"),
                "function",
            ))
            .unwrap();

        let call = travsr_core::UnresolvedCall {
            src: caller.id,
            callee_sig: "fn:helper".to_string(),
            alt_callee_sig: None,
            hint_crate: None,
            caller_line: 2,
            is_method_call: false,
            recv_type: None,
        };

        let (edges, _s) = resolve_unresolved_calls(
            &store,
            std::slice::from_ref(&call),
            &[],
            &[],
            &std::collections::HashSet::new(),
        );
        assert!(
            edges.is_empty(),
            "python call must not resolve to a rust definition: {edges:?}"
        );

        // Now add a Python `fn:helper` — the same call resolves, within language.
        let py_helper = Node::new(
            VName::new("", "", "util.py", "python", "fn:helper"),
            "function",
        );
        store.put_node(&py_helper).unwrap();
        let (edges2, _s) = resolve_unresolved_calls(
            &store,
            std::slice::from_ref(&call),
            &[],
            &[],
            &std::collections::HashSet::new(),
        );
        assert_eq!(
            edges2.len(),
            1,
            "same-language call must resolve: {edges2:?}"
        );
        assert_eq!(edges2[0].dst, py_helper.id);
    }

    // ── #449 regression: full Phase A + Phase B pipeline for Swift ──────────
    //
    // Phase A nodes are computed for real via travsr_analysis::swift::parse
    // against inline fixture sources (self-contained, no dependency on the
    // sibling travsr-lang repo's fixtures). Phase B definitions/references are
    // transcribed verbatim from a real `swift-index-emitter` run against those
    // same sources, built from the fixed Sources/main.swift (travsr-lang PR for
    // #449). Exercises the true write_phase_b_results -> unify_all ->
    // write_scip_attributed_batch -> find_references pipeline end to end, and
    // pins the exact repro from the issue: `find_references("ClassA")` must
    // show the `ClassA(controller:)` constructor call site, not "0 references".
    #[test]
    fn e2e_449_swift_full_pipeline_resolves_literal_repro() {
        use travsr_core::{Node, ScipRef, VName};

        let corpus = "";
        let fixture_dir = tempfile::tempdir().unwrap();
        let sources: &[(&str, &str)] = &[
            (
                "ClassA.swift",
                "class Controller {\n    var name: String = \"\"\n}\n\n\
                 class ClassA {\n    let controller: Controller\n\n    \
                 init(controller: Controller) {\n        self.controller = controller\n    }\n\
                 \n    func start() {\n        print(controller.name)\n    }\n}\n",
            ),
            (
                "ClassB.swift",
                "class ClassB {\n    func makeA() -> ClassA {\n        \
                 let controller = Controller()\n        \
                 let a = ClassA(controller: controller)\n        a.start()\n        return a\n    }\n}\n",
            ),
            (
                "ClassC.swift",
                "import Foundation\n\n@objc class ClassC: NSObject {\n    \
                 @objc static let shared = ClassC()\n\n    var environments: [String] = []\n\n    \
                 @objc static func registerEnvironments() {\n        \
                 ClassC.shared.environments.append(\"default\")\n    }\n}\n",
            ),
            (
                "Caller.swift",
                "func configure() {\n    let c = ClassC.shared\n    \
                 c.environments.append(\"staging\")\n    ClassC.registerEnvironments()\n}\n",
            ),
        ];
        for (name, src) in sources {
            std::fs::write(fixture_dir.path().join(name), src).unwrap();
        }

        let mut store = SqliteStore::open_in_memory().unwrap();

        // ── Real Phase A (tree-sitter) for every fixture file ───────────────
        for (file, _) in sources {
            let abs = fixture_dir.path().join(file);
            let out = travsr_analysis::swift::parse(corpus, &abs, file).unwrap();
            for n in &out.nodes {
                store.put_node(n).unwrap();
            }
            for e in &out.edges {
                store.put_edge(e).unwrap();
            }
        }

        // ── Real Phase B definitions, transcribed verbatim from an actual
        // swift-index-emitter run against these exact sources ──────────────
        let defs: &[(&str, &str, &str, u32, u32)] = &[
            ("Caller.swift", "swift::configure", "function", 1, 5),
            ("Caller.swift", "swift::c", "variable", 2, 2),
            ("ClassA.swift", "swift::Controller", "class", 1, 3),
            ("ClassA.swift", "swift::Controller.name", "field", 2, 2),
            ("ClassA.swift", "swift::ClassA", "class", 5, 15),
            ("ClassA.swift", "swift::ClassA.controller", "field", 6, 6),
            ("ClassA.swift", "swift::ClassA.init", "constructor", 8, 10),
            ("ClassA.swift", "swift::ClassA.start", "function", 12, 14),
            ("ClassB.swift", "swift::ClassB", "class", 1, 8),
            ("ClassB.swift", "swift::ClassB.makeA", "function", 2, 7),
            ("ClassB.swift", "swift::ClassB.controller", "field", 3, 3),
            ("ClassB.swift", "swift::ClassB.a", "field", 4, 4),
            ("ClassC.swift", "swift::ClassC", "class", 3, 11),
            ("ClassC.swift", "swift::ClassC.shared", "field", 4, 4),
            ("ClassC.swift", "swift::ClassC.environments", "field", 6, 6),
            (
                "ClassC.swift",
                "swift::ClassC.registerEnvironments",
                "function",
                8,
                10,
            ),
        ];
        let mut def_ids: std::collections::HashMap<&str, travsr_core::NodeId> =
            std::collections::HashMap::new();
        let mut pb_nodes: Vec<Node> = Vec::new();
        for (path, sym, kind, line, end_line) in defs {
            let vname = VName::new(corpus, "", *path, "swift", *sym);
            let id = vname.id();
            def_ids.insert(sym, id);
            pb_nodes.push(
                Node::new(vname, *kind)
                    .with_line(*line)
                    .with_end_line(*end_line),
            );
        }

        // References, transcribed verbatim from a real emitter run AFTER the
        // #449 fix: bare `ClassC.shared`/`registerEnvironments` member accesses
        // now exist at all, and constructor calls (`Controller()`, `ClassA(...)`)
        // target the type symbol directly rather than a synthetic `.init`
        // member, so `find_references("ClassA")` sees them without the caller
        // needing to know about `.init`.
        let refs_raw: &[(&str, u32, &str)] = &[
            ("Caller.swift", 2, "swift::ClassC.shared"),
            ("Caller.swift", 4, "swift::ClassC.registerEnvironments"),
            ("ClassA.swift", 9, "swift::ClassA.controller"),
            ("ClassB.swift", 3, "swift::Controller"),
            ("ClassB.swift", 4, "swift::ClassA"),
            ("ClassC.swift", 4, "swift::ClassC"),
            ("ClassC.swift", 9, "swift::ClassC.shared"),
        ];
        let mut pb_refs: Vec<ScipRef> = Vec::new();
        for (path, line, sym) in refs_raw {
            let Some(&callee_id) = def_ids.get(sym) else {
                panic!("ref symbol {sym} has no definition");
            };
            pb_refs.push(ScipRef {
                caller_path: path.to_string(),
                caller_line: *line,
                callee_id,
                is_call: true,
            });
        }

        let (_report, _alias) = write_phase_b_results(
            &mut store,
            corpus,
            pb_nodes,
            Vec::new(),
            pb_refs,
            travsr_plugin_host::PhaseBOutcome::default(),
        );

        // Literal repro from issue #449: "ClassA (Swift class instantiated via
        // ClassA(controller:) from many files) reports 0 references."
        let fr_class_a = travsr_mcp::find_references(&store, "ClassA", None);
        assert!(
            fr_class_a.contains("1 reference(s)") && fr_class_a.contains("ClassB.swift:4"),
            "find_references(\"ClassA\") must show the constructor call site: {fr_class_a}"
        );
        assert!(
            !fr_class_a.contains("0 reference(s)"),
            "must not regress to the reported '0 references' bug: {fr_class_a}"
        );

        // Literal repro: "ClassC.shared (dotted static access) does not even resolve."
        let fr_shared = travsr_mcp::find_references(&store, "ClassC.shared", None);
        assert!(
            fr_shared.contains("2 reference(s)")
                && fr_shared.contains("Caller.swift:2")
                && fr_shared.contains("ClassC.swift:9"),
            "find_references(\"ClassC.shared\") must resolve and list both sites: {fr_shared}"
        );

        // Literal repro: "registerEnvironments (@objc static func) called from
        // Objective-C ... reports 0 references". This covers the Swift-side
        // call; the ObjC bridged call is macOS-only, verified separately by
        // the scip-reader unit tests (objc_ref_without_def_becomes_unresolved_call).
        let fr_register = travsr_mcp::find_references(&store, "registerEnvironments", None);
        assert!(
            fr_register.contains("1 reference(s)") && fr_register.contains("Caller.swift:4"),
            "find_references(\"registerEnvironments\") must resolve: {fr_register}"
        );
    }

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

    /// #583: a reindex that rewrites a file's Phase A nodes also drops that
    /// file's Phase B `ref/call` edges, and on the watcher path HEAD never
    /// moves, so `last_commit` cannot signal it. `reindex_files` must record
    /// the staleness itself, and only when something actually changed.
    #[test]
    fn reindex_files_flags_phase_b_dirty_only_when_content_changed() {
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        let ts_path = tmp.path().join("svc.ts");
        std::fs::write(&ts_path, "export class OldSvc { foo() {} }").unwrap();

        let db_path = tmp.path().join(".travsr/graph.db");
        std::fs::create_dir_all(tmp.path().join(".travsr")).unwrap();
        let mut store = travsr_store::SqliteStore::open(&db_path).unwrap();
        store
            .set_signature_format_version(travsr_core::SIGNATURE_FORMAT_VERSION)
            .unwrap();

        reindex_files(std::slice::from_ref(&ts_path), tmp.path(), &mut store).unwrap();
        assert_eq!(
            store.get_meta("phase_b_dirty").unwrap().as_deref(),
            Some("1"),
            "first index of a file must flag Phase B as stale"
        );

        // A completed Phase B run clears the flag.
        store.set_meta("phase_b_dirty", "0").unwrap();

        // Re-running over unchanged content must not re-flag: the hash guard
        // means nothing was rewritten, so the semantic layer is still valid.
        reindex_files(std::slice::from_ref(&ts_path), tmp.path(), &mut store).unwrap();
        assert_eq!(
            store.get_meta("phase_b_dirty").unwrap().as_deref(),
            Some("0"),
            "an unchanged file must not flag Phase B as stale"
        );

        // Editing it must flag again. This is the #583 path.
        std::fs::write(&ts_path, "export class NewSvc { bar() {} }").unwrap();
        reindex_files(std::slice::from_ref(&ts_path), tmp.path(), &mut store).unwrap();
        assert_eq!(
            store.get_meta("phase_b_dirty").unwrap().as_deref(),
            Some("1"),
            "a changed file must flag Phase B as stale"
        );
    }

    /// #583 review: the flag can be set while the working tree is *clean*.
    ///
    /// The motivating cases (branch switch, `git stash pop`, revert) all end
    /// with the file back at its committed content, so `git status` is clean
    /// and there is nothing to stage — but the reindex in between already
    /// dropped that file's `ref/call` edges, and the flag is still set. That
    /// combination is why `travsr status` names the condition rather than
    /// telling the user to commit: at this point `git commit` reports
    /// "nothing to commit" and the user is stuck.
    #[test]
    fn reindex_files_flags_phase_b_dirty_even_when_the_tree_ends_up_clean() {
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        let ts_path = tmp.path().join("svc.ts");
        let committed = "export class OldSvc { foo() {} }";
        std::fs::write(&ts_path, committed).unwrap();

        let db_path = tmp.path().join(".travsr/graph.db");
        std::fs::create_dir_all(tmp.path().join(".travsr")).unwrap();
        let mut store = travsr_store::SqliteStore::open(&db_path).unwrap();
        store
            .set_signature_format_version(travsr_core::SIGNATURE_FORMAT_VERSION)
            .unwrap();

        // Index the committed content, then let a Phase B run settle.
        reindex_files(std::slice::from_ref(&ts_path), tmp.path(), &mut store).unwrap();
        store.set_meta("phase_b_dirty", "0").unwrap();

        // Edit and reindex — the watcher path. Phase A nodes are rewritten.
        std::fs::write(&ts_path, "export class NewSvc { bar() {} }").unwrap();
        reindex_files(std::slice::from_ref(&ts_path), tmp.path(), &mut store).unwrap();
        store.set_meta("phase_b_dirty", "0").unwrap(); // pretend a run cleared it

        // Now revert to the committed content and reindex again, which is what
        // `git checkout <file>` / `stash pop` / a branch switch produce. The
        // tree is clean afterwards, yet the content changed relative to what is
        // indexed, so the semantic layer is degraded and must say so.
        std::fs::write(&ts_path, committed).unwrap();
        reindex_files(std::slice::from_ref(&ts_path), tmp.path(), &mut store).unwrap();
        assert_eq!(
            store.get_meta("phase_b_dirty").unwrap().as_deref(),
            Some("1"),
            "a revert back to committed content still degrades Phase B, so the \
             flag must be set even though the working tree is now clean"
        );
    }

    /// A2 follow-up: reindexing ONLY a workspace member (the common incremental
    /// case — edit a member, commit; the root is not in the batch) must keep the
    /// member's inherited `serde = { workspace = true }` edge pointed at the root
    /// version, not degrade it to the `@workspace` sentinel.
    #[test]
    fn member_only_reindex_keeps_inherited_dep_at_root_version() {
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());

        // Workspace root declares the version under [workspace.dependencies].
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/member\"]\n\n\
             [workspace.dependencies]\nserde = \"1.0.200\"\n",
        )
        .unwrap();
        // Member inherits it with `{ workspace = true }`.
        let member_dir = tmp.path().join("crates/member");
        std::fs::create_dir_all(&member_dir).unwrap();
        let member_manifest = member_dir.join("Cargo.toml");
        std::fs::write(
            &member_manifest,
            "[package]\nname = \"member\"\nversion = \"0.1.0\"\n\n\
             [dependencies]\nserde = { workspace = true }\n",
        )
        .unwrap();

        let db_path = tmp.path().join(".travsr/graph.db");
        std::fs::create_dir_all(tmp.path().join(".travsr")).unwrap();
        let mut store = travsr_store::SqliteStore::open(&db_path).unwrap();
        store
            .set_signature_format_version(travsr_core::SIGNATURE_FORMAT_VERSION)
            .unwrap();
        store.set_meta("corpus", "test").unwrap();

        // Reindex ONLY the member manifest — the root is not in this batch.
        reindex_files(
            std::slice::from_ref(&member_manifest),
            tmp.path(),
            &mut store,
        )
        .unwrap();

        let versioned = travsr_analysis::data_format::cargo_package_node("serde", "1.0.200").id;
        let sentinel = travsr_analysis::data_format::cargo_package_node("serde", "workspace").id;
        let edges = store.all_edges().unwrap();
        assert!(
            edges.iter().any(|(_, dst, _, _)| *dst == versioned),
            "member-only reindex must resolve serde to the root version 1.0.200"
        );
        assert!(
            !edges.iter().any(|(_, dst, _, _)| *dst == sentinel),
            "member-only reindex must NOT degrade serde to the @workspace sentinel"
        );
    }

    /// B: the C1 manifest cross-link pass is gated on the Phase B cycle writing
    /// a `crate` node. A cycle that writes none (e.g. a Go-only repo) must not
    /// create the `crate -> package` link even when both nodes already exist;
    /// a cycle that writes a crate node must.
    #[test]
    fn cross_link_gated_on_crate_nodes_in_cycle() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".travsr")).unwrap();
        let mut store =
            travsr_store::SqliteStore::open(&tmp.path().join(".travsr/graph.db")).unwrap();

        // Pre-seed a manifest package node and a module crate node with matching
        // bare names, as prior cycles would have written them.
        let pkg = travsr_analysis::data_format::cargo_package_node("serde", "1.0.200");
        let crate_node = travsr_core::Node::new(
            travsr_core::VName::new("test", "", "", "rust", "crate:serde"),
            "crate",
        );
        store.put_node(&pkg).unwrap();
        store.put_node(&crate_node).unwrap();

        let linked = |store: &travsr_store::SqliteStore| {
            store
                .all_edges()
                .unwrap()
                .iter()
                .any(|(src, dst, _, _)| *src == crate_node.id && *dst == pkg.id)
        };

        // Cycle 1: writes NO crate node -> the gate skips the cross-link scan.
        write_phase_b_results(
            &mut store,
            "test",
            vec![],
            vec![],
            vec![],
            travsr_plugin_host::PhaseBOutcome::default(),
        );
        assert!(
            !linked(&store),
            "a cycle with no crate node must not run the cross-link pass"
        );

        // Cycle 2: writes a crate node -> the gate runs the cross-link pass.
        write_phase_b_results(
            &mut store,
            "test",
            vec![crate_node.clone()],
            vec![],
            vec![],
            travsr_plugin_host::PhaseBOutcome::default(),
        );
        assert!(
            linked(&store),
            "a cycle that writes a crate node links crate -> package"
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

    /// L4 (#376 lifecycle plan): found indexing kubernetes/website, whose
    /// entire doc corpus lives under `content/` — the standard source root
    /// for Hugo/Jekyll/Gatsby-style static sites. Before `content` was added
    /// to `KNOWN_SOURCE_DIRS`, a single top-level `content/` directory large
    /// enough to dominate the repo (>=1000 files, >=15% of the total) was
    /// flagged as a false-positive "large dep dir" and auto-excluded in
    /// non-TTY runs with no visible warning in the command's own output —
    /// silently discarding the repo's actual content.
    #[test]
    fn detect_large_dep_dir_does_not_flag_content() {
        let repo_root = std::path::Path::new("/repo");
        let mut files: Vec<PathBuf> = (0..1200)
            .map(|i| repo_root.join(format!("content/en/docs/page-{i}.md")))
            .collect();
        files.push(repo_root.join("go.mod"));
        assert_eq!(
            detect_large_dep_dir(&files, repo_root),
            None,
            "content/ is a known source dir and must never be auto-excluded"
        );
    }

    #[test]
    fn detect_large_dep_dir_still_flags_an_unknown_large_dir() {
        let repo_root = std::path::Path::new("/repo");
        let mut files: Vec<PathBuf> = (0..1200)
            .map(|i| repo_root.join(format!("node_modules/pkg-{i}/index.js")))
            .collect();
        files.push(repo_root.join("go.mod"));
        let detected = detect_large_dep_dir(&files, repo_root);
        assert_eq!(
            detected.map(|(dir, ..)| dir),
            Some("node_modules".to_string()),
            "an unknown, dominant top-level dir must still be flagged"
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

    /// #580: `fsck` in report mode (no `--fix`) must surface orphan edges rather
    /// than reporting the graph clean. An injected `ref/call` edge whose `dst`
    /// is absent from `nodes` must be counted in `orphan_edges_detected` and
    /// must NOT be deleted by the report path.
    #[test]
    fn fsck_report_mode_counts_orphan_edges_without_deleting() {
        use travsr_core::{Edge, EdgeKind, NodeId};

        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());

        std::fs::write(tmp.path().join("svc.ts"), "export class Svc { run() {} }").unwrap();

        std::env::set_var("TRAVSR_DISABLE_REGISTRY", "1");
        init_repo(tmp.path()).unwrap();
        std::env::remove_var("TRAVSR_DISABLE_REGISTRY");

        let db_path = tmp.path().join(".travsr/graph.db");

        // Inject an orphan ref/call edge: both endpoints reference node ids that
        // do not exist, mirroring the write-path defect from #578/#579.
        {
            let mut store = SqliteStore::open(&db_path).unwrap();
            store
                .put_edge(&Edge::new(
                    NodeId(u64::MAX),
                    NodeId(u64::MAX - 1),
                    EdgeKind::RefCall,
                ))
                .unwrap();
        }

        let edges_before = SqliteStore::open(&db_path).unwrap().edge_count().unwrap();

        // Report mode (fix=false): must detect the orphan and leave it in place.
        let report = fsck_repo(tmp.path(), false, false).unwrap();
        assert_eq!(
            report.orphan_edges_detected, 1,
            "report mode must count the injected orphan edge"
        );

        let edges_after = SqliteStore::open(&db_path).unwrap().edge_count().unwrap();
        assert_eq!(
            edges_before, edges_after,
            "report mode must not delete edges ({edges_before} -> {edges_after})"
        );

        // Fix mode: sweeps the orphan and reports it swept.
        let fixed = fsck_repo(tmp.path(), true, false).unwrap();
        assert_eq!(
            fixed.orphan_edges_swept, 1,
            "fix mode must sweep the injected orphan edge"
        );
        assert_eq!(
            SqliteStore::open(&db_path)
                .unwrap()
                .count_orphans()
                .unwrap(),
            0,
            "no orphan edges must remain after --fix"
        );
    }

    /// #650: `fsck` must detect self-referential (`src == dst`) `ref/call` edges
    /// in report mode and sweep them — together with their occurrence sites —
    /// under `--fix`, without disturbing legitimate edges. Covers DBs written
    /// before the `write_scip_attributed_batch` guard existed.
    #[test]
    fn fsck_detects_and_sweeps_self_referential_ref_call_edges() {
        use travsr_core::{Edge, EdgeKind, Node, VName};

        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());

        std::fs::write(tmp.path().join("svc.ts"), "export class Svc { run() {} }").unwrap();

        std::env::set_var("TRAVSR_DISABLE_REGISTRY", "1");
        init_repo(tmp.path()).unwrap();
        std::env::remove_var("TRAVSR_DISABLE_REGISTRY");

        let db_path = tmp.path().join(".travsr/graph.db");

        // Inject a self-referential ref/call edge on a REAL node (so it is not an
        // orphan) plus its occurrence site, mirroring a pre-guard DB.
        let self_id = {
            let mut store = SqliteStore::open(&db_path).unwrap();
            let n = Node::new(
                VName::new("", "", "svc.ts", "typescript", "fn:selfcall"),
                "function",
            );
            store.put_node(&n).unwrap();
            store
                .put_edge(&Edge::new(n.id, n.id, EdgeKind::RefCall))
                .unwrap();
            store.record_edge_sites(&[(n.id, n.id, 1)]).unwrap();
            n.id
        };

        // Report mode: detect, do not delete.
        let report = fsck_repo(tmp.path(), false, false).unwrap();
        assert_eq!(
            report.self_ref_call_edges_detected, 1,
            "report mode must count the injected self-loop"
        );
        assert_eq!(
            SqliteStore::open(&db_path)
                .unwrap()
                .count_self_ref_call_edges()
                .unwrap(),
            1,
            "report mode must not delete the self-loop"
        );

        // Fix mode: sweep edge + site.
        let fixed = fsck_repo(tmp.path(), true, false).unwrap();
        assert_eq!(
            fixed.self_ref_call_edges_swept, 1,
            "fix mode must sweep the injected self-loop"
        );
        let store = SqliteStore::open(&db_path).unwrap();
        assert_eq!(
            store.count_self_ref_call_edges().unwrap(),
            0,
            "no self-referential ref/call edges may remain after --fix"
        );
        assert!(
            store.reference_sites(self_id).unwrap().is_empty(),
            "the self-loop's occurrence site must be swept with its edge"
        );
    }

    /// Regression: a file tracked in the DB (a `.yml` under `.github/`, which
    /// carries a `file` node) but present on disk must NOT be reported as a
    /// ghost, and `--fix` must not delete its tracking row. Before the fix the
    /// disk re-walk diverged from the DB `files` set on two axes — the
    /// `ignore::WalkBuilder` default skips hidden dirs like `.github/`, and it
    /// also filtered by `Language::from_extension` — so existing files such as
    /// `.github/workflows/*.yml` and `.mcp.json` were flagged as ghosts.
    #[test]
    fn fsck_does_not_flag_existing_non_code_file_as_ghost() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());

        std::fs::write(tmp.path().join("svc.ts"), "export class Svc { run() {} }").unwrap();
        std::fs::create_dir_all(tmp.path().join(".github/workflows")).unwrap();
        let yml_rel = ".github/workflows/ci.yml";
        std::fs::write(tmp.path().join(yml_rel), "name: ci\non: [push]\n").unwrap();

        std::env::set_var("TRAVSR_DISABLE_REGISTRY", "1");
        init_repo(tmp.path()).unwrap();
        std::env::remove_var("TRAVSR_DISABLE_REGISTRY");

        let db_path = tmp.path().join(".travsr/graph.db");

        // Precondition: the .yml is tracked in the DB (has a `files` row).
        assert!(
            SqliteStore::open(&db_path)
                .unwrap()
                .get_all_file_hashes()
                .unwrap()
                .contains_key(yml_rel),
            "precondition: init must track the non-code .yml file in `files`"
        );

        // Report mode: the existing .yml must NOT be flagged as a ghost.
        let report = fsck_repo(tmp.path(), false, false).unwrap();
        assert!(
            !report.ghost_paths.iter().any(|p| p == yml_rel),
            "existing non-code file wrongly reported as ghost: {:?}",
            report.ghost_paths
        );

        // Fix mode must not delete the existing .yml's tracking row.
        fsck_repo(tmp.path(), true, false).unwrap();
        assert!(
            SqliteStore::open(&db_path)
                .unwrap()
                .get_all_file_hashes()
                .unwrap()
                .contains_key(yml_rel),
            "`--fix` deleted an existing non-code file's tracking row"
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

    /// #376 Phase 1: `travsr init` on a docs fixture produces exactly one
    /// `file` node and one `doc-chunk` node per heading section, with the
    /// signature format matching the plan's anchor scheme.
    #[test]
    fn init_repo_indexes_markdown_docs_with_exact_counts() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        std::fs::write(
            tmp.path().join("guide.md"),
            "## Overview\nSome explanatory content here.\n## Setup\nMore content, also here.\n",
        )
        .unwrap();

        std::env::set_var("TRAVSR_DISABLE_REGISTRY", "1");
        let stats = init_repo(tmp.path()).unwrap();
        std::env::remove_var("TRAVSR_DISABLE_REGISTRY");
        assert!(stats.files_indexed >= 1);

        let db_path = tmp.path().join(".travsr/graph.db");
        let store = travsr_store::SqliteStore::open(&db_path).unwrap();
        let file_nodes = store.nodes_by_kind("file").unwrap();
        let doc_nodes = store.nodes_by_kind("doc-chunk").unwrap();

        assert_eq!(
            file_nodes
                .iter()
                .filter(|n| n.vname.path == "guide.md")
                .count(),
            1
        );
        assert_eq!(doc_nodes.len(), 2, "one doc-chunk per top-level heading");
        let sigs: std::collections::HashSet<_> = doc_nodes
            .iter()
            .map(|n| n.vname.signature.as_str())
            .collect();
        assert!(sigs.contains("doc:overview"));
        assert!(sigs.contains("doc:setup"));
    }

    /// #376 Phase 1: editing a `.md` file incrementally (reindex_files) must
    /// leave the graph in the same state as a full rebuild from scratch —
    /// same highest-risk invariant `incremental_delete_matches_full_rebuild`
    /// checks for code, applied to prose.
    #[test]
    fn markdown_full_vs_incremental_equality() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        let doc_path = tmp.path().join("guide.md");
        std::fs::write(&doc_path, "## Overview\nOriginal content.\n").unwrap();

        std::env::set_var("TRAVSR_DISABLE_REGISTRY", "1");
        init_repo(tmp.path()).unwrap();
        std::env::remove_var("TRAVSR_DISABLE_REGISTRY");

        // Incremental path: edit the file on disk, then reindex just it.
        std::fs::write(
            &doc_path,
            "## Overview\nEdited content.\n## New Section\nA whole new section.\n",
        )
        .unwrap();
        {
            let db_path = tmp.path().join(".travsr/graph.db");
            let mut store = travsr_store::SqliteStore::open(&db_path).unwrap();
            reindex_files(std::slice::from_ref(&doc_path), tmp.path(), &mut store).unwrap();
        }
        let (inc_nodes, inc_edges) = {
            let db_path = tmp.path().join(".travsr/graph.db");
            let store = travsr_store::SqliteStore::open(&db_path).unwrap();
            (store.node_count().unwrap(), store.edge_count().unwrap())
        };

        // Full-rebuild path: wipe .travsr and re-init on the same (edited) tree.
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
            "incremental doc edit + reindex must yield same node count as full rebuild \
             (incremental={inc_nodes}, full={full_nodes})"
        );
        assert_eq!(
            inc_edges, full_edges,
            "incremental doc edit + reindex must yield same edge count as full rebuild \
             (incremental={inc_edges}, full={full_edges})"
        );
    }

    /// #376 Phase 1: deleting a `.md` file must tombstone its doc-chunk nodes
    /// exactly like any other file kind — no ghost doc-chunk nodes survive.
    #[test]
    fn markdown_delete_tombstones_doc_chunk_nodes() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        let doc_path = tmp.path().join("guide.md");
        std::fs::write(&doc_path, "## Overview\nSome content.\n").unwrap();
        std::fs::write(tmp.path().join("other.md"), "## Kept\nStays around.\n").unwrap();

        std::env::set_var("TRAVSR_DISABLE_REGISTRY", "1");
        init_repo(tmp.path()).unwrap();
        std::env::remove_var("TRAVSR_DISABLE_REGISTRY");

        std::fs::remove_file(&doc_path).unwrap();
        let db_path = tmp.path().join(".travsr/graph.db");
        let mut store = travsr_store::SqliteStore::open(&db_path).unwrap();
        reindex_files(std::slice::from_ref(&doc_path), tmp.path(), &mut store).unwrap();

        let doc_nodes = store.nodes_by_kind("doc-chunk").unwrap();
        assert!(
            doc_nodes.iter().all(|n| n.vname.path != "guide.md"),
            "deleted file's doc-chunk nodes must be tombstoned, not left as ghosts"
        );
        assert!(
            doc_nodes.iter().any(|n| n.vname.path == "other.md"),
            "unrelated file's doc-chunk nodes must survive the delete"
        );
    }

    /// #376 Phase 1: `.travsrignore` applies to `.md` files exactly like any
    /// other extension (no markdown-specific ignore code exists — the walker
    /// is extension-agnostic), and a negation pattern re-includes a file an
    /// earlier broader rule excluded.
    #[test]
    fn travsrignore_excludes_and_negation_reincludes_markdown() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        std::fs::create_dir_all(tmp.path().join("drafts")).unwrap();
        std::fs::write(
            tmp.path().join("drafts/wip.md"),
            "## Draft\nNot ready yet.\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("drafts/important.md"),
            "## Important\nMust still be indexed.\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join(".travsrignore"),
            "drafts/*.md\n!drafts/important.md\n",
        )
        .unwrap();

        std::env::set_var("TRAVSR_DISABLE_REGISTRY", "1");
        init_repo(tmp.path()).unwrap();
        std::env::remove_var("TRAVSR_DISABLE_REGISTRY");

        let db_path = tmp.path().join(".travsr/graph.db");
        let store = travsr_store::SqliteStore::open(&db_path).unwrap();
        let file_nodes = store.nodes_by_kind("file").unwrap();
        let paths: std::collections::HashSet<_> =
            file_nodes.iter().map(|n| n.vname.path.as_str()).collect();

        assert!(
            !paths.contains("drafts/wip.md"),
            "wip.md must be excluded by the broad .travsrignore rule"
        );
        assert!(
            paths.contains("drafts/important.md"),
            "important.md must be re-included by the negation rule"
        );
    }

    /// #376 W1: `travsr init` deliberately skips `embed_text` generation (it
    /// parses every source file and blocks for minutes on a large repo), so
    /// every doc chunk it writes starts with NULL prose. The sidecar now refuses
    /// to embed those — otherwise it builds a heading-and-path-only vector and,
    /// candidacy being presence-only, never revisits it. This is the daemon-side
    /// half: fill the prose in before the auto-embed path spawns anything.
    #[test]
    fn ensure_doc_embed_texts_fills_prose_left_null_by_init() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        std::fs::create_dir_all(tmp.path().join("docs")).unwrap();
        std::fs::write(
            tmp.path().join("docs/adr.md"),
            "# Decision Record\n\nWe picked the second option because the first \
             requires a global lock during reindex, which stalls every query.\n\n\
             ## Consequences\n\nReaders never block, writers serialise on one path.\n",
        )
        .unwrap();

        std::env::set_var("TRAVSR_DISABLE_REGISTRY", "1");
        init_repo(tmp.path()).unwrap();
        std::env::remove_var("TRAVSR_DISABLE_REGISTRY");

        let db_path = tmp.path().join(".travsr/graph.db");
        let store = travsr_store::SqliteStore::open(&db_path).unwrap();
        let chunks = store.nodes_by_kind("doc-chunk").unwrap();
        assert!(
            !chunks.is_empty(),
            "the markdown file must produce doc-chunk nodes"
        );
        let missing_before = store.doc_nodes_missing_embed_text().unwrap().len();
        assert_eq!(
            missing_before,
            chunks.len(),
            "init leaves every doc chunk without prose"
        );

        let store = std::sync::Mutex::new(store);
        let filled = ensure_doc_embed_texts(&store, tmp.path());
        assert_eq!(filled, chunks.len());

        let s = store.lock().unwrap();
        assert!(
            s.doc_nodes_missing_embed_text().unwrap().is_empty(),
            "every doc chunk must carry prose after the backfill"
        );
        let texts = s.get_embed_texts(&chunks.iter().map(|n| n.id).collect::<Vec<_>>());
        let joined: String = texts.unwrap_or_default().values().cloned().collect();
        assert!(
            joined.contains("global lock"),
            "the backfilled text must be the chunk's prose, not just its heading: {joined}"
        );
    }

    /// #376 W2: the tick must launch a pass when invalidation is pending even
    /// though coverage is complete, but must not relaunch one for a backlog a
    /// previous pass already looked at. Some tombstones are deliberately not
    /// drainable yet (the sidecar defers a node whose `embed_text` is NULL), and
    /// without the re-arm rule those would spawn a sidecar every 60 s forever.
    #[test]
    fn invalidation_pass_arms_on_new_work_and_not_on_a_stale_backlog() {
        const NEVER: u64 = u64::MAX;
        // Nothing pending → nothing to do.
        assert!(!should_spawn_invalidation(0, NEVER, false));
        // First invalidation seen → spawn.
        assert!(should_spawn_invalidation(154, NEVER, false));
        // Same backlog a pass already processed → do not respawn.
        assert!(!should_spawn_invalidation(154, 154, false));
        // New edits arrived on top of the deferred backlog → spawn again.
        assert!(should_spawn_invalidation(160, 154, false));
        // A drain that shrank the backlog is still new information.
        assert!(should_spawn_invalidation(12, 154, false));
        // Never compete with a running sidecar.
        assert!(!should_spawn_invalidation(154, NEVER, true));
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

    /// The silent no-op. With no `.travsr/embed.toml`, this returned before
    /// touching the store, so `travsr embed reindex` printed "Preparing embed
    /// text for ..." and prepared nothing, and `travsr init` had the same gap.
    /// Only the model-*tier* decision needs a configured model; generating the
    /// text does not, which is why the daemon's own reindex path had always run
    /// it unconditionally. The two paths disagreed.
    #[test]
    fn embed_text_is_generated_with_no_model_configured() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(
            tmp.path().join("src/lib.rs"),
            "pub fn handler(a: u32) -> u32 { a }\n",
        )
        .unwrap();

        let db_path = tmp.path().join(".travsr/graph.db");
        let node = {
            let mut store = setup_embed_store(tmp.path());
            let node = travsr_core::Node::new(
                travsr_core::VName::new("c", "", "src/lib.rs", "rust", "fn:handler"),
                "function",
            )
            .with_line(1)
            .with_end_line(1);
            store.put_node(&node).unwrap();
            assert!(
                !store.nodes_missing_embed_text().unwrap().is_empty(),
                "precondition: the node starts with no embed_text"
            );
            node
        };

        assert!(
            travsr_plugin_host::repo_backend_id(tmp.path()).is_none(),
            "precondition: this repo has no embed model configured"
        );

        let regenerated = regenerate_embed_texts_if_stale(&db_path).unwrap();
        assert!(
            !regenerated,
            "no model means no tier change, so nothing was *re*generated"
        );

        let store = travsr_store::SqliteStore::open(&db_path).unwrap();
        let text = store
            .get_nodes_embed_text(&[node.id])
            .unwrap()
            .into_iter()
            .next()
            .map(|(_, t)| t);
        assert_eq!(
            text.as_deref(),
            Some("function: fn:handler | module: src/lib.rs | params: a: u32 | returns: u32"),
            "the text is generated at the Compact fallback, not skipped"
        );
    }

    /// #526: hook injection must prefer a repo's own `.travsr/embed.toml`
    /// override over the machine-wide `~/.travsr/embed.toml` default, the
    /// same resolution order `resolve_backend` (travsr-plugin-host) uses.
    #[test]
    fn hook_backend_id_prefers_repo_config_over_global() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());

        std::fs::create_dir_all(home.path().join(".travsr")).unwrap();
        std::fs::write(
            home.path().join(".travsr").join("embed.toml"),
            "active = \"global-backend\"\n",
        )
        .unwrap();

        let repo_travsr = repo.path().join(".travsr");
        std::fs::create_dir_all(&repo_travsr).unwrap();
        std::fs::write(
            repo_travsr.join("embed.toml"),
            "active = \"repo-backend\"\n",
        )
        .unwrap();

        let resolved = hook_backend_id(&repo_travsr.join("graph.db"));

        match old_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }

        assert_eq!(
            resolved.as_deref(),
            Some("repo-backend"),
            "repo's .travsr/embed.toml must win over the machine-wide default"
        );
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "dirs::home_dir() on Windows ignores HOME/USERPROFILE entirely (SHGetKnownFolderPath) - this test's isolation cannot work there, see crates/travsr-cli/tests/embed_switch.rs's module doc comment"
    )]
    fn hook_backend_id_falls_back_to_global_when_repo_unconfigured() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());

        std::fs::create_dir_all(home.path().join(".travsr")).unwrap();
        std::fs::write(
            home.path().join(".travsr").join("embed.toml"),
            "active = \"global-backend\"\n",
        )
        .unwrap();

        // No repo .travsr/embed.toml written at all.
        let resolved = hook_backend_id(&repo.path().join(".travsr").join("graph.db"));

        match old_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }

        assert_eq!(resolved.as_deref(), Some("global-backend"));
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
    fn maybe_spawn_embed_reconciles_stale_embed_text_model_id() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // #512: reproduce a model switch applied via daemon restart alone (no
        // explicit `travsr embed reindex`) — `embed_text_model_id` still names
        // a stale model while the repo's configured backend has moved on. The
        // auto-embed tick must reconcile this itself rather than silently
        // embedding under the wrong richness tier forever.
        let (tmp, mut store) = setup_repo_with_phase_b(5, Some("deadbeef"));
        store
            .set_meta("embed_text_model_id", "stale-old-model")
            .unwrap();
        let store = std::sync::Mutex::new(store);
        let flag = std::sync::atomic::AtomicBool::new(false);

        maybe_spawn_embed(tmp.path(), &store, &flag);

        let s = store.lock().unwrap_or_else(|e| e.into_inner());
        let reconciled = s.get_meta("embed_text_model_id").unwrap().unwrap();
        assert_eq!(reconciled, "bge-small-en-v1.5");
    }

    #[test]
    fn control_reindex_commit_stamps_last_commit_when_nothing_changed() {
        // Regression: reindex_files only stamps last_commit when any_changed
        // is true (avoids stamping FS noise). The daemon's git-hook path
        // never got the PR #207 fix that made init_repo stamp last_commit
        // unconditionally — so a commit whose files were already reindexed
        // by the live watcher before the hook ran (the common editor-save-
        // then-commit flow) left last_commit permanently stale.
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        let ts_path = tmp.path().join("svc.ts");
        std::fs::write(&ts_path, "export class Svc { go() {} }").unwrap();

        let db_path = tmp.path().join(".travsr/graph.db");
        std::fs::create_dir_all(tmp.path().join(".travsr")).unwrap();
        let mut store = travsr_store::SqliteStore::open(&db_path).unwrap();
        store
            .set_signature_format_version(travsr_core::SIGNATURE_FORMAT_VERSION)
            .unwrap();
        // Pre-reindex so the file's hash is already current — the next
        // ReindexCommit must find any_changed == false, same as the real
        // "editor already saved, watcher already indexed it" sequence.
        reindex_files(std::slice::from_ref(&ts_path), tmp.path(), &mut store).unwrap();
        assert!(store.get_meta("last_commit").unwrap().is_none());

        let store = std::sync::Mutex::new(store);
        let read_store = std::sync::Mutex::new(travsr_store::SqliteStore::open(&db_path).unwrap());
        let cache = std::sync::Mutex::new(query_cache::QueryCache::new(8));
        let phase_b_scheduler =
            phase_b_sched::PhaseBScheduler::new(std::time::Duration::from_secs(30));
        let (index_tx, _index_rx) = std::sync::mpsc::channel::<watcher::WatchEvent>();

        let msg = serde_json::to_string(&travsr_ipc::ControlMessage::ReindexCommit {
            sha: "deadbeef".to_string(),
        })
        .unwrap();
        let (resp, _shutdown) = handle_control_message(
            &msg,
            tmp.path(),
            &store,
            &read_store,
            &cache,
            &phase_b_scheduler,
            &index_tx,
            &std::sync::Mutex::new(EditorPlane::default()),
        );
        assert!(resp.ok, "control message must succeed: {resp:?}");

        let s = store.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            s.get_meta("last_commit").unwrap().as_deref(),
            Some("deadbeef"),
            "last_commit must be stamped even when reindex_files found nothing to change"
        );
    }

    // ── #621 startup / tick HEAD reconciliation ──────────────────────────────

    /// Commit a snapshot of the working tree and return the short HEAD sha.
    fn git_commit_all(dir: &std::path::Path, msg: &str) -> String {
        StdCommand::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .status()
            .unwrap();
        StdCommand::new("git")
            .args(["commit", "-q", "-m", msg])
            .current_dir(dir)
            .status()
            .unwrap();
        read_head_commit_sha(dir).unwrap()
    }

    /// The #621 repro: `last_commit` points at a commit that is no longer
    /// HEAD (history moved during daemon downtime). Reconcile must reindex
    /// the working tree, advance the marker to live HEAD, and arm Phase B.
    #[test]
    fn reconcile_head_drift_reindexes_and_stamps_on_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        std::fs::write(
            tmp.path().join("svc.ts"),
            "export class NewSvc { callee_good() {} }",
        )
        .unwrap();
        let head = git_commit_all(tmp.path(), "p");

        let db_path = tmp.path().join(".travsr/graph.db");
        std::fs::create_dir_all(tmp.path().join(".travsr")).unwrap();
        let mut store = travsr_store::SqliteStore::open(&db_path).unwrap();
        store
            .set_signature_format_version(travsr_core::SIGNATURE_FORMAT_VERSION)
            .unwrap();
        // The graph is pinned to a commit that no longer exists as HEAD.
        store.set_meta("last_commit", "f0f0f0f").unwrap();
        let store = std::sync::Mutex::new(store);
        let sched = phase_b_sched::PhaseBScheduler::new(std::time::Duration::from_secs(30));
        let (index_tx, _index_rx) = std::sync::mpsc::channel::<watcher::WatchEvent>();

        assert!(
            reconcile_head_drift(tmp.path(), &store, &sched, &index_tx),
            "mismatched marker must trigger a reconcile"
        );

        let s = store.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            s.get_meta("last_commit").unwrap().as_deref(),
            Some(head.as_str()),
            "marker must advance to live HEAD"
        );
        assert!(
            s.node_count().unwrap() > 0,
            "working tree must have been reindexed"
        );
        drop(s);
        assert!(
            sched.is_pending(),
            "whole-project Phase B rebuild must be armed"
        );
    }

    /// Marker == HEAD is the healthy steady state: the periodic tick must be
    /// a cheap no-op that neither reindexes nor arms Phase B.
    #[test]
    fn reconcile_head_drift_noop_when_marker_matches_head() {
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        std::fs::write(tmp.path().join("svc.ts"), "export class Svc { go() {} }").unwrap();
        let head = git_commit_all(tmp.path(), "p");

        let db_path = tmp.path().join(".travsr/graph.db");
        std::fs::create_dir_all(tmp.path().join(".travsr")).unwrap();
        let mut store = travsr_store::SqliteStore::open(&db_path).unwrap();
        store
            .set_signature_format_version(travsr_core::SIGNATURE_FORMAT_VERSION)
            .unwrap();
        store.set_meta("last_commit", &head).unwrap();
        let store = std::sync::Mutex::new(store);
        let sched = phase_b_sched::PhaseBScheduler::new(std::time::Duration::from_secs(30));
        let (index_tx, _index_rx) = std::sync::mpsc::channel::<watcher::WatchEvent>();

        assert!(
            !reconcile_head_drift(tmp.path(), &store, &sched, &index_tx),
            "matching marker must be a no-op"
        );
        assert!(!sched.is_pending(), "no-op must not arm Phase B");
        let s = store.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            s.node_count().unwrap(),
            0,
            "no-op must not reindex anything"
        );
    }

    /// An absent marker means init never stamped a commit; reconcile must not
    /// fabricate freshness for an index that never ran.
    #[test]
    fn reconcile_head_drift_skips_when_marker_absent() {
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        std::fs::write(tmp.path().join("svc.ts"), "export class Svc { go() {} }").unwrap();
        git_commit_all(tmp.path(), "p");

        let db_path = tmp.path().join(".travsr/graph.db");
        std::fs::create_dir_all(tmp.path().join(".travsr")).unwrap();
        let mut store = travsr_store::SqliteStore::open(&db_path).unwrap();
        store
            .set_signature_format_version(travsr_core::SIGNATURE_FORMAT_VERSION)
            .unwrap();
        let store = std::sync::Mutex::new(store);
        let sched = phase_b_sched::PhaseBScheduler::new(std::time::Duration::from_secs(30));
        let (index_tx, _index_rx) = std::sync::mpsc::channel::<watcher::WatchEvent>();

        assert!(!reconcile_head_drift(tmp.path(), &store, &sched, &index_tx));
        let s = store.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            s.get_meta("last_commit").unwrap().is_none(),
            "absent marker must stay absent"
        );
    }

    /// A repo with no commits has no HEAD to pin to; reconcile must bail
    /// before touching the store.
    #[test]
    fn reconcile_head_drift_skips_without_commits() {
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());

        let db_path = tmp.path().join(".travsr/graph.db");
        std::fs::create_dir_all(tmp.path().join(".travsr")).unwrap();
        let mut store = travsr_store::SqliteStore::open(&db_path).unwrap();
        store
            .set_signature_format_version(travsr_core::SIGNATURE_FORMAT_VERSION)
            .unwrap();
        store.set_meta("last_commit", "abc123").unwrap();
        let store = std::sync::Mutex::new(store);
        let sched = phase_b_sched::PhaseBScheduler::new(std::time::Duration::from_secs(30));
        let (index_tx, _index_rx) = std::sync::mpsc::channel::<watcher::WatchEvent>();

        assert!(!reconcile_head_drift(tmp.path(), &store, &sched, &index_tx));
        let s = store.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            s.get_meta("last_commit").unwrap().as_deref(),
            Some("abc123"),
            "marker untouched when HEAD is unreadable"
        );
    }

    /// Signature-format mismatch: the reindex is skipped inside reindex_files,
    /// so the marker must NOT advance — same guard as the hook path (#405).
    #[test]
    fn reconcile_head_drift_does_not_stamp_on_version_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        std::fs::write(tmp.path().join("svc.ts"), "export class Svc { go() {} }").unwrap();
        git_commit_all(tmp.path(), "p");

        let db_path = tmp.path().join(".travsr/graph.db");
        std::fs::create_dir_all(tmp.path().join(".travsr")).unwrap();
        let mut store = travsr_store::SqliteStore::open(&db_path).unwrap();
        // DB built by an older binary.
        store.set_signature_format_version(0).unwrap();
        store.set_meta("last_commit", "f0f0f0f").unwrap();
        let store = std::sync::Mutex::new(store);
        let sched = phase_b_sched::PhaseBScheduler::new(std::time::Duration::from_secs(30));
        let (index_tx, _index_rx) = std::sync::mpsc::channel::<watcher::WatchEvent>();

        reconcile_head_drift(tmp.path(), &store, &sched, &index_tx);
        let s = store.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            s.get_meta("last_commit").unwrap().as_deref(),
            Some("f0f0f0f"),
            "marker must not claim freshness for a skipped reindex"
        );
    }

    /// The shape a whole-tree change needs, in one call: everything git tracks
    /// gets indexed, and everything it no longer tracks gets removed.
    ///
    /// The second half is the one that is easy to leave out. `reindex_files`
    /// only visits the paths handed to it, so a file that vanished from the tree
    /// is absent from that list by definition and nothing would ever delete its
    /// nodes. A branch checkout produces exactly that: `travsr ask` keeps
    /// answering with a path that is not on disk.
    #[test]
    fn reconcile_tracked_tree_indexes_what_git_tracks_and_prunes_what_it_does_not() {
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        std::fs::write(tmp.path().join("kept.ts"), "export class Kept { go() {} }").unwrap();
        std::fs::write(tmp.path().join("gone.ts"), "export class Gone { go() {} }").unwrap();
        git_commit_all(tmp.path(), "both files");

        std::fs::create_dir_all(tmp.path().join(".travsr")).unwrap();
        let mut store =
            travsr_store::SqliteStore::open(&tmp.path().join(".travsr/graph.db")).unwrap();
        store
            .set_signature_format_version(travsr_core::SIGNATURE_FORMAT_VERSION)
            .unwrap();

        // First pass: both tracked, both indexed.
        reconcile_tracked_tree(tmp.path(), &mut store).unwrap();
        let hashes = store.get_all_file_hashes().unwrap();
        assert!(hashes.contains_key("kept.ts"), "kept.ts should be indexed");
        assert!(hashes.contains_key("gone.ts"), "gone.ts should be indexed");

        // Now the tree loses a file, the way a branch checkout loses one: it is
        // gone from disk and gone from `git ls-files`.
        std::fs::remove_file(tmp.path().join("gone.ts")).unwrap();
        StdCommand::new("git")
            .args(["rm", "-q", "--cached", "gone.ts"])
            .current_dir(tmp.path())
            .status()
            .expect("git rm --cached");

        let (_dirty, files) = reconcile_tracked_tree(tmp.path(), &mut store).unwrap();

        let hashes = store.get_all_file_hashes().unwrap();
        assert!(
            hashes.contains_key("kept.ts"),
            "the surviving file must stay indexed"
        );
        assert!(
            !hashes.contains_key("gone.ts"),
            "a file git no longer tracks must not survive as a ghost: {:?}",
            hashes.keys().collect::<Vec<_>>()
        );
        assert!(
            files >= 1,
            "the reindex should have visited the tracked set"
        );
    }

    /// #645 WS-A: a file the drift DELETED (present in the graph, gone from the
    /// tracked-at-HEAD set) must have its nodes pruned by the reconcile, not
    /// left as a ghost from the discarded commit. This is the issue's exact
    /// repro: index at C2 (which added `second.ts`), then `git reset --hard C1`
    /// removes it and moves HEAD back.
    #[test]
    fn reconcile_head_drift_prunes_drift_deleted_file() {
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        std::fs::write(tmp.path().join("a.ts"), "export class A { go() {} }").unwrap();
        std::fs::write(tmp.path().join("b.ts"), "export class B { go() {} }").unwrap();
        let c1 = git_commit_all(tmp.path(), "c1");
        std::fs::write(
            tmp.path().join("second.ts"),
            "export class Second { secondCommitSym() {} }",
        )
        .unwrap();
        let c2 = git_commit_all(tmp.path(), "c2 adds second.ts");
        assert_ne!(c1, c2);

        let db_path = tmp.path().join(".travsr/graph.db");
        std::fs::create_dir_all(tmp.path().join(".travsr")).unwrap();
        let mut store = travsr_store::SqliteStore::open(&db_path).unwrap();
        store
            .set_signature_format_version(travsr_core::SIGNATURE_FORMAT_VERSION)
            .unwrap();
        // Index the working tree as it stands at C2 (all three files present).
        let paths: Vec<std::path::PathBuf> = ["a.ts", "b.ts", "second.ts"]
            .iter()
            .map(|f| tmp.path().join(f))
            .collect();
        reindex_files(&paths, tmp.path(), &mut store).unwrap();
        store.set_meta("last_commit", &c2).unwrap();
        assert!(
            store
                .get_all_file_hashes()
                .unwrap()
                .contains_key("second.ts"),
            "second.ts must be in the graph before the drift"
        );

        // The drift: history moves back to C1, deleting second.ts from the tree.
        StdCommand::new("git")
            .args(["reset", "--hard", &c1])
            .current_dir(tmp.path())
            .status()
            .unwrap();

        let store = std::sync::Mutex::new(store);
        let sched = phase_b_sched::PhaseBScheduler::new(std::time::Duration::from_secs(30));
        let (index_tx, _index_rx) = std::sync::mpsc::channel::<watcher::WatchEvent>();

        assert!(reconcile_head_drift(tmp.path(), &store, &sched, &index_tx));

        let s = store.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            s.get_meta("last_commit").unwrap().as_deref(),
            Some(c1.as_str()),
            "marker must advance to the reconciled HEAD"
        );
        let hashes = s.get_all_file_hashes().unwrap();
        assert!(
            !hashes.contains_key("second.ts"),
            "drift-deleted second.ts must be pruned, not left as a ghost"
        );
        assert!(
            hashes.contains_key("a.ts") && hashes.contains_key("b.ts"),
            "surviving tracked files must be untouched: {:?}",
            hashes.keys().collect::<Vec<_>>()
        );
    }

    /// #645 WS-A: the reconcile's ghost prune reuses the §6.5 TOCTOU re-check —
    /// a path in the graph but absent from the tracked set that is still present
    /// on disk (an untracked working file) must NOT be deleted.
    #[test]
    fn reconcile_head_drift_prune_toctou_skips_present_file() {
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        std::fs::write(tmp.path().join("tracked.ts"), "export class T { go() {} }").unwrap();
        let c1 = git_commit_all(tmp.path(), "c1");
        std::fs::write(tmp.path().join("another.ts"), "export class N { go() {} }").unwrap();
        let c2 = git_commit_all(tmp.path(), "c2");
        // On disk but never committed — tracked_files_from_git will not list it.
        std::fs::write(
            tmp.path().join("untracked.ts"),
            "export class U { go() {} }",
        )
        .unwrap();

        let db_path = tmp.path().join(".travsr/graph.db");
        std::fs::create_dir_all(tmp.path().join(".travsr")).unwrap();
        let mut store = travsr_store::SqliteStore::open(&db_path).unwrap();
        store
            .set_signature_format_version(travsr_core::SIGNATURE_FORMAT_VERSION)
            .unwrap();
        let paths: Vec<std::path::PathBuf> = ["tracked.ts", "another.ts", "untracked.ts"]
            .iter()
            .map(|f| tmp.path().join(f))
            .collect();
        reindex_files(&paths, tmp.path(), &mut store).unwrap();
        // Stale marker so reconcile actually runs (HEAD is c2).
        store.set_meta("last_commit", &c1).unwrap();

        let store = std::sync::Mutex::new(store);
        let sched = phase_b_sched::PhaseBScheduler::new(std::time::Duration::from_secs(30));
        let (index_tx, _index_rx) = std::sync::mpsc::channel::<watcher::WatchEvent>();

        assert!(reconcile_head_drift(tmp.path(), &store, &sched, &index_tx));
        let s = store.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            s.get_meta("last_commit").unwrap().as_deref(),
            Some(c2.as_str())
        );
        assert!(
            s.get_all_file_hashes()
                .unwrap()
                .contains_key("untracked.ts"),
            "an untracked file present on disk must survive the TOCTOU re-check"
        );
    }

    #[test]
    fn phase_b_finish_guard_releases_slot_on_panic() {
        // #404: if the background Phase B worker unwinds, the single-flight slot
        // must still be released. Before the drop guard, a panic skipped
        // finish_with_outcome and `running` stayed true forever, silently
        // freezing every future Phase B refresh.
        let sched = phase_b_sched::PhaseBScheduler::new(std::time::Duration::from_secs(0));
        sched.mark_dirty();
        assert!(sched.try_claim(), "first claim succeeds");
        assert!(sched.is_running(), "slot held while the run is in flight");

        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_with_phase_b_finish_guard(&sched, || panic!("simulated Phase B unwind"));
        }));
        assert!(panicked.is_err(), "the panic must propagate past the guard");
        assert!(
            !sched.is_running(),
            "#404: slot must be released after the worker panics"
        );
        // A panic maps to AllCrashed, which counts toward MAX_FAILURES but does
        // not block the next claim after a single failure.
        assert_eq!(sched.consecutive_failures(), 1);
        sched.mark_dirty();
        assert!(sched.try_claim(), "scheduler must be able to claim again");
    }

    #[test]
    fn tracked_files_from_git_lists_committed_and_errors_off_repo() {
        // #405: the fallback enumerates the full tracked set. Happy path returns
        // committed files; a path git cannot enter surfaces as an error so the
        // caller can refuse to claim freshness.
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        std::fs::write(tmp.path().join("a.ts"), "export const x = 1;").unwrap();
        StdCommand::new("git")
            .args(["add", "."])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        StdCommand::new("git")
            .args(["commit", "-q", "-m", "init"])
            .current_dir(tmp.path())
            .status()
            .unwrap();

        let tracked = tracked_files_from_git(tmp.path()).unwrap();
        assert!(
            tracked.iter().any(|p| p.ends_with("a.ts")),
            "tracked set must include the committed file: {tracked:?}"
        );

        let missing = tmp.path().join("does-not-exist");
        assert!(
            tracked_files_from_git(&missing).is_err(),
            "must error when git cannot be spawned"
        );
    }

    /// #698 review P1: discovery cannot tell which daemon owns which socket,
    /// so the client broadcasts and identity is settled here. A report naming
    /// another repo must be dropped: its paths are keys into a different
    /// graph, and accepting it makes `travsr daemon lsp` quote files this repo
    /// does not have.
    #[test]
    fn a_report_for_another_repo_is_rejected_not_recorded() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("graph.db");
        let mut store = travsr_store::SqliteStore::open(&db_path).unwrap();
        store
            .set_signature_format_version(travsr_core::SIGNATURE_FORMAT_VERSION)
            .unwrap();
        let store = std::sync::Mutex::new(store);
        let read_store = std::sync::Mutex::new(travsr_store::SqliteStore::open(&db_path).unwrap());
        let cache = std::sync::Mutex::new(query_cache::QueryCache::new(8));
        let phase_b_scheduler =
            phase_b_sched::PhaseBScheduler::new(std::time::Duration::from_secs(30));
        let (index_tx, _index_rx) = std::sync::mpsc::channel::<watcher::WatchEvent>();
        let sessions = std::sync::Mutex::new(EditorPlane::default());

        let mine = tmp.path().join("repo-a");
        let theirs = tmp.path().join("repo-b");
        std::fs::create_dir_all(&mine).unwrap();
        std::fs::create_dir_all(&theirs).unwrap();

        let report = |root: &std::path::Path| {
            serde_json::to_string(&travsr_ipc::ControlMessage::ReportLspDiagnostics {
                repo_root: root.to_string_lossy().into_owned(),
                session: "w1".to_string(),
                ttl_secs: 900,
                files: vec![travsr_ipc::message::FileDiagnostics {
                    path: "src/a.ts".to_string(),
                    errors: 2,
                    warnings: 0,
                }],
                seen: 1,
                undiagnosed: 0,
            })
            .unwrap()
        };

        let (resp, _) = handle_control_message(
            &report(&theirs),
            &mine,
            &store,
            &read_store,
            &cache,
            &phase_b_scheduler,
            &index_tx,
            &sessions,
        );
        assert!(!resp.ok, "a foreign report must be refused: {resp:?}");
        {
            let plane = sessions.lock().unwrap_or_else(|e| e.into_inner());
            assert!(
                plane.sessions.is_empty(),
                "a foreign report must not create a session"
            );
            // Counted rather than silently dropped (#698 review, P3): the
            // client cannot observe the refusal, so a persistent mismatch is
            // undiagnosable unless the daemon keeps a tally someone can read.
            // Keyed on the normalized root, so two spellings of one directory
            // cannot become two entries (#698 review, P2).
            assert_eq!(
                plane.refused.get(&travsr_ipc::normalize_repo_root(&theirs)),
                Some(&1),
                "a refused report must be countable: {:?}",
                plane.refused
            );
        }

        // The same report for this repo is accepted, so the check rejects by
        // identity rather than rejecting everything.
        let (resp, _) = handle_control_message(
            &report(&mine),
            &mine,
            &store,
            &read_store,
            &cache,
            &phase_b_scheduler,
            &index_tx,
            &sessions,
        );
        assert!(resp.ok, "an own-repo report must be accepted: {resp:?}");
        assert_eq!(
            sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .sessions
                .len(),
            1
        );
    }

    /// #698 review P2: the refusal tally takes a caller-supplied string as a
    /// map key, so it needs the same bound the sessions map has. The control
    /// socket is reachable by any process running as this user and this
    /// variant is parsed before any other limit applies.
    #[test]
    fn the_refusal_tally_is_bounded_and_keyed_on_the_normalized_root() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("graph.db");
        let mut store = travsr_store::SqliteStore::open(&db_path).unwrap();
        store
            .set_signature_format_version(travsr_core::SIGNATURE_FORMAT_VERSION)
            .unwrap();
        let store = std::sync::Mutex::new(store);
        let read_store = std::sync::Mutex::new(travsr_store::SqliteStore::open(&db_path).unwrap());
        let cache = std::sync::Mutex::new(query_cache::QueryCache::new(8));
        let phase_b_scheduler =
            phase_b_sched::PhaseBScheduler::new(std::time::Duration::from_secs(30));
        let (index_tx, _index_rx) = std::sync::mpsc::channel::<watcher::WatchEvent>();
        let sessions = std::sync::Mutex::new(EditorPlane::default());
        let mine = tmp.path().join("mine");
        std::fs::create_dir_all(&mine).unwrap();

        let report = |root: &str| {
            serde_json::to_string(&travsr_ipc::ControlMessage::ReportLspDiagnostics {
                repo_root: root.to_string(),
                session: "w".to_string(),
                ttl_secs: 900,
                files: vec![],
                seen: 0,
                undiagnosed: 0,
            })
            .unwrap()
        };
        let call = |line: String| {
            handle_control_message(
                &line,
                &mine,
                &store,
                &read_store,
                &cache,
                &phase_b_scheduler,
                &index_tx,
                &sessions,
            );
        };

        // Far more distinct roots than the cap allows.
        for i in 0..(MAX_REFUSED_ROOTS * 3) {
            call(report(&format!("/nowhere/repo-{i}")));
        }

        let plane = sessions.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            plane.refused.len() <= MAX_REFUSED_ROOTS,
            "an unbounded map keyed on caller input is how a daemon dies: {} entries",
            plane.refused.len()
        );
        assert!(
            plane.refused_overflow > 0,
            "refusals past the cap must still be counted, not dropped"
        );
    }

    /// Two spellings of one directory must not read as two different repos:
    /// that is the exact confusion the tally exists to remove.
    #[test]
    fn trailing_slash_and_plain_root_collapse_to_one_refusal_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("graph.db");
        let mut store = travsr_store::SqliteStore::open(&db_path).unwrap();
        store
            .set_signature_format_version(travsr_core::SIGNATURE_FORMAT_VERSION)
            .unwrap();
        let store = std::sync::Mutex::new(store);
        let read_store = std::sync::Mutex::new(travsr_store::SqliteStore::open(&db_path).unwrap());
        let cache = std::sync::Mutex::new(query_cache::QueryCache::new(8));
        let phase_b_scheduler =
            phase_b_sched::PhaseBScheduler::new(std::time::Duration::from_secs(30));
        let (index_tx, _index_rx) = std::sync::mpsc::channel::<watcher::WatchEvent>();
        let sessions = std::sync::Mutex::new(EditorPlane::default());

        let mine = tmp.path().join("mine");
        let theirs = tmp.path().join("theirs");
        std::fs::create_dir_all(&mine).unwrap();
        std::fs::create_dir_all(&theirs).unwrap();

        let report = |root: String| {
            serde_json::to_string(&travsr_ipc::ControlMessage::ReportLspDiagnostics {
                repo_root: root,
                session: "w".to_string(),
                ttl_secs: 900,
                files: vec![],
                seen: 0,
                undiagnosed: 0,
            })
            .unwrap()
        };
        for spelling in [
            theirs.to_string_lossy().into_owned(),
            format!("{}/", theirs.to_string_lossy()),
        ] {
            handle_control_message(
                &report(spelling),
                &mine,
                &store,
                &read_store,
                &cache,
                &phase_b_scheduler,
                &index_tx,
                &sessions,
            );
        }

        let plane = sessions.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            plane.refused.len(),
            1,
            "one directory, one entry: {:?}",
            plane.refused
        );
        assert_eq!(plane.refused.values().sum::<usize>(), 2);
    }

    #[test]
    fn reindex_commit_errors_without_stamping_when_git_unavailable() {
        // #405: when neither the commit diff nor the tracked-file fallback can be
        // resolved (git cannot be spawned), the handler must report failure and
        // must NOT stamp last_commit. The old code logged "reindexing all tracked
        // files", reindexed nothing, stamped last_commit, and returned success —
        // silently dropping the commit's changes while status claimed freshness.
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("graph.db");
        let mut store = travsr_store::SqliteStore::open(&db_path).unwrap();
        store
            .set_signature_format_version(travsr_core::SIGNATURE_FORMAT_VERSION)
            .unwrap();
        let store = std::sync::Mutex::new(store);
        let read_store = std::sync::Mutex::new(travsr_store::SqliteStore::open(&db_path).unwrap());
        let cache = std::sync::Mutex::new(query_cache::QueryCache::new(8));
        let phase_b_scheduler =
            phase_b_sched::PhaseBScheduler::new(std::time::Duration::from_secs(30));
        let (index_tx, _index_rx) = std::sync::mpsc::channel::<watcher::WatchEvent>();

        // repo_root does not exist, so git cannot spawn there and both
        // changed_files_from_git and tracked_files_from_git error.
        let missing_repo = tmp.path().join("no-such-repo");
        let msg = serde_json::to_string(&travsr_ipc::ControlMessage::ReindexCommit {
            sha: "deadbeef".to_string(),
        })
        .unwrap();
        let (resp, _shutdown) = handle_control_message(
            &msg,
            &missing_repo,
            &store,
            &read_store,
            &cache,
            &phase_b_scheduler,
            &index_tx,
            &std::sync::Mutex::new(EditorPlane::default()),
        );
        assert!(
            !resp.ok,
            "#405: reindex must report failure when git is unavailable: {resp:?}"
        );
        let s = store.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            s.get_meta("last_commit").unwrap().is_none(),
            "#405: last_commit must NOT be stamped when the reindex did not happen"
        );
    }

    #[test]
    fn reindex_commit_skips_travsrignored_paths() {
        // #403: a commit touching only .travsrignore'd files must index nothing.
        // The hook path must apply the same ignore rules as init and the watcher
        // so vendored/generated files init excluded are not re-added on commit.
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        std::fs::write(tmp.path().join(".travsrignore"), "vendor/\n").unwrap();
        let vendor = tmp.path().join("vendor");
        std::fs::create_dir_all(&vendor).unwrap();
        std::fs::write(vendor.join("lib.ts"), "export class Lib { go() {} }").unwrap();
        StdCommand::new("git")
            .args(["add", "."])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        StdCommand::new("git")
            .args(["commit", "-q", "-m", "init"])
            .current_dir(tmp.path())
            .status()
            .unwrap();

        let db_path = tmp.path().join(".travsr/graph.db");
        std::fs::create_dir_all(tmp.path().join(".travsr")).unwrap();
        let mut store = travsr_store::SqliteStore::open(&db_path).unwrap();
        store
            .set_signature_format_version(travsr_core::SIGNATURE_FORMAT_VERSION)
            .unwrap();
        let store = std::sync::Mutex::new(store);
        let read_store = std::sync::Mutex::new(travsr_store::SqliteStore::open(&db_path).unwrap());
        let cache = std::sync::Mutex::new(query_cache::QueryCache::new(8));
        let phase_b_scheduler =
            phase_b_sched::PhaseBScheduler::new(std::time::Duration::from_secs(30));
        let (index_tx, _index_rx) = std::sync::mpsc::channel::<watcher::WatchEvent>();

        let msg = serde_json::to_string(&travsr_ipc::ControlMessage::ReindexCommit {
            sha: "deadbeef".to_string(),
        })
        .unwrap();
        let (resp, _shutdown) = handle_control_message(
            &msg,
            tmp.path(),
            &store,
            &read_store,
            &cache,
            &phase_b_scheduler,
            &index_tx,
            &std::sync::Mutex::new(EditorPlane::default()),
        );
        assert!(resp.ok, "control message must succeed: {resp:?}");

        let s = store.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            s.node_count().unwrap(),
            0,
            "#403: a .travsrignore'd vendored file must not be indexed by the hook path"
        );
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

    /// #464: an out-of-band graph.db writer (`fsck --fix` from a separate
    /// process/connection) never advances `last_commit`/`phase_b_commit`, so
    /// the warm query cache must key on the read connection's `data_version`
    /// to avoid serving pre-delete results indefinitely.
    #[test]
    #[cfg(any(unix, windows))]
    fn query_cache_invalidated_by_out_of_band_delete() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        // Three files so deleting one stays under the 50% mass-delete breaker.
        std::fs::write(
            tmp.path().join("prime.ts"),
            "export function isPrime(n: number): boolean { return n > 1; }",
        )
        .unwrap();
        std::fs::write(tmp.path().join("a.ts"), "export class Alpha { run() {} }").unwrap();
        std::fs::write(tmp.path().join("b.ts"), "export class Beta { run() {} }").unwrap();
        std::env::set_var("TRAVSR_DISABLE_REGISTRY", "1");
        init_repo(tmp.path()).unwrap();
        std::env::remove_var("TRAVSR_DISABLE_REGISTRY");

        let db_path = tmp.path().join(".travsr/graph.db");
        // The daemon's long-lived read connection (R5 #342).
        let read_store = SqliteStore::open_read_only(&db_path).unwrap();
        let mut cache = query_cache::QueryCache::new(8);

        let markers = |s: &SqliteStore| {
            (
                s.get_meta("last_commit").ok().flatten().unwrap_or_default(),
                s.get_meta("phase_b_commit")
                    .ok()
                    .flatten()
                    .unwrap_or_default(),
                query_cache::DataVersions {
                    graph: s.data_version().unwrap(),
                    embed: s.embed_data_version().unwrap(),
                },
            )
        };

        let args = serde_json::json!({ "query": "isPrime" });
        let (last, pb, dv) = markers(&read_store);
        let warm = run_query(&read_store, "ask", args.clone()).unwrap();
        assert_eq!(
            warm["matched"],
            serde_json::json!(true),
            "pre-delete ask must hit"
        );
        cache.put("ask", &args, &last, &pb, dv, warm.clone());

        // Out-of-band repair: delete the file on disk, then fsck --fix from a
        // separate connection — exactly the reproduction in #464.
        std::fs::remove_file(tmp.path().join("prime.ts")).unwrap();
        let report = fsck_repo(tmp.path(), true, false).unwrap();
        assert_eq!(report.ghost_paths, vec!["prime.ts".to_string()]);

        // Commit markers did not move…
        let (last2, pb2, dv2) = markers(&read_store);
        assert_eq!((last.as_str(), pb.as_str()), (last2.as_str(), pb2.as_str()));
        // …but data_version did, so the cached pre-delete entry stops matching.
        assert_ne!(
            dv.graph, dv2.graph,
            "out-of-band write must bump data_version"
        );
        assert!(
            cache.get("ask", &args, &last2, &pb2, dv2).is_none(),
            "stale warm-cache entry must not be served after fsck --fix"
        );
        // A fresh query through the same warm read connection sees the deletion.
        let fresh = run_query(&read_store, "ask", args).unwrap();
        assert_eq!(
            fresh["matched"],
            serde_json::json!(false),
            "deleted node must not resurface"
        );
    }
}

// ControlMessage and ControlResponse are now in travsr_ipc — no local defs needed.

/// The Travsr daemon — owns the file watcher, indexer worker, and control socket.
#[derive(Debug, Default)]
pub struct Daemon {
    _private: (),
}

/// Path of the startup-error breadcrumb the daemon writes when it fails to come
/// up (e.g. a control-socket bind failure). Because `daemon start` detaches the
/// child, a startup crash is otherwise silent; `daemon start` and `daemon
/// status` read this file so the failure is surfaced (travsr #592). Cleared on
/// a successful bind. Unix-only: Windows uses named pipes, which have no
/// SUN_LEN limit and bind synchronously in-process.
#[cfg(unix)]
fn startup_error_path(travsr_dir: &std::path::Path) -> std::path::PathBuf {
    travsr_dir.join("daemon-start.err")
}

#[cfg(unix)]
fn record_startup_error(travsr_dir: &std::path::Path, msg: &str) {
    let _ = std::fs::write(startup_error_path(travsr_dir), msg);
}

#[cfg(unix)]
fn clear_startup_error(travsr_dir: &std::path::Path) {
    let _ = std::fs::remove_file(startup_error_path(travsr_dir));
}

/// When the control socket lives outside the repo (the SUN_LEN fallback,
/// travsr #592), create and lock down its parent directory before binding.
///
/// Fail closed: the directory must be owned by the same uid that owns `.travsr`
/// and be mode 0700, so a hostile user on a shared `/tmp` cannot pre-create a
/// world-accessible directory and either block the daemon (squat the socket
/// name) or stand up a rogue listener the CLI would connect to. In-`.travsr`
/// sockets need no handling — travsr already created and owns that directory.
#[cfg(unix)]
fn prepare_fallback_socket_dir(
    travsr_dir: &std::path::Path,
    sock_path: &std::path::Path,
) -> anyhow::Result<()> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let Some(dir) = sock_path.parent() else {
        return Ok(());
    };
    if dir == travsr_dir {
        return Ok(());
    }
    let our_uid = std::fs::metadata(travsr_dir)
        .context("stat .travsr for socket-dir ownership check")?
        .uid();
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating control-socket dir {}", dir.display()))?;
    // Never operate through a symlink. `create_dir_all` silently succeeds when
    // the leaf already exists as a symlink to a directory; without this check the
    // set_permissions/metadata calls below would follow it and we could end up
    // binding through an attacker-chosen redirect. Reject a symlinked leaf before
    // we touch it. (The parent runtime base carries the sticky bit, so a
    // non-owner cannot swap this entry after the check.)
    let leaf_type = std::fs::symlink_metadata(dir)
        .with_context(|| format!("stat control-socket dir {}", dir.display()))?
        .file_type();
    anyhow::ensure!(
        !leaf_type.is_symlink(),
        "refusing to bind control socket: {} is a symlink",
        dir.display()
    );
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("locking down control-socket dir {}", dir.display()))?;
    let md = std::fs::metadata(dir)
        .with_context(|| format!("stat control-socket dir {}", dir.display()))?;
    anyhow::ensure!(
        md.uid() == our_uid,
        "refusing to bind control socket: {} is not owned by the current user",
        dir.display()
    );
    anyhow::ensure!(
        md.permissions().mode() & 0o777 == 0o700,
        "refusing to bind control socket: {} is not private (mode not 0700)",
        dir.display()
    );
    Ok(())
}

#[cfg(all(test, unix))]
mod fallback_socket_dir_tests {
    use super::prepare_fallback_socket_dir;
    use std::os::unix::fs::symlink;

    // #592 hardening: a symlinked leaf must be rejected so the daemon never
    // binds through an attacker-chosen redirect on a shared runtime base.
    #[test]
    fn rejects_symlinked_leaf() {
        let tmp = tempfile::tempdir().unwrap();
        let travsr_dir = tmp.path().join(".travsr");
        std::fs::create_dir_all(&travsr_dir).unwrap();
        let target = tmp.path().join("real-target");
        std::fs::create_dir_all(&target).unwrap();
        let leaf = tmp.path().join("travsr-uid");
        symlink(&target, &leaf).unwrap();
        let sock = leaf.join("daemon-deadbeef.sock");
        let err = prepare_fallback_socket_dir(&travsr_dir, &sock).unwrap_err();
        assert!(
            err.to_string().contains("symlink"),
            "expected symlink rejection, got: {err}"
        );
    }
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

        // #347/#348: drop old rotations before opening today's, so a long-lived
        // install cannot grow `.travsr/` without bound. `rolling::daily` never
        // deletes anything on its own.
        let pruned = logfile::prune(
            &travsr_dir,
            logfile::LOG_BUDGET_BYTES,
            logfile::MAX_LOG_FILES,
        );

        let file_appender = tracing_appender::rolling::daily(&travsr_dir, logfile::LOG_PREFIX);
        // Must be held for the daemon's lifetime — dropping flushes and closes
        // the log.
        //
        // The default buffer is 128,000 lines, which at INFO during a large
        // Phase A is tens of megabytes of resident memory sitting in a channel.
        // Bounded and lossy instead: under pressure the right thing to drop is
        // log lines, never indexing throughput. `non_blocking` reports what it
        // discarded, so the loss is visible rather than silent.
        let (non_blocking, _appender_guard) =
            tracing_appender::non_blocking::NonBlockingBuilder::default()
                .buffered_lines_limit(logfile::BUFFERED_LINES)
                .lossy(true)
                .finish(file_appender);
        use tracing_subscriber::layer::SubscriberExt as _;
        use tracing_subscriber::util::SubscriberInitExt as _;
        // INFO, not WARN. At WARN the file held nothing a user would want: on
        // this repo, four days of logs were 136 lines, every one of them the
        // same repeated warning and not one lifecycle event. `travsr daemon
        // logs` on top of that would have been a working feature showing
        // nothing. `RUST_LOG` still overrides in both directions.
        let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
        // JSON lines on disk. One line is one object, so every field is named
        // and typed rather than recovered by guessing at column positions, and
        // `jq`, Loki and Datadog all read it as-is. Nobody is asked to read JSON:
        // `travsr daemon logs` renders it back into columns for people, and
        // `--json` hands over the raw line for anything that would rather parse.
        //
        // `with_current_span` is on because the repo tag `--repo` filters by is
        // a span field, not an event field, and would be absent otherwise.
        // `with_span_list` is off: the full ancestry repeats the same few frames
        // on every line for no added information.
        let file_layer = tracing_subscriber::fmt::layer()
            .json()
            .with_current_span(true)
            .with_span_list(false)
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

        // First event in every session, so a rotated file is interpretable on
        // its own: which build wrote it, which repo, which process.
        tracing::info!(
            event = "daemon.session.start",
            version = build_version(),
            pid = std::process::id(),
            repo = %repo_root.display(),
            // No `foreground` field on purpose. A backgrounded daemon is a
            // re-exec of `daemon start --foreground`, so the flag is true in the
            // child either way: accurate for the process, and misleading to the
            // person who ran the background command and is told they did not.
            pruned_logs = pruned,
            "daemon starting"
        );

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

        // #592: resolve the control-socket path and, when it falls back outside
        // the repo (long-path case), create and permission-verify its directory
        // NOW — right after taking the lock, before the multi-second store open
        // and watcher scan. A fail-closed rejection then surfaces within the
        // `daemon start` confirmation window instead of after the daemon has
        // done all its startup work. Record the reason so the detached child is
        // not silent.
        #[cfg(unix)]
        let sock_path = travsr_ipc::ControlAddr::for_repo(&repo_root).socket_path(&travsr_dir);
        #[cfg(unix)]
        if let Err(e) = prepare_fallback_socket_dir(&travsr_dir, &sock_path) {
            record_startup_error(&travsr_dir, &format!("{e:#}"));
            return Err(e);
        }

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
        // #688: last diagnostics overlay reported by the editor. Daemon
        // lifetime only, so `travsr daemon lsp` after a restart correctly
        // reports that nothing has been received yet.
        let lsp_sessions = Arc::new(Mutex::new(EditorPlane::default()));

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
        let _ = std::fs::remove_file(&sock_path);

        // watcher::spawn() blocks until the initial kqueue/inotify scan is fully
        // established and .travsr/ is unwatched. Only then is it safe to create
        // the socket — otherwise kqueue opens the socket file and gets ENOTSUP,
        // crashing the entire watch setup.
        let _watcher_handle =
            watcher::spawn(&repo_root, tx.clone(), start_time).context("starting file watcher")?;

        // Control socket bound AFTER watcher scan completes (see above).
        // #592: on bind failure, record the reason before the detached child
        // exits so `daemon start`/`daemon status` can surface it; clear the
        // breadcrumb once we are actually listening.
        #[cfg(unix)]
        let mut listener = match UnixListener::bind(&sock_path).context("binding control socket") {
            Ok(l) => l,
            Err(e) => {
                record_startup_error(&travsr_dir, &format!("{e:#}"));
                return Err(e);
            }
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o600));
            clear_startup_error(&travsr_dir);
        }

        let mut gc_tick = tokio::time::interval(std::time::Duration::from_secs(300));
        // #621: single-flight guard for HEAD reconciliation. The gc_tick's
        // first firing is immediate, so the reconcile doubles as the startup
        // check; the flag stops a slow reconcile from overlapping the next tick.
        let head_reconcile_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        // #318 O3: poll the Phase B scheduler often enough to honour the debounce
        // window without busy-waiting. The tick is cheap (a single mutex peek);
        // it only spawns work when a re-run is actually due.
        let mut phase_b_tick = tokio::time::interval(std::time::Duration::from_secs(5));
        // Polls every 60 s to spawn embed Phase 2 once Phase 1 is complete.
        // Phase 2 must not overlap Phase 1 — both write node_embeddings in embed.db.
        let mut embed_tick = tokio::time::interval(std::time::Duration::from_secs(60));
        let embed_phase2_spawned = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut shutdown = std::pin::pin!(tokio::signal::ctrl_c());

        tracing::info!(
            event = "daemon.ready",
            repo = %repo_root.display(),
            "travsr daemon started"
        );
        #[cfg(unix)]
        tracing::info!(
            event = "daemon.socket.bound",
            transport = "unix",
            sock = %sock_path.display(),
            "control socket bound"
        );

        // Windows Named Pipe setup — resolved address for use in the accept task.
        #[cfg(windows)]
        let pipe_name = travsr_ipc::ControlAddr::for_repo(&repo_root).pipe_name();
        #[cfg(windows)]
        tracing::info!(
            event = "daemon.socket.bound",
            transport = "named_pipe",
            pipe = %pipe_name,
            "control pipe bound"
        );

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
            let lsp_sessions_win = Arc::clone(&lsp_sessions);
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
                    let lsp_sessions_conn = Arc::clone(&lsp_sessions_win);
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
                                        &lsp_sessions_conn,
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
                            let _ = writer.flush().await;
                            // #503: do not drop the pipe server until the
                            // client has read the response. Closing the handle
                            // (and DisconnectNamedPipe alike) discards bytes
                            // still unread in the pipe buffer, so a fast drop
                            // intermittently surfaced as "daemon closed
                            // connection without a response". The client
                            // closes its end right after reading, which we
                            // observe as EOF here; the timeout bounds a hung
                            // client and merely degrades to today's behavior.
                            let _ = tokio::time::timeout(
                                std::time::Duration::from_secs(2),
                                lines.next_line(),
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
                        let lsp_sessions_conn = Arc::clone(&lsp_sessions);
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
                                            &lsp_sessions_conn,
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
                        // #621: HEAD may have moved while the daemon was down
                        // (reset/checkout/rebase/pull) — the watcher never saw
                        // those edits, so the graph would stay pinned to a
                        // stale commit while reporting healthy. First tick is
                        // immediate → this is also the startup reconciliation.
                        if !head_reconcile_running.swap(true, std::sync::atomic::Ordering::AcqRel) {
                            let store_rc = Arc::clone(&store);
                            let repo_rc = Arc::clone(&repo_root_arc);
                            let sched_rc = Arc::clone(&phase_b_scheduler);
                            let index_tx_rc = index_tx.clone();
                            let flag = Arc::clone(&head_reconcile_running);
                            tokio::task::spawn_blocking(move || {
                                reconcile_head_drift(
                                    repo_rc.as_path(),
                                    &store_rc,
                                    &sched_rc,
                                    &index_tx_rc,
                                );
                                flag.store(false, std::sync::atomic::Ordering::Release);
                            });
                        }
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
                        // #621: see the unix gc_tick arm — startup + periodic
                        // reconciliation of last_commit against live HEAD.
                        if !head_reconcile_running.swap(true, std::sync::atomic::Ordering::AcqRel) {
                            let store_rc = Arc::clone(&store);
                            let repo_rc = Arc::clone(&repo_root_arc);
                            let sched_rc = Arc::clone(&phase_b_scheduler);
                            let index_tx_rc = index_tx.clone();
                            let flag = Arc::clone(&head_reconcile_running);
                            tokio::task::spawn_blocking(move || {
                                reconcile_head_drift(
                                    repo_rc.as_path(),
                                    &store_rc,
                                    &sched_rc,
                                    &index_tx_rc,
                                );
                                flag.store(false, std::sync::atomic::Ordering::Release);
                            });
                        }
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
        tracing::info!(event = "daemon.session.stop", "travsr daemon stopped");
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
             excess deferred to semantic analysis / next reconcile"
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

/// Everything the editor plane holds, under one lock.
///
/// The refusal tally lives beside the sessions rather than in its own mutex
/// because the two are always touched together and a second lock would only
/// add an ordering to get wrong.
#[derive(Debug, Default)]
pub struct EditorPlane {
    pub sessions: std::collections::HashMap<String, EditorSession>,
    /// Reported repo root → how many reports naming it were refused.
    ///
    /// A refused report is otherwise silent on both ends: the daemon does not
    /// log it (correctly, at a per-keystroke rate) and the client is
    /// fire-and-forget, so `send` reports success when the *write* succeeded
    /// even if every daemon on the machine dropped the report. A persistent
    /// mismatch — a multi-root workspace whose first folder is not this repo,
    /// or a window opened on a subdirectory — then reads exactly like "no
    /// editor attached", which is also what a closed panel and an uninstalled
    /// extension look like. Counting makes the drop self-explaining without
    /// costing a log line (#698 review, P3).
    /// Distinct roots retained. Past this the identity of the root has stopped
    /// being the useful part and the fact that reports are being refused at
    /// all is, so further roots fold into `refused_overflow` rather than
    /// growing the map (#698 review, P2).
    ///
    /// Bounded for the same reason `MAX_EDITOR_SESSIONS` is: the control
    /// socket is reachable by any process running as this user, and this
    /// variant is parsed before any other limit applies, so an unbounded map
    /// keyed on a caller-supplied string is a way to grow the daemon without
    /// limit. The key is the *normalized* root, so `/repo`, `/repo/` and a
    /// symlinked spelling collapse to one entry instead of three.
    pub refused: std::collections::HashMap<String, usize>,
    /// Refusals whose root arrived after `MAX_REFUSED_ROOTS` was reached.
    pub refused_overflow: usize,
}

/// Distinct refused roots retained per daemon lifetime. Small because the
/// message it feeds is diagnostic: a handful of roots names the problem, and a
/// wall of them is noise.
pub const MAX_REFUSED_ROOTS: usize = 16;

/// #688: what one editor window currently sees as broken.
///
/// Held only in memory and only while its lease is alive. Nothing here is ever
/// written to the graph: the graph must stay reproducible from the repository,
/// and this depends on which extensions a developer installed and on unsaved
/// buffers. Two planes, each true about its own thing.
#[derive(Debug, Clone)]
pub struct EditorSession {
    /// Repo-relative path → (errors, warnings). Only broken files are present.
    pub files: std::collections::HashMap<String, (usize, usize)>,
    /// Files the editor examined for this report.
    pub seen: usize,
    /// Of `seen`, how many no provider reported on, so are unknown not clean.
    pub undiagnosed: usize,
    /// When this report stops being believable.
    ///
    /// `Instant`, not `SystemTime` (#698 review, P3): a lease measures elapsed
    /// time, and `SystemTime` is not monotonic on any platform the daemon runs
    /// on. An NTP step, a laptop suspend/resume, or a manual clock change
    /// moves it, which expires every live session at once (forwards) or pins a
    /// closed window's view well past its lease (backwards). `Instant` is
    /// monotonic everywhere: QPC on Windows, `CLOCK_MONOTONIC` on Linux,
    /// `mach_absolute_time` on macOS. It also makes the derived JSON fields
    /// total rather than fallible, so no arm has to guess with `unwrap_or(0)`.
    pub expires_at: std::time::Instant,
    pub updated_at: std::time::Instant,
}

/// Editors tracked at once. An editor is a person's open window, so the real
/// number is one or two; the cap exists because this plane is fed from outside
/// the daemon and unbounded external input is how a daemon dies.
const MAX_EDITOR_SESSIONS: usize = 8;
/// Broken files retained per session. Past this the answer is "the build is
/// broken", which no longer needs a file list.
const MAX_FILES_PER_SESSION: usize = 500;
/// Ceiling on a caller-supplied lease, so a bad or hostile client cannot pin a
/// stale view in memory for a week.
const MAX_LEASE_SECS: u64 = 3600;

/// Live sessions, expired ones dropped.
///
/// Expiry is evaluated on read rather than on a timer: there is no work to do
/// when a lease ends, only a fact to stop asserting, and a reader is the only
/// one who can be misled by it.
fn live_editor_sessions(
    sessions: &std::collections::HashMap<String, EditorSession>,
) -> Vec<(&String, &EditorSession)> {
    let now = std::time::Instant::now();
    let mut live: Vec<_> = sessions
        .iter()
        .filter(|(_, s)| s.expires_at > now)
        .collect();
    live.sort_by_key(|(_, s)| std::cmp::Reverse(s.updated_at));
    live
}

/// Returns `(response, should_shutdown)`.
///
/// Called from the Unix domain-socket accept loop and the Windows Named Pipe
/// accept loop. Gated to the two supported control-plane platforms so the
/// compiler does not emit dead_code on exotic targets.
#[cfg(any(unix, windows))]
#[allow(clippy::too_many_arguments)]
fn handle_control_message(
    line: &str,
    repo_root: &std::path::Path,
    store: &std::sync::Mutex<SqliteStore>,
    read_store: &std::sync::Mutex<SqliteStore>,
    cache: &std::sync::Mutex<query_cache::QueryCache>,
    phase_b_scheduler: &phase_b_sched::PhaseBScheduler,
    index_tx: &std::sync::mpsc::Sender<watcher::WatchEvent>,
    lsp_sessions: &std::sync::Mutex<EditorPlane>,
) -> (travsr_ipc::ControlResponse, bool) {
    use travsr_ipc::{ControlMessage, ControlResponse};

    match serde_json::from_str::<ControlMessage>(line) {
        Ok(ControlMessage::ReindexCommit { sha }) => {
            tracing::info!(sha=%sha, "control: reindex-commit");
            let mut paths = match changed_files_from_git(repo_root) {
                Ok(p) => p,
                Err(e) => {
                    // #405: the commit diff is unavailable. Fall back to the
                    // full tracked set (reindex_files is hash-delta gated, so
                    // this is cheap on an already-fresh graph) instead of
                    // silently reindexing nothing and claiming success.
                    tracing::warn!(err=%e, "changed_files_from_git failed — falling back to full tracked-file reindex");
                    match tracked_files_from_git(repo_root) {
                        Ok(p) => p,
                        Err(e2) => {
                            // Both git paths failed: report the failure so
                            // last_commit is NOT stamped and the hook-side
                            // caller learns the reindex did not happen.
                            tracing::warn!(err=%e2, "tracked-file fallback also failed — reindex skipped");
                            return (
                                ControlResponse::err(format!(
                                    "reindex-commit could not enumerate files: {e2}"
                                )),
                                false,
                            );
                        }
                    }
                }
            };
            // #403: apply the same ignore rules as init and the watcher so a
            // commit touching .travsrignore'd paths (vendor/, **/generated/, …)
            // does not reintroduce the ghost nodes init excluded.
            let ignore = watcher::build_ignore_matcher(repo_root);
            paths.retain(|p| !watcher::should_skip_all(p, repo_root, &ignore));
            let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
            let dirty = match reindex_files(&paths, repo_root, &mut s) {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(err=%e, "control reindex failed");
                    return (ControlResponse::err(e.to_string()), false);
                }
            };
            // Stamp last_commit unconditionally, independent of whether any
            // file actually changed — mirrors the init_repo fix (PR #207) for
            // the same any_changed-suppression regression: a commit whose
            // files were already reindexed by the live watcher before the
            // hook ran (the common editor-save-then-commit flow) must still
            // advance last_commit, or `travsr status` reports stale forever.
            // Skip only when reindex_files itself skipped everything due to a
            // signature-format mismatch (graph.db needs `travsr init`), so
            // this never claims freshness for a commit that was never
            // actually reindexed.
            if s.get_signature_format_version().ok() == Some(SIGNATURE_FORMAT_VERSION) {
                let _ = s.set_meta("last_commit", &sha);
            }
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
        Ok(ControlMessage::ReportLspDiagnostics {
            repo_root: reported_root,
            session,
            ttl_secs,
            files,
            seen,
            undiagnosed,
        }) => {
            // #698 review P1: the client cannot tell which daemon owns which
            // socket, so it sends to every candidate and identity is settled
            // here. A report for another repo is dropped rather than answered:
            // its paths are keys into a different graph, and accepting it
            // would make `travsr daemon lsp` quote files this repo does not
            // have. Normalized through the same helper the socket name is
            // derived from, so the two cannot disagree about identity.
            let reported = travsr_ipc::normalize_repo_root(std::path::Path::new(&reported_root));
            if reported != travsr_ipc::normalize_repo_root(repo_root) {
                // Counted, not logged: at a per-keystroke rate a log line would
                // be noise, but a silent drop leaves a persistent mismatch with
                // no thread to pull. `travsr daemon lsp` surfaces the tally.
                //
                // Keyed on the normalized root, the same form the comparison
                // above uses, so the printed message cannot read "named X but
                // this daemon serves X" for a symlinked or trailing-slash
                // spelling of one directory, which is the exact case the tally
                // exists to explain. Capped so a caller cannot grow the map
                // without limit (#698 review, P2).
                let mut plane = lsp_sessions.lock().unwrap_or_else(|e| e.into_inner());
                if plane.refused.len() < MAX_REFUSED_ROOTS || plane.refused.contains_key(&reported)
                {
                    *plane.refused.entry(reported).or_insert(0) += 1;
                } else {
                    plane.refused_overflow += 1;
                }
                return (
                    ControlResponse::err("report is for a different repo".to_string()),
                    false,
                );
            }

            let mut plane = lsp_sessions.lock().unwrap_or_else(|e| e.into_inner());
            let sessions = &mut plane.sessions;
            let now = std::time::Instant::now();

            // ttl 0 is an explicit detach: the window is closing and says so,
            // rather than leaving its view to rot until the lease runs out.
            if ttl_secs == 0 {
                if sessions.remove(&session).is_some() {
                    tracing::info!(
                        event = "editor.detached",
                        session = %session,
                        "editor detached"
                    );
                }
                return (ControlResponse::ok(None), false);
            }

            // Attach and detach are lifecycle facts and belong in the log. The
            // reports in between are a value that changes, which is what the
            // plane is for; logging each one would bury the daemon's own story
            // under an editor's typing.
            let known = sessions.contains_key(&session);
            if !known {
                if sessions.len() >= MAX_EDITOR_SESSIONS {
                    // Drop the least recently updated rather than reject the
                    // newcomer: the stalest view is the least likely to be true.
                    if let Some(oldest) = sessions
                        .iter()
                        .min_by_key(|(_, s)| s.updated_at)
                        .map(|(k, _)| k.clone())
                    {
                        sessions.remove(&oldest);
                        tracing::info!(
                            event = "editor.detached",
                            session = %oldest,
                            reason = "evicted",
                            "editor evicted, session cap reached"
                        );
                    }
                }
                tracing::info!(
                    event = "editor.attached",
                    session = %session,
                    "editor attached"
                );
            }

            let mut map = std::collections::HashMap::new();
            for f in files.into_iter().take(MAX_FILES_PER_SESSION) {
                map.insert(f.path, (f.errors, f.warnings));
            }
            let lease = std::time::Duration::from_secs(ttl_secs.min(MAX_LEASE_SECS));
            sessions.insert(
                session,
                EditorSession {
                    files: map,
                    seen,
                    undiagnosed,
                    expires_at: now + lease,
                    updated_at: now,
                },
            );
            (ControlResponse::ok(None), false)
        }
        Ok(ControlMessage::LspStatus) => {
            let mut plane = lsp_sessions.lock().unwrap_or_else(|e| e.into_inner());
            // Reading is the only moment an expired lease can mislead anyone,
            // so it is also where they are cleared.
            let now = std::time::Instant::now();
            plane.sessions.retain(|_, s| s.expires_at > now);

            // Reported roots this daemon refused, so a caller seeing "no editor
            // attached" can tell "nothing reported" from "everything reported
            // named a repo I do not serve".
            let mut refused: Vec<_> = plane
                .refused
                .iter()
                .map(|(root, n)| serde_json::json!({ "repo_root": root, "count": n }))
                .collect();
            refused.sort_by(|a, b| a["repo_root"].as_str().cmp(&b["repo_root"].as_str()));

            let live = live_editor_sessions(&plane.sessions);
            let payload = serde_json::json!({
                "editors": live.len(),
                // Normalized, matching the keys above: an unnormalized pair
                // could print two spellings of one directory as if they were
                // different repos.
                "served_repo_root": travsr_ipc::normalize_repo_root(repo_root),
                "refused": refused,
                "refused_overflow": plane.refused_overflow,
                "sessions": live
                    .iter()
                    .map(|(id, s)| {
                        let mut broken: Vec<_> = s
                            .files
                            .iter()
                            .map(|(path, (errors, warnings))| {
                                serde_json::json!({
                                    "path": path,
                                    "errors": errors,
                                    "warnings": warnings,
                                })
                            })
                            .collect();
                        // Stable order so repeated calls do not shuffle, since
                        // a HashMap iterates differently every time.
                        broken.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
                        serde_json::json!({
                            "session": id,
                            "seen": s.seen,
                            "undiagnosed": s.undiagnosed,
                            // Total, not fallible: `Instant` arithmetic cannot
                            // run backwards, so there is no error case to
                            // guess at with `unwrap_or(0)` (#698 review, P3).
                            "age_secs": now.duration_since(s.updated_at).as_secs(),
                            "expires_in_secs": s
                                .expires_at
                                .saturating_duration_since(now)
                                .as_secs(),
                            "broken": broken,
                        })
                    })
                    .collect::<Vec<_>>(),
            });
            (ControlResponse::query_result(payload), false)
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
                "complete".to_string()
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
                 semantic: {phase_b_activity}\n\
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
            let started = std::time::Instant::now();
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
            // #464: also key on the SQLite data_version of graph.db (and of
            // the sibling embed.db, which `ask` depends on) so out-of-band
            // writers (fsck --fix, manual sqlite3, embed reindex) that never
            // bump the commit markers still invalidate cached results.
            // Read once and reuse for the put: if a DB changes mid-query, the
            // entry is stamped with the pre-change version and simply stops
            // matching on the next lookup — conservative, never stale.
            // On a pragma failure the cache is bypassed entirely (no lookup,
            // no store) rather than collapsing to a sentinel value: a sentinel
            // could collide across two consecutive errors bracketing a
            // mutation and serve one stale hit.
            // Only `ask` reads embed.db (KNN + RFC-019 cosine oracle) — keying
            // graph/status entries on embed state would let the embed
            // sidecar's batched reindex writes thrash unrelated cache entries.
            let embed_version = if tool == "ask" {
                s.embed_data_version()
            } else {
                Ok(None)
            };
            let versions = match (s.data_version(), embed_version) {
                (Ok(graph), Ok(embed)) => Some(query_cache::DataVersions { graph, embed }),
                (graph, embed) => {
                    let err = graph
                        .err()
                        .map(|e| e.to_string())
                        .or_else(|| embed.err().map(|e| e.to_string()))
                        .unwrap_or_default();
                    tracing::warn!(error = %err, "data_version pragma failed — bypassing query cache");
                    None
                }
            };
            if let Some(versions) = versions {
                let mut c = cache.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(cached) = c.get(&tool, &args, &last_commit, &phase_b_commit, versions) {
                    tracing::info!(
                        event = "query.served",
                        tool = %tool,
                        cached = true,
                        elapsed_ms = started.elapsed().as_millis(),
                        "query served"
                    );
                    return (ControlResponse::query_result(cached), false);
                }
            }
            match run_query(&s, &tool, args.clone()) {
                Ok(value) => {
                    if let Some(versions) = versions {
                        let mut c = cache.lock().unwrap_or_else(|e| e.into_inner());
                        c.put(
                            &tool,
                            &args,
                            &last_commit,
                            &phase_b_commit,
                            versions,
                            value.clone(),
                        );
                    }
                    // The line that makes "which query was slow" answerable.
                    // Without it a successful query logged nothing at all, so
                    // the `req` correlation id had nothing on the happy path to
                    // bind to and per-request timing did not exist. `cached`
                    // distinguishes the two costs, which is usually the first
                    // thing worth knowing about a slow one.
                    tracing::info!(
                        event = "query.served",
                        tool = %tool,
                        cached = false,
                        elapsed_ms = started.elapsed().as_millis(),
                        "query served"
                    );
                    (ControlResponse::query_result(value), false)
                }
                Err(e) => {
                    tracing::warn!(
                        event = "query.failed",
                        tool = %tool,
                        elapsed_ms = started.elapsed().as_millis(),
                        error = %e,
                        "query failed"
                    );
                    (ControlResponse::err(format!("query failed: {e:#}")), false)
                }
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

/// Resolve the embed backend id to use for hook injection at `db_path`,
/// preferring the repo's own `.travsr/embed.toml` override over the
/// machine-wide `~/.travsr/embed.toml` default. Mirrors `resolve_backend`'s
/// resolution order in travsr-plugin-host — hook injection is a per-repo
/// embedding decision, so it must not resolve on `active_backend_id()` alone
/// (#526: a repo-level `travsr embed switch` was silently ignored, causing
/// the daemon to spawn the wrong sidecar model and disable Step 4).
fn hook_backend_id(db_path: &Path) -> Option<String> {
    use travsr_plugin_host::{active_backend_id, repo_backend_id};

    let repo_root = db_path.parent().and_then(|p| p.parent());
    repo_root
        .and_then(repo_backend_id)
        .or_else(active_backend_id)
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
    use travsr_plugin_host::{embed_backends, lookup_embed_backend, EmbedSupervisor};

    let Some(home) = dirs::home_dir() else {
        tracing::debug!("embed hook: HOME not set — skipping");
        return;
    };

    // Prefer the repo's own `.travsr/embed.toml` override, then the user's
    // machine-wide active backend from ~/.travsr/embed.toml, then the catalog
    // default so a fresh install without `travsr embed switch` still works.
    // Mirrors `resolve_backend`'s resolution order (travsr-plugin-host) — this
    // is a per-repo embedding decision, not an install/list/hint path, so it
    // must not resolve on `active_backend_id()` alone (#526).
    let backend = hook_backend_id(db_path)
        .as_deref()
        .and_then(lookup_embed_backend)
        .or_else(|| embed_backends().first())
        .cloned();
    let Some(backend) = backend else {
        return;
    };

    let binary = home
        .join(".travsr")
        .join("bin")
        .join(backend.binary_filename());
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
        // #376 §4.4: arm the doc-space KNN hook beside the code one. The doc
        // index is a separate HNSW space (`hnsw-docs.usearch`), so it needs its
        // own hook — the code hook cannot answer a doc query and `VecIndex::knn`
        // has no metadata filter to partition one space into two.
        //
        // Without this, `travsr ask` could never render a docs section no matter
        // how the lane was configured: the MCP server arms this hook in
        // `travsr_mcp::inject_embed_hook`, but the daemon (which is what answers
        // every CLI `ask`) did not, so `store.embed_doc_knn_fn()` was always
        // `None` on that route. `None` is indistinguishable from "sidecar too
        // old" or "repo has no doc-chunk nodes", which is why it failed silently.
        if let Some(doc_hook) = supervisor.doc_knn_hook(model_id.clone()) {
            query_store.set_embed_doc_knn_hook(doc_hook);
        }
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
