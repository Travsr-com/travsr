use std::path::Path;
use travsr_error::IndexError;
use travsr_indexer::{hash_file, ParseOutput};
use crate::cache::{CacheKey, ParseCache};
use crate::dispatcher::Dispatcher;
use crate::plugins::response_to_output;
use crate::registry::register_builtins;

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
        Self { corpus, dispatcher, cache: ParseCache::new() }
    }

    /// Parse a file. Caches by (CARGO_PKG_VERSION, sha256). Returns ParseOutput
    /// so the daemon's existing call sites need no changes.
    pub fn parse_file_with_vname(
        &mut self,
        abs_path: &Path,
        _vname_path: &str,
    ) -> Result<ParseOutput, IndexError> {
        let file_hash = hash_file(abs_path).map_err(|e| IndexError::Parse {
            file: abs_path.display().to_string(),
            message: e.to_string(),
        })?;
        let version = env!("CARGO_PKG_VERSION");
        let key = CacheKey { plugin_version: version.to_string(), file_hash };

        // Cache hit
        if let Some(cached) = self.cache.get(version, file_hash) {
            return Ok(response_to_output(cached.clone()));
        }

        // Cache miss: dispatch through plugin
        let corpus = self.dispatcher.corpus.clone();
        let resp = match self.dispatcher.parse_file(abs_path, &corpus, "")? {
            Some(r) => r,
            None => return Ok(ParseOutput::default()),
        };

        self.cache.insert(key, resp.clone());
        Ok(response_to_output(resp))
    }

    /// Resolve cross-language FFI edges from accumulated markers.
    /// Delegates to the existing travsr_indexer resolver.
    pub fn resolve_ffi_edges(
        &self,
        markers: &[travsr_indexer::FfiMarker],
    ) -> Vec<travsr_core::Edge> {
        // Use a temporary Indexer just for FFI resolution (it's stateless for this call)
        travsr_indexer::Indexer::with_corpus(&self.corpus).resolve_ffi_edges(markers)
    }
}
