//! travsr-analysis — unified Tree-sitter parsing and code analysis crate.
//!
//! Canonical home for every capability that requires a parsed AST:
//! - All 16 language parsers (Phase A + Phase B call-site edges)
//! - Config-driven generic Phase A infrastructure (`generic`)
//! - Edge construction helpers
//! - Snippet extraction utilities
//! - FFI boundary marker types
//!
//! Dependency invariant: this crate depends ONLY on `travsr-core`.
//! No other travsr-* crate may appear in Cargo.toml.

#![forbid(unsafe_code)]

// ── Infrastructure ────────────────────────────────────────────────────────────
pub mod emit;
pub mod ffi;
pub mod generic;
pub mod live_detect;
pub mod skeleton;
pub mod snippet;
pub mod test_role;

// ── Phase A parsers — high-complexity (hand-crafted, FFI/LSIF aware) ─────────
pub mod go;
pub mod python;
pub mod rust;
pub mod typescript;

// ── Phase A parsers — config-driven via generic::LanguageConfig ───────────────
pub mod c;
pub mod cpp;
pub mod csharp;
pub mod dart;
pub mod java;
pub mod kotlin;
pub mod objc;
pub mod php;
pub mod ruby;
pub mod scala;
pub mod swift;

// ── Data/config format parsers — Phase A only, no Phase B tool ───────────────
pub mod data_format;

// ── Prose parser — Phase A only, no Phase B tool (#376) ──────────────────────
pub mod markdown;

// ── Phase B call-site parsers ─────────────────────────────────────────────────
pub mod phase_b_dart;
pub mod phase_b_python;
pub mod phase_b_rust;
pub mod phase_b_typescript;

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
    /// Cargo workspace dependency markers (A2). Consumed by the indexer's
    /// workspace-dep resolver after all files in the batch are parsed, so a
    /// member's `{ workspace = true }` entry gets the root's version.
    pub workspace_dep_markers: Vec<data_format::WorkspaceDepMarker>,
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
        self.workspace_dep_markers
            .extend(other.workspace_dep_markers);
    }
}
