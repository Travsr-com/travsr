//! travsr-indexer — Tree-sitter + LSIF parsing pipeline.
//!
//! Turns source files on disk into `travsr-core::Node` / `Edge` records.

#![forbid(unsafe_code)]

use std::path::Path;

use travsr_core::Node;

/// Streaming indexer that walks a repository and emits graph records.
#[derive(Debug, Default)]
pub struct Indexer {
    _private: (),
}

impl Indexer {
    /// Construct a new indexer with default configuration.
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse a single source file into a vector of graph nodes.
    ///
    /// Stub implementation — returns an empty vector. Real Tree-sitter
    /// wiring lands in Sprint 1 (see CLAUDE.md).
    pub fn parse_file(&self, _path: &Path) -> anyhow::Result<Vec<Node>> {
        Ok(vec![])
    }
}
