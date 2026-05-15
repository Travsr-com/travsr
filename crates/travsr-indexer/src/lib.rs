//! travsr-indexer — Tree-sitter + LSIF parsing pipeline.
//!
//! Turns source files on disk into `travsr-core::Node` / `Edge` records.
//! Sprint 1 supports TypeScript and TSX via tree-sitter.
//! Sprint 2 will add LSIF-derived call and ref edges.

#![forbid(unsafe_code)]

mod emit;
mod typescript;

use std::path::Path;

pub use travsr_core::{Edge, Node};

/// All graph records produced by parsing a single source file.
#[derive(Debug, Default)]
pub struct ParseOutput {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

/// Streaming indexer that walks a repository and emits graph records.
#[derive(Debug, Default)]
pub struct Indexer;

impl Indexer {
    pub fn new() -> Self {
        Self
    }

    /// Parse a single source file into nodes and edges.
    ///
    /// Returns `Err` only on I/O failure. Parse errors produce a partial
    /// `ParseOutput` that always contains at least the file node.
    pub fn parse_file(&self, path: &Path) -> anyhow::Result<ParseOutput> {
        match path.extension().and_then(|e| e.to_str()) {
            Some("ts" | "tsx") => typescript::parse(path),
            _ => Ok(ParseOutput::default()),
        }
    }
}
