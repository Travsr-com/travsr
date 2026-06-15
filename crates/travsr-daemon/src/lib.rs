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
use travsr_core::{canonical_corpus, canonical_corpus_local, Language, SIGNATURE_FORMAT_VERSION};
use travsr_indexer::{
    hash_file, ingest_lsif, link_imports, link_imports_go, link_imports_python_fs,
    link_imports_rust, run_lsif_emitter, FfiMarker,
};
use travsr_plugin_host::PluginIndexer;
use travsr_store::{BatchWriteCounts, FileGraph, SqliteStore, Store};

pub use hook::{changed_files_from_git, install_hook, try_dispatch_to_daemon};

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
    /// Languages whose analyzer spawned but died or errored mid-invoke.
    pub crashed: Vec<String>,
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

/// Heuristic: a single directory holding ≥ 1 000 source-language files AND
/// ≥ 15 % of the total discovered source files is flagged as a "large dep dir".
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

    // Register in global registry so `travsr mcp --global` can find this repo.
    // TRAVSR_DISABLE_REGISTRY=1 bypasses registration — set in tests and CI to
    // prevent temp-dir paths polluting ~/.travsr/registry.json.
    if std::env::var("TRAVSR_DISABLE_REGISTRY").as_deref() != Ok("1") {
        let repo_name = repo_root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");
        if let Err(e) = travsr_store::registry::register(repo_name, &db_path) {
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
    let corpus = detect_corpus(repo_root);
    store
        .set_meta("corpus", &corpus)
        .context("writing corpus to meta (ARCH-102)")?;
    tracing::debug!("corpus for {}: {corpus}", repo_root.display());

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
    // Restored unconditionally after indexing (success or error path below).
    store
        .set_bulk_init_mode(true)
        .context("enabling bulk init mode")?;
    store
        .begin_bulk_fts_tracking()
        .context("creating bulk FTS tracking table")?;

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

    let (batch_counts, files_skipped_unchanged) = index_result?;

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
            let phase_b_indexer = travsr_plugin_host::PluginIndexer::new(&corpus);
            let inputs = travsr_plugin_host::PhaseBInputs {
                repo_root,
                present_languages: present_languages.clone(),
                // P6 (#329): reuse the already-walked file list so Phase B runners
                // skip their own directory walks.
                indexable_paths: &indexable_paths,
            };
            let (pb_nodes, pb_edges, pb_refs, pb_outcome) =
                phase_b_indexer.invoke_phase_b_all(&inputs);
            write_phase_b_results(&mut store, &corpus, pb_nodes, pb_edges, pb_refs, pb_outcome)
        };
        tracing::info!(
            elapsed_ms = t_phase_b.elapsed().as_millis(),
            "TIMING: invoke_phase_b_all done"
        );
        Some(report)
    } else if phase_b_already_done {
        // Phase B is current for this commit — nothing to do, no message.
        // Return Some(empty) so init.rs skips the daemon spawn.
        Some(PhaseBReport {
            ran: vec![],
            crashed: vec![],
            skipped_not_in_repo: vec![],
            skipped_no_analyzer: vec![],
            skipped_unregistered: vec![],
        })
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
        // phase_b_commit is stamped only when Phase B ran inline. On the
        // deferred path we leave it absent so the daemon's phase_b_tick
        // auto-arms the scheduler when it opens the store.
        if run_phase_b_inline {
            let _ = store.set_meta("phase_b_commit", &sha);
        }
    }

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
    PhaseBReport {
        ran: pb_outcome.ran,
        skipped_not_in_repo: pb_outcome.skipped_not_in_repo,
        skipped_no_analyzer: pb_outcome.skipped_no_analyzer,
        skipped_unregistered: pb_outcome.skipped_unregistered,
        crashed: pb_outcome.crashed,
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
            langs.insert(lang.as_str().to_string());
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
    let indexer = travsr_plugin_host::PluginIndexer::new(&corpus);
    let inputs = travsr_plugin_host::PhaseBInputs {
        repo_root,
        present_languages,
        indexable_paths: &indexable_paths,
    };
    let (pb_nodes, pb_edges, pb_refs, pb_outcome) = indexer.invoke_phase_b_all(&inputs);

    // ── Single write batch under the lock ─────────────────────────────────────
    let mut s = store.lock().unwrap_or_else(|e| e.into_inner());

    // Write LSIF edges first (pre-collected lock-free above).
    for edge in &lsif_edges {
        if let Err(e) = s.put_edge_lsif(edge) {
            tracing::warn!("lsif edge write error: {e}");
        }
    }

    let report = write_phase_b_results(&mut s, &corpus, pb_nodes, pb_edges, pb_refs, pb_outcome);
    let _ = s.set_meta("phase_b_commit", &target_sha);

    let succeeded = !report.ran.is_empty() || !lsif_edges.is_empty() || report.crashed.is_empty();

    tracing::info!(
        commit = %target_sha,
        ran = report.ran.len(),
        lsif_edges = lsif_edges.len(),
        crashed = report.crashed.len(),
        "background phase B refresh complete"
    );

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

    let mut indexer = PluginIndexer::new(&corpus);
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

        let _watcher_handle =
            watcher::spawn(&repo_root, tx.clone(), start_time).context("starting file watcher")?;

        // Control socket — Unix domain socket at .travsr/daemon-<hex>.sock (Unix only).
        #[cfg(unix)]
        let listener = UnixListener::bind(&sock_path).context("binding control socket")?;
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

        // Windows: spawn named pipe accept loop. Runs until the runtime is shut
        // down or until the accept fails (which happens when the daemon exits).
        #[cfg(windows)]
        {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
            use tokio::net::windows::named_pipe::ServerOptions;

            let store_win = Arc::clone(&store);
            let repo_win = Arc::clone(&repo_root_arc);
            let sd_win = Arc::clone(&pipe_shutdown);
            let cache_win = Arc::clone(&query_cache);
            let sched_win = Arc::clone(&phase_b_scheduler);
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
                    let repo = Arc::clone(&repo_win);
                    let sd = Arc::clone(&sd_win);
                    let cache = Arc::clone(&cache_win);
                    let sched = Arc::clone(&sched_win);
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
                                        &cache,
                                        &sched,
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
                        let repo = Arc::clone(&repo_root_arc);
                        let sd_notify = Arc::clone(&sock_shutdown);
                        let cache = Arc::clone(&query_cache);
                        let sched = Arc::clone(&phase_b_scheduler);
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
                                            &cache,
                                            &sched,
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
                        // Auto-arm when Phase B is pending (deferred init, or daemon
                        // restarted after a crash mid-Phase-B).
                        arm_phase_b_if_pending(&store, &phase_b_scheduler);
                        // #318 O3: start a background Phase B refresh iff one is
                        // due (armed + past debounce) and none is already running.
                        if phase_b_scheduler.try_claim() {
                            let store_bg = Arc::clone(&store);
                            let repo_bg = Arc::clone(&repo_root_arc);
                            let sched_bg = Arc::clone(&phase_b_scheduler);
                            tokio::task::spawn_blocking(move || {
                                run_background_phase_b(repo_bg.as_path(), &store_bg, &sched_bg);
                            });
                        }
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
                        // Auto-arm when Phase B is pending (deferred init, or daemon
                        // restarted after a crash mid-Phase-B).
                        arm_phase_b_if_pending(&store, &phase_b_scheduler);
                        // #318 O3: start a background Phase B refresh iff one is
                        // due (armed + past debounce) and none is already running.
                        if phase_b_scheduler.try_claim() {
                            let store_bg = Arc::clone(&store);
                            let repo_bg = Arc::clone(&repo_root_arc);
                            let sched_bg = Arc::clone(&phase_b_scheduler);
                            tokio::task::spawn_blocking(move || {
                                run_background_phase_b(repo_bg.as_path(), &store_bg, &sched_bg);
                            });
                        }
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
///
/// Called from the Unix domain-socket accept loop and the Windows Named Pipe
/// accept loop. Gated to the two supported control-plane platforms so the
/// compiler does not emit dead_code on exotic targets.
#[cfg(any(unix, windows))]
fn handle_control_message(
    line: &str,
    repo_root: &std::path::Path,
    store: &std::sync::Mutex<SqliteStore>,
    cache: &std::sync::Mutex<query_cache::QueryCache>,
    phase_b_scheduler: &phase_b_sched::PhaseBScheduler,
) -> (travsr_ipc::ControlResponse, bool) {
    use travsr_ipc::{ControlMessage, ControlResponse};

    match serde_json::from_str::<ControlMessage>(line) {
        Ok(ControlMessage::ReindexCommit { sha }) => {
            tracing::info!(sha=%sha, "control: reindex-commit");
            let paths = changed_files_from_git(repo_root).unwrap_or_default();
            let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(e) = reindex_files(&paths, repo_root, &mut s) {
                tracing::warn!(err=%e, "control reindex failed");
                return (ControlResponse::err(e.to_string()), false);
            }
            drop(s);
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
            if let Err(e) = reindex_files(&paths, repo_root, &mut s) {
                return (ControlResponse::err(e.to_string()), false);
            }
            (ControlResponse::ok(None), false)
        }
        Ok(ControlMessage::Status) => (ControlResponse::ok(Some("running".to_string())), false),
        Ok(ControlMessage::Shutdown) => (ControlResponse::ok(None), true),
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
            let s = store.lock().unwrap_or_else(|e| e.into_inner());
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
            Ok(serde_json::to_value(query::ask_query(store, q)?)?)
        }
        "status" => Ok(serde_json::to_value(query::status_query(store)?)?),
        other => anyhow::bail!("unknown query tool '{other}'"),
    }
}
