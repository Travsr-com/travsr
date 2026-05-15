//! travsr-indexer — Tree-sitter + LSIF parsing pipeline.
//!
//! Turns source files on disk into `travsr-core::Node` / `Edge` records.
//! Sprint 1 supports TypeScript and TSX via tree-sitter.
//! Sprint 2 adds SHA-256 file hashing and repo-relative VName paths.

#![forbid(unsafe_code)]

mod emit;
mod hash;
mod typescript;

use std::path::Path;

pub use hash::hash_file;
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
    /// Uses the file's own path string as the VName path. Callers that need
    /// repo-relative VName paths (e.g. the daemon) should use
    /// [`parse_file_with_vname`] instead.
    pub fn parse_file(&self, path: &Path) -> anyhow::Result<ParseOutput> {
        let vname_path = path.to_string_lossy().replace('\\', "/");
        self.parse_file_with_vname(path, &vname_path)
    }

    /// Parse `abs_path` using `vname_path` as the stable, repo-relative path
    /// stored in every emitted VName (closes DEBT-012).
    pub fn parse_file_with_vname(
        &self,
        abs_path: &Path,
        vname_path: &str,
    ) -> anyhow::Result<ParseOutput> {
        match abs_path.extension().and_then(|e| e.to_str()) {
            Some("ts" | "tsx") => typescript::parse(abs_path, vname_path),
            _ => Ok(ParseOutput::default()),
        }
    }
}
