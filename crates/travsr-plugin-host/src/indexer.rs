use crate::cache::{CacheKey, ParseCache};
use crate::dispatcher::Dispatcher;
use crate::plugins::response_to_output;
use crate::registry::register_builtins;
use crate::resolver::PluginResolver;
use std::path::Path;
use travsr_error::IndexError;
use travsr_indexer::{hash_and_read, ParseOutput};

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
        let (file_hash, _) = hash_and_read(abs_path).map_err(|e| IndexError::Parse {
            file: abs_path.display().to_string(),
            message: e.to_string(),
        })?;
        self.parse_file_with_vname_prehashed(abs_path, vname_path, file_hash)
    }

    /// Parse a file using a pre-computed SHA-256 hash (e.g. already computed
    /// for change detection in the caller).  Avoids re-hashing the file.
    ///
    /// Sends `source: None` — sidecar plugins read directly from the
    /// bind-mounted filesystem, which is served from the kernel page cache
    /// after the caller's hash read.  This avoids serialising raw bytes as a
    /// JSON integer array, which inflates a 50 KB file to ~150 KB on the pipe.
    pub fn parse_file_with_vname_prehashed(
        &mut self,
        abs_path: &Path,
        vname_path: &str,
        file_hash: [u8; 32],
    ) -> Result<ParseOutput, IndexError> {
        let version = env!("CARGO_PKG_VERSION");
        let key = CacheKey {
            plugin_version: version.to_string(),
            file_hash,
        };

        // Cache hit — no IPC needed.
        if let Some(cached) = self.cache.get(version, file_hash) {
            return Ok(response_to_output(cached.clone()));
        }

        // Cache miss: dispatch to plugin. Pass source: None so sidecar plugins
        // read from the filesystem (page-cached after caller's hash_file read)
        // rather than receiving large byte arrays through the JSON pipe.
        let corpus = self.dispatcher.corpus.clone();
        let resp = match self
            .dispatcher
            .parse_file(abs_path, vname_path, &corpus, "", None)?
        {
            Some(r) => r,
            None => return Ok(ParseOutput::default()),
        };

        self.cache.insert(key, resp.clone());
        Ok(response_to_output(resp))
    }

    /// Parse a file when the caller already has the file bytes and hash.
    /// The `source` bytes are available for in-process plugins; sidecar
    /// plugins ignore them and read from the filesystem as normal.
    pub fn parse_file_preread(
        &mut self,
        abs_path: &Path,
        vname_path: &str,
        file_hash: [u8; 32],
        _source: Vec<u8>,
    ) -> Result<ParseOutput, IndexError> {
        self.parse_file_with_vname_prehashed(abs_path, vname_path, file_hash)
    }

    /// Register Phase A sidecar plugins (RFC-013 Direction A).
    ///
    /// Must be called with the repo root after construction so the sandbox can
    /// grant read access to the source tree. Fail-closed: missing binaries are
    /// logged and skipped, never a panic or hard error.
    pub fn register_phase_a_sidecars(&mut self, repo_root: &std::path::Path) {
        crate::registry::register_phase_a_sidecars(&mut self.dispatcher, repo_root);
    }

    /// Phase B: semantic indexing for all trusted, registered languages.
    /// Called from `init_repo` once per full index — not per commit.
    /// ADR-017 Rule 3: checks trust before any code-executing subprocess spawns.
    pub fn invoke_phase_b_all(
        &self,
        repo_root: &std::path::Path,
        trust: &crate::trust::TrustConfig,
    ) -> (Vec<travsr_core::Node>, Vec<travsr_core::Edge>) {
        if !trust.is_trusted(&self.corpus) {
            tracing::info!(
                "Phase B skipped for corpus '{}' — add trust first",
                self.corpus
            );
            return (vec![], vec![]);
        }

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

        for lang in resolver.providable_languages() {
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
                continue;
            }
            let spec = match resolver.resolve(&lang) {
                Some(s) => s,
                None => continue,
            };

            let req = travsr_plugin_protocol::InvokeRequest {
                root: repo_root.to_path_buf(),
                corpus: self.corpus.clone(),
            };

            match crate::transport::Sidecar::spawn(&spec, repo_root) {
                Ok((sidecar, _hs)) => match crate::transport::Transport::invoke_phase_b(&sidecar, req) {
                    Ok(resp) => {
                        all_nodes.extend(resp.nodes);
                        all_edges.extend(resp.edges);
                    }
                    Err(travsr_error::IndexError::PhaseNotSupported) => {}
                    Err(e) => tracing::warn!("Phase B {lang}: {e}"),
                },
                Err(e) => tracing::warn!("Phase B sidecar spawn {lang}: {e}"),
            }
        }

        (all_nodes, all_edges)
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
