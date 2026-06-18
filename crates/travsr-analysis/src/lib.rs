//! travsr-analysis — unified Tree-sitter parsing and code analysis crate.
//!
//! Canonical home for every capability that requires a parsed AST:
//! - All 16 language parsers (Phase A + Phase B call-site edges)
//! - Edge construction helpers
//! - Snippet extraction utilities
//! - FFI boundary marker types
//!
//! Dependency invariant: this crate depends ONLY on `travsr-core`.
//! No other travsr-* crate may appear in Cargo.toml.

#![forbid(unsafe_code)]

pub mod emit;
pub mod ffi;
pub mod go;
pub mod phase_b_python;
pub mod phase_b_rust;
pub mod phase_b_typescript;
pub mod python;
pub mod rust;
pub mod skeleton;
pub mod snippet;
pub mod typescript;

pub use travsr_core::Language;

use travsr_core::{Edge, Node};

/// All graph records produced by parsing a single source file.
#[derive(Debug, Default)]
pub struct ParseOutput {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    /// FFI boundary markers collected during parse.
    /// Consumed by `ffi_resolver` in travsr-indexer's second pass.
    pub ffi_markers: Vec<ffi::FfiMarker>,
}

impl ParseOutput {
    /// Merge `other` into `self`, deduplicating edges on `(src, dst, kind)`.
    /// Nodes are appended without dedup (dedup happens in each parser).
    pub fn merge_deduped(&mut self, other: ParseOutput) {
        self.nodes.extend(other.nodes);
        // Build from self first, then insert from other — HashSet::insert returns
        // false when the element already exists, which also catches duplicates
        // within other.edges itself (not just against the original self.edges).
        let mut existing: std::collections::HashSet<(
            travsr_core::NodeId,
            travsr_core::NodeId,
            travsr_core::EdgeKind,
        )> = self.edges.iter().map(|e| (e.src, e.dst, e.kind)).collect();
        for edge in other.edges {
            if existing.insert((edge.src, edge.dst, edge.kind)) {
                self.edges.push(edge);
            }
        }
        self.ffi_markers.extend(other.ffi_markers);
    }
}
