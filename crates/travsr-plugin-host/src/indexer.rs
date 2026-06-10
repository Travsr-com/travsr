use crate::cache::{CacheKey, ParseCache};
use crate::dispatcher::Dispatcher;
use crate::plugins::response_to_output;
use crate::registry::register_builtins;
use crate::resolver::PluginResolver;
use std::path::Path;
use travsr_error::IndexError;
use travsr_indexer::{hash_file, ParseOutput};

/// Per-language Phase B outcome reported by [`PluginIndexer::invoke_phase_b_all`].
#[derive(Debug, Default, Clone)]
pub struct PhaseBOutcome {
    pub ran: Vec<String>,
    pub skipped_absent: Vec<String>,
    pub skipped_unregistered: Vec<String>,
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
    pub fn invoke_phase_b_all(
        &self,
        repo_root: &std::path::Path,
    ) -> (
        Vec<travsr_core::Node>,
        Vec<travsr_core::Edge>,
        PhaseBOutcome,
    ) {
        // Gate Phase B per language against lang.toml registration.
        // `travsr lang remove <lang>` writes registered=[] which must be respected here.
        let registered: std::collections::HashSet<String> =
            crate::trust::registered_languages_from_disk()
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

        let resolver = crate::resolver::CompositeResolver::new(vec![
            Box::new(crate::resolver::BuiltinResolver::new(
                current_exe,
                builtin_langs,
            )),
            Box::new(crate::resolver::CatalogResolver::new()),
        ]);

        let mut all_nodes = Vec::new();
        let mut all_edges = Vec::new();
        let mut outcome = PhaseBOutcome::default();

        let providable = resolver.providable_languages();
        tracing::debug!(
            "Phase B: resolver surfaced {} language(s): {:?}",
            providable.len(),
            providable
        );

        for lang in providable {
            // Builtins (ts, js, rust, python) ship inside the travsr binary and
            // are always ready — no user registration required. External plugins
            // (go, java, …) still need explicit lang.toml registration.
            let is_builtin =
                crate::phase_b::catalog::lookup(lang.as_str()).is_some_and(|e| e.builtin);
            if !is_builtin && !registered.contains(lang.as_str()) {
                tracing::debug!(
                    "Phase B skipped for '{}' — not registered in lang.toml",
                    lang
                );
                outcome.skipped_unregistered.push(lang.clone());
                continue;
            }
            // Dart AOT emitter crashes with SIGABRT when spawned as a nested
            // subprocess inside the sandboxed sidecar. Call it directly from
            // the daemon process where HOME and the full env are intact.
            if lang == "dart" {
                match travsr_indexer::phase_b_native_dart(&self.corpus, repo_root) {
                    Ok((nodes, edges)) => {
                        tracing::debug!(
                            nodes = nodes.len(),
                            edges = edges.len(),
                            "Phase B: native dart complete"
                        );
                        all_nodes.extend(nodes);
                        all_edges.extend(edges);
                        outcome.ran.push(lang.clone());
                    }
                    Err(e) => {
                        tracing::warn!("Phase B dart: {e:#}");
                        outcome.skipped_absent.push(lang.clone());
                    }
                }
                continue;
            }

            let spec = match resolver.resolve(&lang) {
                Some(s) => s,
                None => {
                    tracing::debug!(lang = %lang, "Phase B: resolver returned None (analyzer absent)");
                    outcome.skipped_absent.push(lang.clone());
                    continue;
                }
            };

            tracing::debug!(
                lang = %lang,
                program = %spec.program,
                "Phase B: resolved spec, spawning sidecar"
            );

            let req = travsr_plugin_protocol::InvokeRequest {
                root: repo_root.to_path_buf(),
                corpus: self.corpus.clone(),
                scratch: std::path::PathBuf::default(), // overwritten in Sidecar::invoke_phase_b
            };

            match crate::transport::Sidecar::spawn(&spec, repo_root) {
                Ok(sidecar) => match crate::transport::Transport::invoke_phase_b(&sidecar, req) {
                    Ok(resp) => {
                        tracing::debug!(
                            lang = %lang,
                            nodes = resp.nodes.len(),
                            edges = resp.edges.len(),
                            "Phase B: invoke complete"
                        );
                        all_nodes.extend(resp.nodes);
                        all_edges.extend(resp.edges);
                        outcome.ran.push(lang.clone());
                    }
                    Err(travsr_error::IndexError::PhaseNotSupported) => {
                        tracing::debug!(lang = %lang, "Phase B: PhaseNotSupported (sidecar declined)");
                        outcome.skipped_absent.push(lang.clone());
                    }
                    Err(e) => {
                        tracing::warn!("Phase B {lang}: {e}");
                        outcome.skipped_absent.push(lang.clone());
                    }
                },
                Err(e) => {
                    tracing::warn!("Phase B sidecar spawn {lang}: {e}");
                    outcome.skipped_absent.push(lang.clone());
                }
            }
        }

        (all_nodes, all_edges, outcome)
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
