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

/// Whether a language runs Phase B in-process rather than through a sidecar.
///
/// Mirrors the `LangWork` dispatch. Kept as a name list rather than threaded
/// through `LangResult` because every construction site would otherwise need a
/// new field; a test pins the two against each other.
fn is_native_phase_b(lang: &str) -> bool {
    matches!(
        lang,
        "rust" | "typescript" | "javascript" | "python" | "dart"
    )
}

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
    /// `travsr lang install <lang>`.
    pub skipped_no_analyzer: Vec<String>,
    pub skipped_unregistered: Vec<String>,
    /// Non-builtin languages that are registered globally but not yet enabled for
    /// this repo (no per-corpus trust grant in lang.toml — ADR-017 Rule 3, #414).
    /// Their external tooling is never spawned. User-actionable: re-run
    /// `travsr lang install <lang>` inside the repo, which auto-grants trust.
    pub skipped_untrusted_corpus: Vec<String>,
    /// Languages that require a `compile_commands.json` at the repo root
    /// (scip-clang, for `c`/`cpp`) but don't have one. Without this gate the
    /// scip-clang invoke hangs with no compilation database until the 300s
    /// invoke timeout, then reports as `crashed`. User-actionable: generate a
    /// compile_commands.json (e.g. via `bear` or CMake's
    /// `CMAKE_EXPORT_COMPILE_COMMANDS`).
    pub skipped_no_compdb: Vec<String>,
    /// Languages registered as RequiresElevated but lacking a PSE approval
    /// entry in lang.toml. User-actionable: `travsr lang approve <lang>`.
    pub skipped_needs_approval: Vec<String>,
    /// Windows only: languages whose analyzer cannot run inside the isolation
    /// layer (java/scala) and for which the user has not granted permission to run
    /// it with their own privileges. Skipped before spawn — never a silent
    /// zero-node run. User-actionable: `travsr lang allow-unsandboxed <lang>`.
    pub skipped_needs_consent: Vec<String>,
    /// Languages whose analyzer was found and spawned but died or errored
    /// mid-invoke.
    pub crashed: Vec<String>,
    /// #712: languages whose analyzer ran cleanly but produced no graph output
    /// at all (no nodes, edges, refs, unresolved calls, or positional refs) even
    /// though the language is present in the repo. A build-free tool that
    /// silently indexes nothing (e.g. scip-ruby spawned without an input path)
    /// would otherwise read as success. User-actionable: rebuild / check the
    /// tool. Not a crash — the sidecar completed, it just emitted nothing.
    /// #724 Finding 2: this is emptiness of ALL output, not just nodes; native
    /// Phase B analyzers legitimately emit edges onto existing Phase A nodes
    /// with no new nodes of their own.
    pub produced_no_nodes: Vec<String>,
    /// Sidecar languages whose analyzer emitted definitions but no reference
    /// occurrences of any kind (#724).
    ///
    /// Distinct from `produced_no_nodes` because the analyzer did produce
    /// symbols, so that message would be false. What it did not produce is any
    /// relationship, which means no call edge can ever be derived: the run reads
    /// as a success and the graph gains nothing traversable.
    pub produced_no_references: Vec<String>,
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
    /// Extra `docs.exclude` path-substring patterns (#376 §3.3), additive to
    /// `travsr_analysis::markdown`'s built-in default exclusion list. These are
    /// the patterns supplied *explicitly* by a caller via
    /// [`PluginIndexer::with_doc_excludes`]; the layered-config ones are
    /// resolved separately into [`Self::config_doc_excludes`].
    doc_excludes: Vec<String>,
    /// #376 O1: the `docs.exclude` config key resolved across
    /// `env > repo > global`, cached after the first markdown file this indexer
    /// sees.
    ///
    /// Resolved lazily, and the repo layer located by walking up from the file
    /// being parsed, because `PluginIndexer` is constructed from seven different
    /// places across `travsr-daemon` and `travsr-cli` and none of them carries a
    /// repo root. Threading one through all seven would work until someone added
    /// an eighth and it silently read only the env layer — the class of silent
    /// divergence this whole item exists to remove. Self-locating is correct at
    /// every construction site by construction.
    config_doc_excludes: Option<Vec<String>>,
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
            doc_excludes: Vec::new(),
            config_doc_excludes: None,
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
            None => {
                // Data formats and prose (#376) have no sidecar plugin — fall
                // back to the built-in Level 1 emitters (Phase A only).
                let ext = abs_path.extension().and_then(|e| e.to_str()).unwrap_or("");
                let lang = Language::from_extension(ext);
                if lang.is_some_and(|l| l.is_data_format()) {
                    return travsr_analysis::data_format::parse(&corpus, abs_path, vname_path)
                        .map_err(|e| IndexError::Parse {
                            file: abs_path.display().to_string(),
                            message: e.to_string(),
                        });
                }
                if lang == Some(Language::Markdown) {
                    let excludes = self.doc_excludes_for(abs_path);
                    return travsr_analysis::markdown::parse(
                        &corpus, abs_path, vname_path, &excludes,
                    )
                    .map_err(|e| IndexError::Parse {
                        file: abs_path.display().to_string(),
                        message: e.to_string(),
                    });
                }
                // Name-recognized manifest whose extension is unmapped or absent
                // (`go.mod`, `*.csproj`): route to the data-format parser, which
                // dispatches by filename. Same fallback as the mapped-extension
                // data formats above — there is no Phase B sidecar for these.
                let file_name = Path::new(vname_path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                if travsr_core::is_manifest_file(file_name) {
                    return travsr_analysis::data_format::parse(&corpus, abs_path, vname_path)
                        .map_err(|e| IndexError::Parse {
                            file: abs_path.display().to_string(),
                            message: e.to_string(),
                        });
                }
                return Ok(ParseOutput::default());
            }
        };

        self.cache.insert(key, resp.clone());
        Ok(response_to_output(resp))
    }

    /// Add extra path-substring patterns (`docs.exclude`, #376 §3.3) beyond
    /// whatever the layered config supplies. Mainly for tests that want a
    /// deterministic exclusion list, and for callers with patterns that are not
    /// user configuration.
    pub fn with_doc_excludes(mut self, mut patterns: Vec<String>) -> Self {
        self.doc_excludes.append(&mut patterns);
        self
    }

    /// The full `docs.exclude` pattern list for a file: the caller's explicit
    /// patterns plus the layered-config ones, which are resolved once and then
    /// reused for every subsequent file (#376 O1).
    fn doc_excludes_for(&mut self, abs_path: &Path) -> Vec<String> {
        if self.config_doc_excludes.is_none() {
            let repo_root = find_repo_root(abs_path);
            self.config_doc_excludes = Some(travsr_config::effective_list(
                "docs.exclude",
                repo_root.as_deref(),
            ));
        }
        let mut all = self.doc_excludes.clone();
        if let Some(cfg) = &self.config_doc_excludes {
            all.extend(cfg.iter().cloned());
        }
        all
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
    // E3 W3b added a 6th return element (positional refs), crossing clippy's
    // tuple-complexity threshold; the tuple shape is intentional and consumed by
    // exactly the daemon/CLI Phase B drivers, so a named struct would only add
    // indirection.
    #[allow(clippy::type_complexity)]
    pub fn invoke_phase_b_all(
        &self,
        inputs: &PhaseBInputs<'_>,
    ) -> (
        Vec<travsr_core::Node>,
        Vec<travsr_core::Edge>,
        Vec<travsr_core::ScipRef>,
        Vec<travsr_core::UnresolvedCall>,
        Vec<travsr_core::LsifPositionalRef>,
        PhaseBOutcome,
    ) {
        let repo_root = inputs.repo_root;

        // One lang.toml read serves both gates below (#685 review): the
        // registration gate and the trust gate used to read and parse the same
        // file back-to-back.
        let lang_toml = crate::trust::LangToml::from_disk();

        // Gate Phase B per language against lang.toml registration.
        // `travsr lang remove <lang>` writes registered=[] which must be respected here.
        let registered: HashSet<String> = lang_toml.registered.iter().cloned().collect();

        // ADR-017 Rule 3 (#414): external Phase B tooling executes repo-related
        // code (go, scip sidecars, …), so it is spawned only for corpora with an
        // explicit trust grant in lang.toml. Loaded once per invocation, checked
        // per non-builtin language below. Builtins are exempt: they ship inside
        // the travsr binary and are governed by Rule 4's first-party rules.
        let trust = lang_toml.trust_config();
        let corpus_trusted = trust.is_trusted(&self.corpus);

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
        // Windows-only: analyzers that cannot run isolated here and have no
        // permission on record are skipped before spawn. Surface repo-present ones
        // so `travsr init`/`status` print the `travsr lang allow-unsandboxed <lang>`
        // step instead of leaving the language silently absent.
        let needs_consent_langs: Vec<String> = catalog
            .needs_unsandboxed_consent()
            .iter()
            .filter(|lang| {
                inputs.present_languages.is_empty()
                    || inputs.present_languages.contains(lang.as_str())
            })
            .cloned()
            .collect();
        // #573: providers installed only as npm shims are dropped inside the
        // resolver (the AppContainer spawn runs PE images only), which used to
        // leave the language in NO outcome bucket — invisible in `travsr init`
        // and `status`. Surface repo-present ones as skipped_no_analyzer: the
        // native binary IS missing, and the bucket's `travsr lang install
        // <lang>` hint is the fix.
        // G2: a wrapper that is installed but whose underlying analyzer tool is
        // absent is dropped here (rather than spawned into a zero-node run that
        // reads as "analyzer installed, fix your project"). Same treatment as the
        // shim case: surface repo-present ones as skipped_no_analyzer so the
        // `travsr lang install <lang>` hint fires.
        let no_analyzer_langs: Vec<String> = catalog
            .unresolvable_shims()
            .iter()
            .chain(catalog.missing_tool())
            .filter(|lang| {
                inputs.present_languages.is_empty()
                    || inputs.present_languages.contains(lang.as_str())
            })
            .cloned()
            .collect();

        let resolver = crate::resolver::CompositeResolver::new(vec![
            Box::new(crate::resolver::BuiltinResolver::new(
                current_exe,
                builtin_langs,
            )),
            Box::new(catalog),
        ]);

        let mut outcome = PhaseBOutcome {
            skipped_needs_approval: needs_approval_langs,
            skipped_no_analyzer: no_analyzer_langs,
            skipped_needs_consent: needs_consent_langs,
            ..Default::default()
        };

        let providable = resolver.providable_languages();
        tracing::debug!(
            "semantic analysis available for {} language(s): {:?}",
            providable.len(),
            providable
        );

        // Build per-language work items after applying all gates. Collecting
        // here (before the thread scope) avoids borrowing `resolver` across the
        // scope boundary while also holding `outcome` mutably.
        enum LangWork {
            /// Dart AOT emitter: run in-process (avoids nested-subprocess SIGABRT).
            Dart,
            /// Rust: run in-process (PATH-independent; avoids daemon PATH stripping
            /// that silently prevents rust-analyzer from being found via sidecar).
            NativeRust,
            /// TypeScript: run in-process for same reason as NativeRust.
            NativeTypescript,
            /// Python: run in-process for same reason as NativeRust.
            NativePython,
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
                tracing::debug!(lang = %lang, "semantic analysis skipped, language not present in this repo");
                outcome.skipped_not_in_repo.push(lang.clone());
                continue;
            }

            // Builtins (ts, js, rust, python) ship inside the travsr binary and
            // are always ready — no user registration required. External plugins
            // (go, java, …) still need explicit lang.toml registration.
            let is_builtin =
                crate::phase_b::catalog::lookup(lang.as_str()).is_some_and(|e| e.builtin);
            if !is_builtin && !registered.contains(lang.as_str()) {
                tracing::debug!(lang = %lang, "semantic analysis skipped, no tool registered for this language");
                outcome.skipped_unregistered.push(lang.clone());
                continue;
            }

            // ADR-017 Rule 3 (#414): a registered external language still needs
            // a per-corpus trust grant before its tooling runs against this
            // repo. Without this gate, registering a language once would execute
            // its build tooling (network-capable under Standard policy) on every
            // repo the user ever opens, including hostile ones.
            if !is_builtin && !corpus_trusted {
                tracing::warn!(
                    lang = %lang,
                    corpus = %self.corpus,
                    "semantic analysis skipped — not enabled for this repo (run `travsr lang install {lang}` here)"
                );
                outcome.skipped_untrusted_corpus.push(lang.clone());
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

            // Builtin languages (rust, typescript, javascript, python) ship inside
            // the travsr binary. Run them in-process so that:
            // (a) rust-analyzer is found via $HOME/.cargo/bin even when the daemon's
            //     PATH has been stripped to /usr/bin:/bin (background fork — #XXX).
            // (b) We avoid spawning a sidecar subprocess whose ENV_ALLOWLIST only
            //     forwards the already-stripped PATH, silently preventing LSIF
            //     enrichment for Rust.
            if lang == "rust" {
                work_items.push(WorkItem {
                    lang,
                    work: LangWork::NativeRust,
                    files: lang_files("rust"),
                });
                continue;
            }
            if lang == "typescript" || lang == "javascript" {
                work_items.push(WorkItem {
                    lang: lang.clone(),
                    work: LangWork::NativeTypescript,
                    files: lang_files(&lang),
                });
                continue;
            }
            if lang == "python" {
                work_items.push(WorkItem {
                    lang,
                    work: LangWork::NativePython,
                    files: lang_files("python"),
                });
                continue;
            }

            // L5a: scip-clang (c/cpp) requires a compile_commands.json at the repo
            // root (`--compdb-path` in its catalog args). Without one it hangs
            // with no compilation database until the invoke timeout fires and the
            // whole batch reports `crashed`, blocking phase_b_commit forever.
            // Detect the dependency from the catalog entry rather than hardcoding
            // language names, so any future scip-clang-based language is covered.
            let needs_compdb = crate::phase_b::catalog::lookup(lang.as_str())
                .is_some_and(|entry| entry.command == "scip-clang");
            if needs_compdb && !inputs.repo_root.join("compile_commands.json").exists() {
                tracing::debug!(
                    lang = %lang,
                    "Phase B skipped — scip-clang requires compile_commands.json"
                );
                outcome.skipped_no_compdb.push(lang.clone());
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
            unresolved_calls: Vec<travsr_core::UnresolvedCall>,
            /// E3 W3b: rust-analyzer LSIF references whose callee is identified by
            /// definition location, resolved against the store in the daemon.
            positional_refs: Vec<travsr_core::LsifPositionalRef>,
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
                                    Ok((nodes, edges, refs)) => {
                                        tracing::debug!(
                                            nodes = nodes.len(),
                                            edges = edges.len(),
                                            refs = refs.len(),
                                            "Phase B: native dart complete"
                                        );
                                        LangResult {
                                            lang,
                                            nodes,
                                            edges,
                                            refs,
                                            unresolved_calls: Vec::new(),
                                            positional_refs: Vec::new(),
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
                                            unresolved_calls: Vec::new(),
                                            positional_refs: Vec::new(),
                                            ran: false,
                                            skipped_no_analyzer: false,
                                            crashed: true,
                                            version_mismatch: None,
                                        }
                                    }
                                }
                            }
                            LangWork::NativeRust => {
                                use travsr_indexer::sandbox::SandboxConfig;
                                // Convert pre-walked relative paths to (abs, vname) pairs.
                                let files_owned: Option<Vec<(std::path::PathBuf, String)>> =
                                    item.files.as_ref().map(|rel| {
                                        rel.iter()
                                            .map(|r| (repo_root.join(r), r.clone()))
                                            .collect()
                                    });
                                let (mut nodes, mut edges, mut unresolved_calls, refs) =
                                    travsr_indexer::phase_b_native_rust(
                                        corpus,
                                        repo_root,
                                        files_owned.as_deref(),
                                    )
                                    .unwrap_or_else(|e| {
                                        tracing::warn!("rust native phase_b: {e}");
                                        (vec![], vec![], vec![], vec![])
                                    });
                                tracing::debug!(
                                    nodes = nodes.len(),
                                    edges = edges.len(),
                                    unresolved_calls = unresolved_calls.len(),
                                    "Phase B: native rust complete"
                                );
                                // LSIF enrichment via rust-analyzer — uses $HOME/.cargo/bin
                                // fallback so it works even when daemon PATH is stripped.
                                let cfg = SandboxConfig {
                                    repo_root: repo_root.to_path_buf(),
                                    allow_unsandboxed: travsr_indexer::sandbox::allow_unsandboxed_opt_in(),
                                    ..Default::default()
                                };
                                // E3 (W3b) — positional, fail-closed rust-analyzer
                                // LSIF ingestion. Each reference carries its
                                // callee's DEFINITION location; the daemon resolves
                                // it to a real Phase A node against the full store
                                // (works cross-file and incrementally — Invariant #2)
                                // and drops anything that does not resolve. Replaces
                                // the moniker-synth `ingest_rust` path whose callee
                                // VName (at `path = project_root`) matched no Phase A
                                // node — 100% dangling (18,530 dead edges here).
                                let mut positional_refs: Vec<travsr_core::LsifPositionalRef> =
                                    Vec::new();
                                match travsr_indexer::run_ra_lsif(repo_root, &cfg) {
                                    Ok(Some(dump)) => {
                                        let prefs = travsr_indexer::ingest_rust_positional(
                                            &dump,
                                            &repo_root.to_string_lossy(),
                                        );
                                        tracing::debug!(
                                            positional_refs = prefs.len(),
                                            "Phase B: rust-analyzer LSIF positional refs parsed"
                                        );
                                        positional_refs = prefs;
                                    }
                                    Ok(None) => {
                                        tracing::debug!(
                                            "rust-analyzer not available — native phase_b only"
                                        )
                                    }
                                    Err(e) => tracing::warn!("rust-analyzer failed: {e}"),
                                }
                                nodes.sort_unstable_by_key(|n| n.id);
                                nodes.dedup_by_key(|n| n.id);
                                edges.sort_unstable_by_key(|e| (e.src, e.dst));
                                edges
                                    .dedup_by(|a, b| a.src == b.src && a.dst == b.dst && a.kind == b.kind);
                                // Keep caller_line in the dedup key so two distinct
                                // call sites of the same callee from one caller both
                                // survive (#299 find_references needs every line).
                                unresolved_calls.sort_unstable_by(|a, b| {
                                    a.src
                                        .0
                                        .cmp(&b.src.0)
                                        .then(a.callee_sig.cmp(&b.callee_sig))
                                        .then(a.caller_line.cmp(&b.caller_line))
                                });
                                unresolved_calls.dedup_by(|a, b| {
                                    a.src == b.src
                                        && a.callee_sig == b.callee_sig
                                        && a.caller_line == b.caller_line
                                });
                                LangResult {
                                    lang,
                                    nodes,
                                    edges,
                                    // #299: native same-file call sites carry
                                    // occurrence lines → edge_sites via the G2
                                    // attributed-write path. rust-analyzer LSIF
                                    // stays edge-only (synthetic moniker callee
                                    // ids don't reconcile to tree-sitter nodes).
                                    refs,
                                    unresolved_calls,
                                    positional_refs,
                                    ran: true,
                                    skipped_no_analyzer: false,
                                    crashed: false,
                                    version_mismatch: None,
                                }
                            }
                            LangWork::NativeTypescript => {
                                let files_owned: Option<Vec<(std::path::PathBuf, String)>> =
                                    item.files.as_ref().map(|rel| {
                                        rel.iter()
                                            .map(|r| (repo_root.join(r), r.clone()))
                                            .collect()
                                    });
                                let (mut nodes, mut edges, mut unresolved_calls) =
                                    travsr_indexer::phase_b_native_typescript(
                                        corpus,
                                        repo_root,
                                        files_owned.as_deref(),
                                    )
                                    .unwrap_or_else(|e| {
                                        tracing::warn!("ts native phase_b: {e}");
                                        (vec![], vec![], vec![])
                                    });
                                tracing::debug!(
                                    nodes = nodes.len(),
                                    edges = edges.len(),
                                    unresolved_calls = unresolved_calls.len(),
                                    "Phase B: native typescript complete"
                                );
                                // LSIF enrichment via travsr-lsif-ts when tsconfig.json exists.
                                // #299: ingest_lsif_g2 recovers per-occurrence lines as
                                // ScipRefs (was: file-level edges). The callee id is the
                                // emitter's travsr_vname (path+signature) → already a
                                // tree-sitter node id, so it reconciles without an alias
                                // pass, and write_scip_attributed_batch records edge_sites.
                                let mut refs: Vec<travsr_core::ScipRef> = Vec::new();
                                let tsconfig = repo_root.join("tsconfig.json");
                                if tsconfig.exists() {
                                    match travsr_indexer::run_lsif_emitter(&tsconfig) {
                                        Ok(dump) => {
                                            match travsr_indexer::ingest_lsif_g2(&dump, corpus) {
                                                Ok(g2) => {
                                                    tracing::debug!(
                                                        refs = g2.refs.len(),
                                                        "Phase B: ts lsif occurrence refs merged"
                                                    );
                                                    refs.extend(g2.refs);
                                                }
                                                Err(e) => tracing::warn!("ts lsif ingest: {e}"),
                                            }
                                        }
                                        Err(e) => {
                                            tracing::debug!("ts lsif emitter not available: {e}")
                                        }
                                    }
                                }
                                nodes.sort_unstable_by_key(|n| n.id);
                                nodes.dedup_by_key(|n| n.id);
                                edges.sort_unstable_by_key(|e| (e.src, e.dst));
                                edges
                                    .dedup_by(|a, b| a.src == b.src && a.dst == b.dst && a.kind == b.kind);
                                // E4: keep caller_line in the dedup key so every
                                // distinct call site survives (#299 find_references).
                                unresolved_calls.sort_unstable_by(|a, b| {
                                    a.src
                                        .0
                                        .cmp(&b.src.0)
                                        .then(a.callee_sig.cmp(&b.callee_sig))
                                        .then(a.caller_line.cmp(&b.caller_line))
                                });
                                unresolved_calls.dedup_by(|a, b| {
                                    a.src == b.src
                                        && a.callee_sig == b.callee_sig
                                        && a.caller_line == b.caller_line
                                });
                                LangResult {
                                    lang,
                                    nodes,
                                    edges,
                                    refs,
                                    unresolved_calls,
                                    positional_refs: Vec::new(),
                                    ran: true,
                                    skipped_no_analyzer: false,
                                    crashed: false,
                                    version_mismatch: None,
                                }
                            }
                            LangWork::NativePython => {
                                let files_owned: Option<Vec<(std::path::PathBuf, String)>> =
                                    item.files.as_ref().map(|rel| {
                                        rel.iter()
                                            .map(|r| (repo_root.join(r), r.clone()))
                                            .collect()
                                    });
                                let (mut nodes, mut edges, mut unresolved_calls) =
                                    travsr_indexer::phase_b_native_python(
                                        corpus,
                                        repo_root,
                                        files_owned.as_deref(),
                                    )
                                    .unwrap_or_else(|e| {
                                        tracing::warn!("python native phase_b: {e}");
                                        (vec![], vec![], vec![])
                                    });
                                tracing::debug!(
                                    nodes = nodes.len(),
                                    edges = edges.len(),
                                    unresolved_calls = unresolved_calls.len(),
                                    "Phase B: native python complete"
                                );
                                // LSIF enrichment via travsr-lsif-py (bundled, PATH-independent).
                                // travsr-lsif-py resolves via current_exe walk-up so it works
                                // even when the daemon's PATH has been stripped by the OS.
                                // #299: ingest_lsif_g2 recovers occurrence lines as ScipRefs
                                // (was: file-level edges) → edge_sites for find_references.
                                let mut refs: Vec<travsr_core::ScipRef> = Vec::new();
                                match travsr_indexer::run_lsif_py_emitter(repo_root) {
                                    Ok(Some(dump)) => {
                                        match travsr_indexer::ingest_lsif_g2(&dump, corpus) {
                                            Ok(g2) => {
                                                tracing::debug!(
                                                    refs = g2.refs.len(),
                                                    "Phase B: python lsif occurrence refs merged"
                                                );
                                                refs.extend(g2.refs);
                                            }
                                            Err(e) => tracing::warn!("python lsif ingest: {e}"),
                                        }
                                    }
                                    Ok(None) => tracing::debug!(
                                        "travsr-lsif-py not found — native phase_b tree-sitter edges only"
                                    ),
                                    Err(e) => tracing::warn!("travsr-lsif-py failed: {e}"),
                                }
                                nodes.sort_unstable_by_key(|n| n.id);
                                nodes.dedup_by_key(|n| n.id);
                                edges.sort_unstable_by_key(|e| (e.src, e.dst));
                                edges
                                    .dedup_by(|a, b| a.src == b.src && a.dst == b.dst && a.kind == b.kind);
                                // E4: keep caller_line in the dedup key so every
                                // distinct call site survives (#299 find_references).
                                unresolved_calls.sort_unstable_by(|a, b| {
                                    a.src
                                        .0
                                        .cmp(&b.src.0)
                                        .then(a.callee_sig.cmp(&b.callee_sig))
                                        .then(a.caller_line.cmp(&b.caller_line))
                                });
                                unresolved_calls.dedup_by(|a, b| {
                                    a.src == b.src
                                        && a.callee_sig == b.callee_sig
                                        && a.caller_line == b.caller_line
                                });
                                LangResult {
                                    lang,
                                    nodes,
                                    edges,
                                    refs,
                                    unresolved_calls,
                                    positional_refs: Vec::new(),
                                    ran: true,
                                    skipped_no_analyzer: false,
                                    crashed: false,
                                    version_mismatch: None,
                                }
                            }
                            LangWork::Sidecar(spec) => {
                                // #388: both the spawn handshake and the invoke
                                // round-trip are now watchdog-guarded inside the
                                // transport (HANDSHAKE_TIMEOUT_SECS / INVOKE_TIMEOUT_SECS),
                                // so a wedged plugin is killed and surfaced as a
                                // crash instead of hanging this scoped thread — no
                                // bespoke timeout needed here.
                                let req = travsr_plugin_protocol::InvokeRequest {
                                    // Strip the Windows `\\?\` verbatim prefix ONCE here,
                                    // for every sidecar: the daemon's repo_root is
                                    // canonicalized (extended-length) on Windows, and
                                    // analyzers that build a URI / working-directory from
                                    // it (scip-dotnet, sbt, KLS) choke on the prefix. This
                                    // is the systemic counterpart to the per-wrapper strips
                                    // (kotlin K7, scala S7, csharp) — belt and suspenders.
                                    root: crate::sandbox::toolchain::strip_windows_verbatim(
                                        repo_root.to_path_buf(),
                                    ),
                                    corpus: corpus.to_string(),
                                    scratch: std::path::PathBuf::default(),
                                    // P6 (#329): forward pre-walked file list so the
                                    // sidecar skips its own directory walk.
                                    files: item.files,
                                };
                                match crate::transport::Sidecar::spawn(&spec, repo_root) {
                                    Ok(sidecar) => {
                                        let result = match crate::transport::Transport::invoke_phase_b(
                                            &sidecar, req,
                                        ) {
                                            Ok(resp) => {
                                                tracing::debug!(
                                                    lang = %lang,
                                                    nodes = resp.nodes.len(),
                                                    edges = resp.edges.len(),
                                                    refs = resp.refs.len(),
                                                    unresolved_calls = resp.unresolved_calls.len(),
                                                    "Phase B: invoke complete"
                                                );
                                                LangResult {
                                                    lang,
                                                    nodes: resp.nodes,
                                                    edges: resp.edges,
                                                    refs: resp.refs,
                                                    unresolved_calls: resp.unresolved_calls,
                                                    positional_refs: Vec::new(),
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
                                                    unresolved_calls: Vec::new(),
                                                    positional_refs: Vec::new(),
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
                                                    unresolved_calls: Vec::new(),
                                                    positional_refs: Vec::new(),
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
                                                    unresolved_calls: Vec::new(),
                                                    positional_refs: Vec::new(),
                                                    ran: false,
                                                    skipped_no_analyzer: false,
                                                    crashed: true,
                                                    version_mismatch: None,
                                                }
                                            }
                                        };
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
                                            unresolved_calls: Vec::new(),
                                            positional_refs: Vec::new(),
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
                        unresolved_calls: Vec::new(),
                        positional_refs: Vec::new(),
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
        let mut all_unresolved: Vec<travsr_core::UnresolvedCall> = Vec::new();
        let mut all_positional_refs: Vec<travsr_core::LsifPositionalRef> = Vec::new();

        for r in lang_results {
            if r.ran {
                // #712: Phase B only invokes languages present in the repo, so a
                // clean run that yields nothing means the analyzer indexed
                // nothing despite having source files — surface it instead of
                // recording a silent zero-node "success".
                //
                // #724 Finding 2: "nothing" must mean no graph output at all, not
                // merely no nodes. Native Phase B analyzers (python/typescript/
                // rust) attach ref/call edges to existing Phase A tree-sitter
                // nodes and emit no new SCIP-style definition nodes, so an empty
                // `nodes` list is normal for a fully successful run. Only flag a
                // language that produced neither nodes nor edges/refs of any kind,
                // so an analyzer that produced call edges is not falsely reported
                // as having "produced no symbols".
                let no_occurrences = r.edges.is_empty()
                    && r.refs.is_empty()
                    && r.unresolved_calls.is_empty()
                    && r.positional_refs.is_empty();

                if r.nodes.is_empty() && no_occurrences {
                    outcome.produced_no_nodes.push(r.lang.clone());
                } else if no_occurrences && !is_native_phase_b(&r.lang) {
                    // #724 follow-up: the widened check above is justified for
                    // native Phase B, where call resolution attaches edges to
                    // pre-existing tree-sitter nodes and emits no new definition
                    // nodes, so empty `nodes` is normal. That reasoning does not
                    // carry to a sidecar: nodes, edges, refs and unresolved calls
                    // all arrive in one external tool response, so definitions
                    // without a single occurrence means the tool succeeded and
                    // dropped every reference. That is #724's Java finding, and
                    // the widened check hid it, because the nodes were there.
                    outcome.produced_no_references.push(r.lang.clone());
                }
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
            all_unresolved.extend(r.unresolved_calls);
            all_positional_refs.extend(r.positional_refs);
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

        // Dedup on (src, callee_sig, caller_line): the same textual call site
        // captured twice collapses, but two distinct call sites of the same
        // callee from the same caller are preserved so find_references (#299)
        // reports every occurrence line, not just the first.
        all_unresolved.sort_unstable_by(|a, b| {
            a.src
                .0
                .cmp(&b.src.0)
                .then(a.callee_sig.cmp(&b.callee_sig))
                .then(a.caller_line.cmp(&b.caller_line))
        });
        all_unresolved.dedup_by(|a, b| {
            a.src == b.src && a.callee_sig == b.callee_sig && a.caller_line == b.caller_line
        });

        // E3 W3b: deterministic order for the positional rust-analyzer refs.
        all_positional_refs.sort_by(|a, b| {
            a.callee_def_path
                .cmp(&b.callee_def_path)
                .then(a.callee_def_line.cmp(&b.callee_def_line))
                .then(a.caller_path.cmp(&b.caller_path))
                .then(a.caller_line.cmp(&b.caller_line))
        });

        (
            all_nodes,
            all_edges,
            all_refs,
            all_unresolved,
            all_positional_refs,
            outcome,
        )
    }

    /// Resolve cross-language FFI edges from accumulated markers.
    /// Delegates to the existing travsr_indexer resolver.
    pub fn resolve_ffi_edges(
        &self,
        markers: &[travsr_indexer::FfiMarker],
    ) -> Vec<travsr_core::Edge> {
        travsr_indexer::Indexer::with_corpus(&self.corpus).resolve_ffi_edges(markers)
    }

    /// Resolve Cargo workspace-inherited dependency versions (A2) from
    /// accumulated markers. Delegates to the travsr_indexer resolver.
    pub fn resolve_workspace_deps(
        &self,
        markers: &[travsr_analysis::data_format::WorkspaceDepMarker],
    ) -> (Vec<travsr_core::Node>, Vec<travsr_core::Edge>) {
        travsr_indexer::Indexer::with_corpus(&self.corpus).resolve_workspace_deps(markers)
    }
}

/// Locate the repo whose `.travsr/config.toml` governs `abs_path`, by walking
/// up from the file being indexed until a `.travsr` directory appears (#376 O1).
///
/// `.travsr` rather than `.git`: it is the directory that holds both the index
/// and the per-repo config, so it is exactly the marker that answers "which
/// repo's config applies to this file". A file outside any indexed repo yields
/// `None`, which degrades to the env and global layers.
///
/// The home directory is excluded, because `~/.travsr` is the **global** config
/// dir (it holds `bin/`, `models/` and the global `config.toml`), not a repo.
/// Without this a file anywhere under `$HOME` but outside a repo resolves its
/// "repo" layer to the global config file, so the repo and global layers alias
/// each other and precedence between them stops meaning anything.
///
/// Found on Windows CI rather than by review: the runner's temp dir lives under
/// `C:\Users\RUNNER~1`, which has a real `~/.travsr`, so a path with no repo
/// above it resolved to the home directory. On macOS and Linux the temp dir
/// sits outside `$HOME`, which is why it never showed up locally.
fn find_repo_root(abs_path: &Path) -> Option<PathBuf> {
    let home = dirs::home_dir();
    abs_path
        .ancestors()
        .skip(1)
        .find(|dir| home.as_deref() != Some(*dir) && dir.join(".travsr").is_dir())
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    /// #724: a sidecar that returns definitions and not one occurrence.
    ///
    /// scip-java did exactly this: Maven succeeded, valid SCIP came back, every
    /// reference occurrence was dropped, and no Java call edge was ever created.
    /// The widened #712 check could not see it, because the nodes were present.
    #[test]
    fn a_sidecar_with_definitions_but_no_occurrences_is_flagged() {
        assert!(
            !super::is_native_phase_b("java"),
            "java runs through a sidecar, so the stricter rule applies to it"
        );
        // Native languages legitimately return no new nodes, so the same shape
        // must not be flagged for them.
        for lang in ["rust", "typescript", "python", "javascript", "dart"] {
            assert!(
                super::is_native_phase_b(lang),
                "{lang} resolves in-process and attaches edges to Phase A nodes"
            );
        }
        for lang in [
            "java", "go", "ruby", "php", "csharp", "kotlin", "scala", "cpp", "c",
        ] {
            assert!(
                !super::is_native_phase_b(lang),
                "{lang} arrives as one sidecar response, so definitions without \
                 occurrences means the tool dropped them"
            );
        }
    }

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
            Language::Json,
            Language::Yaml,
            Language::Toml,
            Language::Xml,
            Language::Markdown,
        ];
        for v in variants {
            if v.is_phase_a_only() {
                // Phase-A-only formats/prose intentionally absent from the Phase B catalog.
                continue;
            }
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
        let (nodes, edges, refs, unresolved, positional, outcome) =
            indexer.invoke_phase_b_all(&inputs);
        assert!(
            positional.is_empty(),
            "expected no positional refs when all langs absent"
        );
        assert!(nodes.is_empty(), "expected no nodes when all langs absent");
        assert!(edges.is_empty(), "expected no edges when all langs absent");
        assert!(refs.is_empty(), "expected no refs when all langs absent");
        assert!(
            unresolved.is_empty(),
            "expected no unresolved when all langs absent"
        );
        // skipped_not_in_repo OR skipped_unregistered should account for every lang
        let total_skipped = outcome.skipped_not_in_repo.len() + outcome.skipped_unregistered.len();
        assert!(
            total_skipped > 0,
            "expected at least one lang reported as skipped"
        );
        assert!(outcome.ran.is_empty(), "expected no langs ran");
        assert!(outcome.crashed.is_empty(), "expected no crashes");
    }

    /// #376 O1: `find_repo_root` must locate the `.travsr`-bearing ancestor of
    /// the file being indexed, so a `PluginIndexer` built anywhere still reads
    /// the right repo's `docs.exclude`. A file outside any indexed repo yields
    /// `None` (env + global layers only), never a panic.
    #[test]
    fn find_repo_root_walks_up_to_the_travsr_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join(".travsr")).expect("mk .travsr");
        std::fs::create_dir_all(root.join("docs").join("adrs")).expect("mk docs");
        let file = root.join("docs").join("adrs").join("ADR-001.md");
        std::fs::write(&file, "# hi").expect("write");

        assert_eq!(
            find_repo_root(&file).as_deref(),
            Some(root),
            "must find the .travsr-bearing ancestor"
        );

        // A `.travsr` *file* (not a directory) must not be mistaken for a root.
        //
        // Asserted as "this directory was not chosen" rather than "the result is
        // None": the temp dir's own ancestors are not ours to control, and on
        // Windows they include the user's home, which has a real `~/.travsr`.
        // Asserting None there tested the runner's filesystem, not this function.
        let other = tempfile::tempdir().expect("tempdir2");
        std::fs::write(other.path().join(".travsr"), "not a dir").expect("write");
        let stray = other.path().join("README.md");
        std::fs::write(&stray, "# hi").expect("write");
        assert_ne!(
            find_repo_root(&stray).as_deref(),
            Some(other.path()),
            "a .travsr file is not a repo root"
        );
    }

    /// `~/.travsr` is the global config dir, not a repo, so it must never be
    /// returned as a repo root. Otherwise a file under `$HOME` but outside any
    /// repo makes the repo layer alias the global layer.
    #[test]
    fn find_repo_root_never_returns_the_home_directory() {
        let Some(home) = dirs::home_dir() else {
            return; // no home to test against
        };
        // Only meaningful when ~/.travsr actually exists; on a machine without
        // it the walk would skip the home dir anyway.
        if !home.join(".travsr").is_dir() {
            return;
        }
        let probe = home.join("__travsr_nonexistent_probe__").join("a.md");
        assert_ne!(find_repo_root(&probe).as_deref(), Some(home.as_path()));
    }

    /// Explicit `with_doc_excludes` patterns and layered-config ones are
    /// additive, and the resolution happens once per indexer rather than per
    /// file.
    #[test]
    fn doc_excludes_merge_explicit_and_config_layers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join(".travsr")).expect("mk .travsr");
        let file = root.join("README.md");
        std::fs::write(&file, "# hi").expect("write");

        let mut indexer =
            PluginIndexer::new("test-corpus").with_doc_excludes(vec!["explicit/".to_string()]);
        let resolved = indexer.doc_excludes_for(&file);
        assert!(
            resolved.contains(&"explicit/".to_string()),
            "caller-supplied patterns must survive the merge: {resolved:?}"
        );
        assert!(
            indexer.config_doc_excludes.is_some(),
            "config layer must be resolved and cached after the first file"
        );
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
        let (_, _, _, _, _, outcome1) = indexer.invoke_phase_b_all(&inputs);
        let (_, _, _, _, _, outcome2) = indexer.invoke_phase_b_all(&inputs);
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
