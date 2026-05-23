//! Integration tests for the Rust tree-sitter indexer and link_imports_rust.
//!
//! Test groups:
//! - **Golden snapshots** (QA-201): exact node/edge counts and signatures for
//!   `tests/fixtures/rust/simple.rs`; hand-curated to catch regressions in the
//!   Phase A structural parser.
//! - **Smoke test** (QA-201): index real Travsr source files, assert liveness.
//! - **Idempotency** (QA-201): parsing the same file twice produces identical
//!   NodeIds and edges.
//! - **Import resolution** (INDEX-202): link_imports_rust correctness.

use std::path::Path;

use travsr_core::EdgeKind;
use travsr_indexer::{link_imports_rust, Indexer};

fn fixture_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust/simple.rs")
}

fn parse_fixture() -> travsr_indexer::ParseOutput {
    Indexer::new()
        .parse_file_with_vname(&fixture_path(), "src/simple.rs")
        .unwrap()
}

// ── link_imports_rust — self:: resolution ─────────────────────────────────────

#[test]
fn link_imports_rust_self_emits_resolves_to_edges() {
    // Synthetic nodes with self:: paths.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mod.rs");
    std::fs::write(&path, b"use self::storage;\nuse self::query::Engine;").unwrap();
    let out = Indexer::new()
        .parse_file_with_vname(&path, "src/mod.rs")
        .unwrap();

    let edges = link_imports_rust(&out.nodes, "src/mod.rs", "");

    // use:self::storage → src/storage.rs + src/storage/mod.rs
    assert!(
        edges.iter().any(|e| e.kind == EdgeKind::ResolvesTo),
        "expected at least one ResolvesTo edge"
    );
}

// ── link_imports_rust — super:: resolution ────────────────────────────────────

#[test]
fn link_imports_rust_super_traverses_parent_dir() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let path = sub.join("mod.rs");
    std::fs::write(&path, b"use super::utils;").unwrap();
    let out = Indexer::new()
        .parse_file_with_vname(&path, "src/sub/mod.rs")
        .unwrap();

    let edges = link_imports_rust(&out.nodes, "src/sub/mod.rs", "");

    // super::utils → src/utils.rs + src/utils/mod.rs
    let use_node = out
        .nodes
        .iter()
        .find(|n| n.vname.signature == "use:super::utils")
        .expect("expected use:super::utils node");

    assert!(
        edges
            .iter()
            .any(|e| e.src == use_node.id && e.kind == EdgeKind::ResolvesTo),
        "expected ResolvesTo from use:super::utils"
    );
}

// ── link_imports_rust — wildcard skipped ─────────────────────────────────────

#[test]
fn link_imports_rust_wildcard_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lib.rs");
    std::fs::write(&path, b"use crate::prelude::*;").unwrap();
    let out = Indexer::new()
        .parse_file_with_vname(&path, "src/lib.rs")
        .unwrap();

    // `use crate::prelude::*` is external (crate::) and wildcard — both skip.
    let edges = link_imports_rust(&out.nodes, "src/lib.rs", "");
    assert!(
        edges.iter().all(|e| e.kind != EdgeKind::ResolvesTo),
        "wildcard/crate paths must produce no ResolvesTo edges"
    );
}

// ── link_imports_rust — external crate skipped ───────────────────────────────

#[test]
fn link_imports_rust_external_crate_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lib.rs");
    std::fs::write(
        &path,
        b"use std::collections::HashMap;\nuse serde::Serialize;",
    )
    .unwrap();
    let out = Indexer::new()
        .parse_file_with_vname(&path, "src/lib.rs")
        .unwrap();

    let edges = link_imports_rust(&out.nodes, "src/lib.rs", "");
    assert!(
        edges.iter().all(|e| e.kind != EdgeKind::ResolvesTo),
        "external crate imports must produce no ResolvesTo edges"
    );
}

// ── link_imports_rust — file-module declaration ───────────────────────────────

#[test]
fn link_imports_rust_filemod_emits_two_candidates() {
    // The fixture has `mod helpers;` — link_imports_rust should produce
    // ResolvesTo edges for src/helpers.rs AND src/helpers/mod.rs.
    let out = parse_fixture();
    let edges = link_imports_rust(&out.nodes, "src/simple.rs", "my_corpus");

    let filemod_node = out
        .nodes
        .iter()
        .find(|n| n.kind == "file-module")
        .expect("expected a file-module node from mod helpers;");

    let resolves: Vec<_> = edges
        .iter()
        .filter(|e| e.src == filemod_node.id && e.kind == EdgeKind::ResolvesTo)
        .collect();

    assert_eq!(
        resolves.len(),
        2,
        "mod foo; must produce exactly 2 ResolvesTo candidates (foo.rs + foo/mod.rs)"
    );
}

// ── link_imports_rust — grouped use imports ───────────────────────────────────

#[test]
fn link_imports_rust_grouped_self_imports_resolve() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lib.rs");
    std::fs::write(&path, b"use self::{store, query};").unwrap();
    let out = Indexer::new()
        .parse_file_with_vname(&path, "src/lib.rs")
        .unwrap();

    // Both self::store and self::query should be present as import nodes.
    assert!(
        out.nodes
            .iter()
            .any(|n| n.vname.signature == "use:self::store"),
        "expected use:self::store"
    );
    assert!(
        out.nodes
            .iter()
            .any(|n| n.vname.signature == "use:self::query"),
        "expected use:self::query"
    );

    let edges = link_imports_rust(&out.nodes, "src/lib.rs", "");
    // Each self:: import → 2 candidates
    assert!(
        edges
            .iter()
            .filter(|e| e.kind == EdgeKind::ResolvesTo)
            .count()
            >= 4,
        "expected ≥4 ResolvesTo edges for two self:: imports (2 candidates each)"
    );
}

// ── link_imports_rust — chained super:: ──────────────────────────────────────

#[test]
fn link_imports_rust_double_super_traverses_two_levels() {
    let dir = tempfile::tempdir().unwrap();
    let sub_sub = dir.path().join("a").join("b");
    std::fs::create_dir_all(&sub_sub).unwrap();
    let path = sub_sub.join("mod.rs");
    std::fs::write(&path, b"use super::super::utils;").unwrap();
    let out = Indexer::new()
        .parse_file_with_vname(&path, "src/a/b/mod.rs")
        .unwrap();

    let edges = link_imports_rust(&out.nodes, "src/a/b/mod.rs", "");

    // super::super::utils from src/a/b/mod.rs → src/utils.rs + src/utils/mod.rs
    assert!(
        edges.iter().any(|e| e.kind == EdgeKind::ResolvesTo),
        "super::super:: should resolve two directory levels up"
    );
    assert_eq!(
        edges
            .iter()
            .filter(|e| e.kind == EdgeKind::ResolvesTo)
            .count(),
        2,
        "double super:: must produce exactly 2 ResolvesTo candidates"
    );
}

// ── use_as_clause — alias stripped ────────────────────────────────────────────

#[test]
fn use_as_clause_indexes_original_path_not_alias() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lib.rs");
    std::fs::write(&path, b"use self::engine::Core as CoreEngine;").unwrap();
    let out = Indexer::new()
        .parse_file_with_vname(&path, "src/lib.rs")
        .unwrap();

    assert!(
        out.nodes
            .iter()
            .any(|n| n.vname.signature == "use:self::engine::Core"),
        "use_as_clause must index the original path"
    );
    assert!(
        !out.nodes
            .iter()
            .any(|n| n.vname.signature.contains("CoreEngine")),
        "alias must not appear in the graph"
    );
}

// ── QA-201: Golden snapshots — simple.rs fixture ─────────────────────────────
//
// These tests are the regression gate for the Phase A structural Rust parser.
// If the fixture or the parser changes, these counts MUST be updated
// deliberately — a silent drift is a bug.
//
// Fixture: crates/travsr-indexer/tests/fixtures/rust/simple.rs
// Expected structure (hand-curated, verified 2026-05-23):
//   file×1  struct×2  enum×1  trait×1  impl×2  module×1  file-module×1
//   function×2  method×3  constant×1  static×1  import×3
//   = 19 nodes, 18 edges

/// Exact node count for the simple.rs fixture must remain 19.
/// Update this number deliberately when the fixture or parser changes.
#[test]
fn golden_simple_fixture_node_count() {
    let out = parse_fixture();
    assert_eq!(
        out.nodes.len(),
        19,
        "expected exactly 19 nodes from simple.rs — update golden if fixture changed.\n\
         Actual nodes:\n{}",
        out.nodes
            .iter()
            .map(|n| format!("  {:12}  {}", n.kind, n.vname.signature))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Exact edge count for the simple.rs fixture must remain 18.
#[test]
fn golden_simple_fixture_edge_count() {
    let out = parse_fixture();
    assert_eq!(
        out.edges.len(),
        18,
        "expected exactly 18 edges from simple.rs — update golden if fixture changed."
    );
}

/// Every expected (kind, signature) pair must be present in the simple.rs output.
/// This guards against regressions where a node kind is silently dropped.
#[test]
fn golden_simple_fixture_all_expected_nodes_present() {
    let out = parse_fixture();

    let expected: &[(&str, &str)] = &[
        ("file", "file"),
        ("struct", "struct:Config"),
        ("struct", "struct:Worker"),
        ("enum", "enum:Status"),
        ("trait", "trait:Processor"),
        ("impl", "impl:Config"),
        ("impl", "impl:Worker"),
        ("module", "mod:utils"),
        ("file-module", "filemod:helpers"),
        ("function", "fn:helper"),
        ("function", "fn:run"),
        ("method", "fn:Worker.new"),
        ("method", "fn:Worker.process"),
        ("method", "fn:Config.fmt"),
        ("constant", "const:MAX_RETRIES"),
        ("static", "static:APP_NAME"),
        ("import", "use:std::fmt"),
        ("import", "use:std::collections::HashMap"),
        ("import", "use:std::io"),
    ];

    for (kind, sig) in expected {
        assert!(
            out.nodes
                .iter()
                .any(|n| n.kind == *kind && n.vname.signature == *sig),
            "missing expected node  kind={kind:12}  sig={sig}\n\
             Actual nodes:\n{}",
            out.nodes
                .iter()
                .map(|n| format!("  {:12}  {}", n.kind, n.vname.signature))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}

/// No duplicate NodeIds in the simple.rs output.
/// Duplicates would corrupt BFS traversal (double-counting, phantom cycles).
#[test]
fn golden_simple_fixture_no_duplicate_node_ids() {
    let out = parse_fixture();
    let mut ids: Vec<_> = out.nodes.iter().map(|n| n.id).collect();
    ids.sort_unstable();
    let before = ids.len();
    ids.dedup();
    assert_eq!(
        ids.len(),
        before,
        "duplicate NodeIds detected in simple.rs output — dedup in rust::parse() may be broken"
    );
}

/// No duplicate edges (src, dst, kind) in the simple.rs output.
#[test]
fn golden_simple_fixture_no_duplicate_edges() {
    let out = parse_fixture();
    let mut keys: Vec<_> = out
        .edges
        .iter()
        .map(|e| (e.src, e.dst, e.kind))
        .collect();
    keys.sort_unstable_by_key(|(s, d, _)| (*s, *d));
    let before = keys.len();
    keys.dedup();
    assert_eq!(
        keys.len(),
        before,
        "duplicate edges detected in simple.rs output"
    );
}

/// No unexpected extra nodes in the simple.rs output.
///
/// Complements `golden_simple_fixture_all_expected_nodes_present` by catching
/// nodes that were added by the parser but are NOT in the expected set.
/// The count test also catches this, but this test names the offending signature
/// directly — much faster to diagnose on a regression.
#[test]
fn golden_simple_fixture_no_unexpected_nodes() {
    let out = parse_fixture();

    let expected_sigs: std::collections::BTreeSet<&str> = [
        "file",
        "struct:Config",
        "struct:Worker",
        "enum:Status",
        "trait:Processor",
        "impl:Config",
        "impl:Worker",
        "mod:utils",
        "filemod:helpers",
        "fn:helper",
        "fn:run",
        "fn:Worker.new",
        "fn:Worker.process",
        "fn:Config.fmt",
        "const:MAX_RETRIES",
        "static:APP_NAME",
        "use:std::fmt",
        "use:std::collections::HashMap",
        "use:std::io",
    ]
    .into_iter()
    .collect();

    let actual_sigs: std::collections::BTreeSet<&str> = out
        .nodes
        .iter()
        .map(|n| n.vname.signature.as_str())
        .collect();

    let unexpected: Vec<_> = actual_sigs.difference(&expected_sigs).collect();
    assert!(
        unexpected.is_empty(),
        "unexpected nodes in simple.rs output (update golden if fixture changed): {unexpected:?}"
    );
}

// ── QA-201: Idempotency — parsing the same file twice is stable ───────────────

/// Parsing the same file twice must produce identical NodeIds and edges.
/// This guards against non-deterministic VName hashing or PRNG-seeded IDs.
#[test]
fn idempotent_reindex_produces_identical_graph() {
    let first = parse_fixture();
    let second = parse_fixture();

    let mut ids_a: Vec<_> = first.nodes.iter().map(|n| n.id).collect();
    let mut ids_b: Vec<_> = second.nodes.iter().map(|n| n.id).collect();
    ids_a.sort_unstable();
    ids_b.sort_unstable();
    assert_eq!(
        ids_a, ids_b,
        "NodeIds differ between first and second parse"
    );

    let mut edges_a: Vec<_> = first.edges.iter().map(|e| (e.src, e.dst, e.kind)).collect();
    let mut edges_b: Vec<_> = second
        .edges
        .iter()
        .map(|e| (e.src, e.dst, e.kind))
        .collect();
    edges_a.sort_unstable_by_key(|(s, d, _)| (*s, *d));
    edges_b.sort_unstable_by_key(|(s, d, _)| (*s, *d));
    assert_eq!(
        edges_a, edges_b,
        "Edge (src, dst, kind) triples differ between first and second parse"
    );
}

// ── QA-201: Smoke test — index real Travsr source files ──────────────────────

/// Parse the actual travsr-core/src/lib.rs from this workspace and assert
/// liveness: at least one struct, one function, and no panics.
/// This is the minimal "Travsr indexes itself" smoke test.
/// CI budget: < 5s (single-file parse, no I/O beyond reading one file).
#[test]
fn smoke_index_travsr_core_lib() {
    let core_lib = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent() // crates/
        .and_then(|p| p.parent()) // workspace root
        .expect("could not find workspace root")
        .join("crates/travsr-core/src/lib.rs");

    // Skip gracefully if running in an environment without the full workspace
    // (e.g. a vendor-only CI that only ships travsr-indexer).
    if !core_lib.exists() {
        eprintln!("smoke_index_travsr_core_lib: skipped — {core_lib:?} not found");
        return;
    }

    let out = Indexer::new()
        .parse_file_with_vname(&core_lib, "crates/travsr-core/src/lib.rs")
        .expect("parse must not fail on real Travsr source");

    assert!(
        out.nodes.len() > 5,
        "expected > 5 nodes from travsr-core/src/lib.rs, got {}",
        out.nodes.len()
    );
    assert!(
        out.nodes
            .iter()
            .any(|n| n.kind == "struct" || n.kind == "function"),
        "expected at least one struct or function node in travsr-core/src/lib.rs"
    );
    assert!(
        out.edges.len() > 3,
        "expected > 3 edges from travsr-core/src/lib.rs, got {}",
        out.edges.len()
    );
}
