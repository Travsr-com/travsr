//! Config-driven Phase A plugin using tree-sitter.
//!
//! `GenericTreeSitterPlugin` wraps a `travsr_analysis::generic::LanguageConfig`
//! and implements the `Plugin` trait. The core parse logic lives in
//! `travsr_analysis::generic::parse_with_config`; this wrapper adds the
//! pre-compiled query cache so the query string is only compiled once per
//! plugin instance rather than once per file.

use travsr_core::Language;
use travsr_plugin_protocol::{InvokeRequest, InvokeResponse, ParseRequest, ParseResponse, Plugin};
use tree_sitter::Query;

pub use travsr_analysis::generic::LanguageConfig;

use crate::plugins::parse_output_to_response;

/// A Phase A plugin driven entirely by a `LanguageConfig` — no per-language
/// Rust logic required. Phase B always returns empty (language-specific
/// invokers live in dedicated plugin crates).
///
/// Caches the compiled `Query` at construction so the query string is
/// compiled once per plugin instance (not per file).
pub struct GenericTreeSitterPlugin {
    config: &'static LanguageConfig,
    grammar: tree_sitter::Language,
    compiled_query: Query,
}

impl GenericTreeSitterPlugin {
    /// Construct from a config. The grammar is obtained via `config.get_grammar()`.
    /// Panics at startup if the query string is invalid — misconfiguration
    /// should be caught at development time, not silently at runtime.
    pub fn new(config: &'static LanguageConfig) -> Self {
        let grammar = (config.get_grammar)();
        let compiled_query = Query::new(&grammar, config.queries).unwrap_or_else(|e| {
            panic!(
                "invalid tree-sitter query for language {:?}: {e}",
                config.language.as_str()
            )
        });
        Self {
            config,
            grammar,
            compiled_query,
        }
    }
}

impl Plugin for GenericTreeSitterPlugin {
    fn language(&self) -> Language {
        self.config.language
    }
    fn extensions(&self) -> &[&str] {
        self.config.extensions
    }
    fn supports_phase_b(&self) -> bool {
        false
    }

    fn parse(&self, req: &ParseRequest) -> ParseResponse {
        match travsr_analysis::generic::parse_with_config(
            self.config,
            &self.grammar,
            Some(&self.compiled_query),
            &req.corpus,
            &req.path,
            &req.vname_path,
        ) {
            Ok(out) => parse_output_to_response(out),
            Err(e) => {
                tracing::warn!(
                    "{} parse {}: {e}",
                    self.config.language.as_str(),
                    req.path.display()
                );
                ParseResponse::default()
            }
        }
    }

    fn invoke_phase_b(&self, _req: &InvokeRequest) -> InvokeResponse {
        InvokeResponse::default()
    }
}
