use crate::cache::{CacheKey, ParseCache};
use crate::dispatcher::Dispatcher;
use crate::plugins::response_to_output;
use crate::registry::register_builtins;
use crate::resolver::PluginResolver;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use travsr_core::Language;
use travsr_error::IndexError;
use travsr_indexer::{hash_file, ParseOutput};

/// Per-language Phase B outcome reported by [`PluginIndexer::invoke_phase_b_all`].
#[derive(Debug, Default, Clone)]
pub struct PhaseBOutcome {
    pub ran: Vec<String>,
    /// Languages P1-gated out because no source files of that type exist in the
    /// repo. Silent — no user-facing message needed (missing Java files in a
    /// TypeScript repo is not an actionable finding).
    pub skipped_not_in_repo: Vec<String>,
    /// Languages present in the repo but for which no semantic analyzer was
    /// found or the analyzer declined the request. User-actionable:
    /// `travsr lang add <lang>`.
    pub skipped_no_analyzer: Vec<String>,
    pub skipped_unregistered: Vec<String>,
    /// Languages registered as RequiresElevated but lacking a PSE approval
    /// entry in lang.toml. User-actionable: `travsr lang approve <lang>`.
    pub skipped_needs_approval: Vec<String>,
    /// Languages whose analyzer was found and spawned but died or errored
    /// mid-invoke.
    pub crashed: Vec<String>,
    /// Languages whose sidecar binary responded with a mismatched protocol
    /// version. User-actionable: `travsr lang install <lang>` to upgrade.
    pub version_mismatch: Vec<(String, u32, u32)>,
}

/// Inputs for [`PluginIndexer::invoke_phase_b_all`].
///
/// `present_languages` gates Phase B to only the languages that actually appear
/// in the repo's source tree (P1 — #322). An **empty** set means "no gating —
/// run every language the resolver provides" (used by call sites that do not
/// have a prior walk, e.g. `run_background_phase_b` before it gains its own
/// walk result).
///
/// `indexable_paths` carries the daemon's pre-walked absolute file paths so
/// Phase B runners don't need to re-walk the directory tree (P6 — #329).
/// An **empty** slice means "not available" — runners fall back to their own walk.
pub struct PhaseBInputs<'a> {
    pub repo_root: &'a Path,
    /// Canonical resolver name strings (`Language::as_str()`) of languages
    /// detected during the Phase A file walk.
    pub present_languages: HashSet<String>,
    /// All indexable absolute paths from the daemon's Phase A walk.
    /// Partitioned by language extension inside `invoke_phase_b_all` and
    /// forwarded via `InvokeRequest.files` (P6 — #329).
    pub indexable_paths: &'a [PathBuf],
}

/// Drop-in replacement for travsr_indexer::Indexer.
/// Routes files through the plugin Dispatcher, caches results by
/// (plugin_version, sha256(file)) — both daemon-computed per ADR-017 Rule 5.
pub struct PluginIndexer {
    pub corpus: String,
    dispatcher: Dispatcher,
    cache: ParseCache,
}

impl PluginIndexer {
    pub fn new(corpus: impl Into<String>) -> Self {
        let corpus = corpus.into();
        let mut dispatcher = Dispatcher::new(&corpus);
        register_builtins(&mut dispatcher);
        Self {
            corpus,
            dispatcher,
            cache: ParseCache::new(),
        }
    }

    /// Parse a file. Caches by (CARGO_PKG_VERSION, sha256). Returns ParseOutput
    /// so the daemon's existing call sites need no changes.
    pub fn parse_file_with_vname(
        &mut self,
        abs_path: &Path,
        vname_path: &str,
    ) -> Result<ParseOutput, IndexError> {
        let file_hash = hash_file(abs_path).map_err(|e| IndexError::Parse {
            file: abs_path.display().to_string(),
            message: e.to_string(),
        })?;
        let version = env!("CARGO_PKG_VERSION");
        let key = CacheKey {
            plugin_version: version.to_string(),
            file_hash,
        };

        // Cache hit
        if let Some(cached) = self.cache.get(version, file_hash) {
            return Ok(response_to_output(cached.clone()));
        }

        // Cache miss: dispatch through plugin
        let corpus = self.dispatcher.corpus.clone();
        let resp = match self
            .dispatcher
            .parse_file(abs_path, vname_path, &corpus, "")?
        {
            Some(r) => r,
            None => return Ok(ParseOutput::default()),
        };

        self.cache.insert(key, resp.clone());
        Ok(response_to_output(resp))
    }

    /// Phase B: semantic indexing for all registered languages.
    /// Called from `init_repo` once per full index — not per commit.
    ///
    /// Returns `(nodes, structural_edges, scip_refs, outcome)`.
    /// `scip_refs` is non-empty when at least one language sidecar supports G2
    /// attribution (i.e. returns `InvokeResponse.refs`). The daemon uses
    /// `write_scip_attributed_batch` when refs are present, falling back to
    /// `write_phase_b_batch` for older sidecars.
    ///
    /// # P1 — language gating
    /// Languages absent from `inputs.present_languages` are skipped before any
    /// sidecar process is spawned. An empty `present_languages` set disables
    /// gating (run all).
    ///
    /// # P2 — parallel execution + P7 — single write path
    /// Per-language runs are fanned out via `std::thread::scope` so a 3-language
    /// repo pays `max(t_lang)` not `Σ t_lang`. Threads never touch the store;
    /// results are merged here and returned as a single batch for the caller's
    /// `unify_all` → `write_scip_attributed_batch` path (Option A, #322 P7).
    pub fn invoke_phase_b_all(
        &self,
        inputs: &PhaseBInputs<'_>,
    ) -> (
        Vec<travsr_core::Node>,
        Vec<travsr_core::Edge>,
        Vec<travsr_core::ScipRef>,
        PhaseBOutcome,
    ) {
        let repo_root = inputs.repo_root;

        // Gate Phase B per language against lang.toml registration.
        // `travsr lang remove <lang>` writes registered=[] which must be respected here.
        let registered: HashSet<String> = crate::trust::registered_languages_from_disk()
            .into_iter()
            .collect();

        let current_exe = std::env::current_exe()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let builtin_langs: Vec<String> = self
            .dispatcher
            .phase_b_languages()
            .into_iter()
            .map(String::from)
            .collect();

        // H5: collect needs_approval before boxing so we can surface it in outcome.
        let catalog = crate::resolver::CatalogResolver::new();
        let needs_approval_langs: Vec<String> = catalog.needs_approval().to_vec();

        let resolver = crate::resolver::CompositeResolver::new(vec![
            Box::new(crate::resolver::BuiltinResolver::new(
                current_exe,
                builtin_langs,
            )),
            Box::new(catalog),
        ]);

        let mut outcome = PhaseBOutcome {
            skipped_needs_approval: needs_approval_langs,
            ..Default::default()
        };

        let providable = resolver.providable_languages();
        tracing::debug!(
            "Phase B: resolver surfaced {} language(s): {:?}",
            providable.len(),
            providable
        );

        // Build per-language work items after applying all gates. Collecting
        // here (before the thread scope) avoids borrowing `resolver` across the
        // scope boundary while also holding `outcome` mutably.
        enum LangWork {
            /// Dart AOT emitter: run in-process (avoids nested-subprocess SIGABRT).
            Dart,
            /// All other languages: spawn a sidecar subprocess.
            Sidecar(crate::resolver::PluginSpec),
        }

        struct WorkItem {
            lang: String,
            work: LangWork,
            /// Pre-filtered repo-root-relative paths for this language (P6 — #329).
            /// `None` when `inputs.indexable_paths` is empty (caller has no pre-walk).
            files: Option<Vec<String>>,
        }

        // P6 (#329): filter the daemon's pre-walked paths by language extension so
        // each sidecar/runner receives only its language's files and skips its own walk.
        // Returns `None` when `inputs.indexable_paths` is empty (no pre-walk available).
        let lang_files = |lang_name: &str| -> Option<Vec<String>> {
            if inputs.indexable_paths.is_empty() {
                return None;
            }
            let paths: Vec<String> = inputs
                .indexable_paths
                .iter()
                .filter(|p| {
                    p.extension()
                        .and_then(|e| e.to_str())
                        .and_then(Language::from_extension)
                        .map(|l| l.as_str() == lang_name)
                        .unwrap_or(false)
                })
                .filter_map(|p| {
                    p.strip_prefix(repo_root)
                        .ok()
                        .map(|rel| rel.to_string_lossy().replace('\\', "/"))
                })
                .collect();
            Some(paths)
        };

        let mut work_items: Vec<WorkItem> = Vec::new();

        for lang in providable {
            // P1: skip languages not present in the repo (zero sidecar startup cost).
            if !inputs.present_languages.is_empty()
                && !inputs.present_languages.contains(lang.as_str())
            {
                tracing::debug!(lang = %lang, "Phase B skipped — language absent from repo");
                outcome.skipped_not_in_repo.push(lang.clone());
                continue;
            }

            // Builtins (ts, js, rust, python) ship inside the travsr binary and
            // are always ready — no user registration required. External plugins
            // (go, java, …) still need explicit lang.toml registration.
            let is_builtin =
                crate::phase_b::catalog::lookup(lang.as_str()).is_some_and(|e| e.builtin);
            if !is_builtin && !registered.contains(lang.as_str()) {
                tracing::debug!(lang = %lang, "Phase B skipped — not registered in lang.toml");
                outcome.skipped_unregistered.push(lang.clone());
                continue;
            }

            if lang == "dart" {
                // Dart AOT emitter crashes with SIGABRT when spawned as a nested
                // subprocess inside the sandboxed sidecar. Call it directly from
                // the daemon process where HOME and the full env are intact.
                // Running in a thread (below) is safe: threads share the process
                // environment and do not trigger the nested-sidecar SIGABRT.
                // Dart uses an external emitter binary that doesn't accept a file
                // list, so P6 does not apply here.
                work_items.push(WorkItem {
                    lang,
                    work: LangWork::Dart,
                    files: None,
                });
                continue;
            }

            match resolver.resolve(&lang) {
                Some(spec) => {
                    tracing::debug!(lang = %lang, program = %spec.program, "Phase B: resolved spec");
                    let files = lang_files(&lang);
                    work_items.push(WorkItem {
                        lang,
                        work: LangWork::Sidecar(spec),
                        files,
                    });
                }
                None => {
                    tracing::debug!(lang = %lang, "Phase B: resolver returned None (analyzer absent)");
                    outcome.skipped_no_analyzer.push(lang.clone());
                }
            }
        }

        // Per-language result gathered by each thread.
        struct LangResult {
            lang: String,
            nodes: Vec<travsr_core::Node>,
            edges: Vec<travsr_core::Edge>,
            refs: Vec<travsr_core::ScipRef>,
            ran: bool,
            skipped_no_analyzer: bool,
            crashed: bool,
            /// Some((expected, got)) when the sidecar binary's protocol version
            /// does not match the daemon's PROTOCOL_VERSION.
            version_mismatch: Option<(u32, u32)>,
        }

        // P2: fan out per-language work in parallel. Each thread owns its work
        // item and borrows only `repo_root`/`corpus` (both `Sync`).
        // `thread::scope` guarantees all threads finish before the scope exits —
        // no `'static` bounds required, no Arc/clone of `repo_root`.
        let corpus: &str = &self.corpus;
        let mut lang_results: Vec<LangResult> = std::thread::scope(|s| {
            let handles: Vec<_> = work_items
                .into_iter()
                .map(|item| {
                    s.spawn(move || {
                        let lang = item.lang;
                        match item.work {
                            LangWork::Dart => {
                                match travsr_indexer::phase_b_native_dart(corpus, repo_root) {
                                    Ok((nodes, edges)) => {
                                        tracing::debug!(
                                            nodes = nodes.len(),
                                            edges = edges.len(),
                                            "Phase B: native dart complete"
                                        );
                                        LangResult {
                                            lang,
                                            nodes,
                                            edges,
                                            refs: Vec::new(),
                                            ran: true,
                                            skipped_no_analyzer: false,
                                            crashed: false,
                                            version_mismatch: None,
                                        }
                                    }
                                    Err(e) => {
                                        // Dart is builtin — failure means the Dart SDK/pub-cache
                                        // is unavailable, not that no analyzer exists.
                                        tracing::warn!("Phase B dart: {e:#}");
                                        LangResult {
                                            lang,
                                            nodes: Vec::new(),
                                            edges: Vec::new(),
                                            refs: Vec::new(),
                                            ran: false,
                                            skipped_no_analyzer: false,
                                            crashed: true,
                                            version_mismatch: None,
                                        }
                                    }
                                }
                            }
                            LangWork::Sidecar(spec) => {
                                // C1: per-language timeout — kill the child if it doesn't
                                // respond within TIMEOUT_SECS. Prevents KLS/sbt hangs from
                                // starving the Tokio blocking thread pool indefinitely.
                                const TIMEOUT_SECS: u64 = 300; // 5 min
                                let req = travsr_plugin_protocol::InvokeRequest {
                                    root: repo_root.to_path_buf(),
                                    corpus: corpus.to_string(),
                                    scratch: std::path::PathBuf::default(),
                                    // P6 (#329): forward pre-walked file list so the
                                    // sidecar skips its own directory walk.
                                    files: item.files,
                                };
                                match crate::transport::Sidecar::spawn(&spec, repo_root) {
                                    Ok(sidecar) => {
                                        // Watchdog: kills the child after TIMEOUT_SECS so the
                                        // blocking thread never hangs indefinitely.
                                        let pid = sidecar.child_pid();
                                        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                                        let done_wg = std::sync::Arc::clone(&done);
                                        let lang_wg = lang.clone();
                                        std::thread::spawn(move || {
                                            std::thread::sleep(std::time::Duration::from_secs(TIMEOUT_SECS));
                                            if !done_wg.load(std::sync::atomic::Ordering::SeqCst) {
                                                tracing::warn!(
                                                    lang = %lang_wg,
                                                    timeout_secs = TIMEOUT_SECS,
                                                    "Phase B timeout — killing sidecar"
                                                );
                                                if let Some(p) = pid {
                                                    sidecar_kill(p);
                                                }
                                            }
                                        });
                                        let result = match crate::transport::Transport::invoke_phase_b(
                                            &sidecar, req,
                                        ) {
                                            Ok(resp) => {
                                                tracing::debug!(
                                                    lang = %lang,
                                                    nodes = resp.nodes.len(),
                                                    edges = resp.edges.len(),
                                                    refs = resp.refs.len(),
                                                    "Phase B: invoke complete"
                                                );
                                                LangResult {
                                                    lang,
                                                    nodes: resp.nodes,
                                                    edges: resp.edges,
                                                    refs: resp.refs,
                                                    ran: true,
                                                    skipped_no_analyzer: false,
                                                    crashed: false,
                                                    version_mismatch: None,
                                                }
                                            }
                                            Err(travsr_error::IndexError::PhaseNotSupported) => {
                                                tracing::debug!(
                                                    lang = %lang,
                                                    "Phase B: PhaseNotSupported (sidecar declined)"
                                                );
                                                LangResult {
                                                    lang,
                                                    nodes: Vec::new(),
                                                    edges: Vec::new(),
                                                    refs: Vec::new(),
                                                    ran: false,
                                                    skipped_no_analyzer: true,
                                                    crashed: false,
                                                    version_mismatch: None,
                                                }
                                            }
                                            // H4: version mismatch is actionable — surface it
                                            // separately from generic crashes so the user knows
                                            // to run `travsr lang install <lang>` to upgrade.
                                            Err(travsr_error::IndexError::ProtocolVersionMismatch {
                                                expected,
                                                got,
                                            }) => {
                                                tracing::warn!(
                                                    lang = %lang,
                                                    expected,
                                                    got,
                                                    "Phase B: protocol version mismatch — run `travsr lang install {lang}` to upgrade"
                                                );
                                                LangResult {
                                                    lang,
                                                    nodes: Vec::new(),
                                                    edges: Vec::new(),
                                                    refs: Vec::new(),
                                                    ran: false,
                                                    skipped_no_analyzer: false,
                                                    crashed: false,
                                                    version_mismatch: Some((expected, got)),
                                                }
                                            }
                                            Err(e) => {
                                                tracing::warn!("Phase B {lang}: {e}");
                                                LangResult {
                                                    lang,
                                                    nodes: Vec::new(),
                                                    edges: Vec::new(),
                                                    refs: Vec::new(),
                                                    ran: false,
                                                    skipped_no_analyzer: false,
                                                    crashed: true,
                                                    version_mismatch: None,
                                                }
                                            }
                                        };
                                        done.store(true, std::sync::atomic::Ordering::SeqCst);
                                        result
                                    }
                                    Err(e) => {
                                        // Resolver confirmed the binary exists — spawn failure is a crash.
                                        tracing::warn!("Phase B sidecar spawn {lang}: {e}");
                                        LangResult {
                                            lang,
                                            nodes: Vec::new(),
                                            edges: Vec::new(),
                                            refs: Vec::new(),
                                            ran: false,
                                            skipped_no_analyzer: false,
                                            crashed: true,
                                            version_mismatch: None,
                                        }
                                    }
                                }
                            }
                        }
                    })
                })
                .collect();

            // Collect — panics in child threads are demoted to `crashed` entries.
            handles
                .into_iter()
                .map(|h| {
                    h.join().unwrap_or_else(|_| LangResult {
                        lang: "<panicked>".to_string(),
                        nodes: Vec::new(),
                        edges: Vec::new(),
                        refs: Vec::new(),
                        ran: false,
                        skipped_no_analyzer: false,
                        crashed: true,
                        version_mismatch: None,
                    })
                })
                .collect()
        });

        // Determinism: thread completion order is nondeterministic; sort by lang
        // name so the same input always yields the same merged output. The
        // #318 O9 A/B-eval harness and SQLite row ordering both depend on this.
        lang_results.sort_by(|a, b| a.lang.cmp(&b.lang));

        let mut all_nodes: Vec<travsr_core::Node> = Vec::new();
        let mut all_edges: Vec<travsr_core::Edge> = Vec::new();
        let mut all_refs: Vec<travsr_core::ScipRef> = Vec::new();

        for r in lang_results {
            if r.ran {
                outcome.ran.push(r.lang);
            } else if let Some((expected, got)) = r.version_mismatch {
                outcome.version_mismatch.push((r.lang, expected, got));
            } else if r.skipped_no_analyzer {
                outcome.skipped_no_analyzer.push(r.lang);
            } else if r.crashed {
                outcome.crashed.push(r.lang);
            }
            all_nodes.extend(r.nodes);
            all_edges.extend(r.edges);
            all_refs.extend(r.refs);
        }

        // Secondary sort within each language's contribution for full determinism.
        all_nodes.sort_by_key(|n| n.id.0);
        all_edges.sort_by(|a, b| {
            a.src
                .0
                .cmp(&b.src.0)
                .then(a.dst.0.cmp(&b.dst.0))
                .then(a.kind.as_str().cmp(b.kind.as_str()))
        });
        all_refs.sort_by(|a, b| {
            a.caller_path
                .cmp(&b.caller_path)
                .then(a.caller_line.cmp(&b.caller_line))
                .then(a.callee_id.0.cmp(&b.callee_id.0))
        });

        (all_nodes, all_edges, all_refs, outcome)
    }

    /// Resolve cross-language FFI edges from accumulated markers.
    /// Delegates to the existing travsr_indexer resolver.
    pub fn resolve_ffi_edges(
        &self,
        markers: &[travsr_indexer::FfiMarker],
    ) -> Vec<travsr_core::Edge> {
        travsr_indexer::Indexer::with_corpus(&self.corpus).resolve_ffi_edges(markers)
    }
}

/// C1: SIGTERM a sidecar process by OS PID when the per-language watchdog fires.
/// Uses a shell `kill` invocation to avoid adding `libc` as a dependency.
/// Best-effort — if the process already exited, this is a no-op.
fn sidecar_kill(pid: u32) {
    let _ = std::process::Command::new("kill")
        .arg(pid.to_string())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phase_b::catalog::CATALOG;

    /// Safety net for P1 (#322): every Language variant's as_str() must appear
    /// in the union of builtin + catalog language strings so that
    /// `present_languages` collected via `Language::from_extension` can never
    /// silently exclude a present language from Phase B.
    #[test]
    fn language_as_str_covered_by_catalog() {
        let catalog_names: HashSet<&str> = CATALOG.iter().map(|e| e.language).collect();

        use travsr_core::Language;
        let variants = [
            Language::TypeScript,
            Language::Rust,
            Language::Python,
            Language::Go,
            Language::Java,
            Language::Kotlin,
            Language::Ruby,
            Language::CSharp,
            Language::Php,
            Language::Scala,
            Language::Cpp,
            Language::C,
            Language::Swift,
            Language::Dart,
            Language::ObjectiveC,
        ];
        for v in variants {
            assert!(
                catalog_names.contains(v.as_str()),
                "Language::{}::as_str() = {:?} not found in CATALOG — P1 would silently skip it",
                v.as_str(),
                v.as_str(),
            );
        }
    }

    /// P1 + P7: when `present_languages` excludes all resolver languages the
    /// call returns empty vecs and every lang lands in `skipped_not_in_repo`;
    /// the write path is invoked once with empty inputs (Option A, #322 P7).
    #[test]
    fn p1_gate_skips_all_when_no_languages_present() {
        let indexer = PluginIndexer::new("test-corpus");
        let inputs = PhaseBInputs {
            repo_root: std::path::Path::new("/nonexistent"),
            // Single dummy language that no file extension maps to — gates out everything.
            present_languages: ["__no_such_lang__".to_string()].into_iter().collect(),
            indexable_paths: &[],
        };
        let (nodes, edges, refs, outcome) = indexer.invoke_phase_b_all(&inputs);
        assert!(nodes.is_empty(), "expected no nodes when all langs absent");
        assert!(edges.is_empty(), "expected no edges when all langs absent");
        assert!(refs.is_empty(), "expected no refs when all langs absent");
        // skipped_not_in_repo OR skipped_unregistered should account for every lang
        let total_skipped = outcome.skipped_not_in_repo.len() + outcome.skipped_unregistered.len();
        assert!(
            total_skipped > 0,
            "expected at least one lang reported as skipped"
        );
        assert!(outcome.ran.is_empty(), "expected no langs ran");
        assert!(outcome.crashed.is_empty(), "expected no crashes");
    }

    /// P2 determinism: two calls with the same (empty) present_languages gate
    /// must yield byte-identical outcome vectors.
    #[test]
    fn p2_deterministic_outcome_vectors() {
        let indexer = PluginIndexer::new("test-corpus");
        let inputs = PhaseBInputs {
            repo_root: std::path::Path::new("/nonexistent"),
            present_languages: HashSet::new(), // no gating
            indexable_paths: &[],
        };
        let (_, _, _, outcome1) = indexer.invoke_phase_b_all(&inputs);
        let (_, _, _, outcome2) = indexer.invoke_phase_b_all(&inputs);
        assert_eq!(
            outcome1.skipped_not_in_repo, outcome2.skipped_not_in_repo,
            "skipped_not_in_repo must be deterministic across runs"
        );
        assert_eq!(
            outcome1.skipped_unregistered, outcome2.skipped_unregistered,
            "skipped_unregistered must be deterministic across runs"
        );
    }
}
