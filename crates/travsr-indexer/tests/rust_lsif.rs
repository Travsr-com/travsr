//! Golden snapshot + regression tests for Rust LSIF ingestion (QA-211).
//!
//! # Test groups
//!
//! 1. **Golden counts** — pinned node/edge counts for `simple.lsif.jsonl`.
//!    Hand-curated 2026-05-23. If these fail, the LSIF parser changed and
//!    the fixture (or the expected values) must be updated deliberately.
//!
//! 2. **Edge kind correctness** — all emitted edges must be `RefCall`.
//!    `definitions` item-edges must NOT produce `RefCall`.
//!
//! 3. **Idempotency** — parsing the same dump twice must produce identical
//!    edges (same sorted `(src, dst, kind)` triples).
//!
//! 4. **ADR-002 provenance** — `RefCall` (LSIF) has strictly higher PPR weight
//!    than `DefinesBinding` (Tree-sitter structural), confirming the semantic
//!    priority encoded in the graph.
//!
//! # Updating the fixture
//!
//! If the golden counts change intentionally (e.g. fixture extended):
//! 1. Run `cargo test -p travsr-indexer --test rust_lsif -- --nocapture`
//! 2. Read the "actual" lines in the failure messages
//! 3. Update the expected constants below and in the fixture file
//! 4. Commit both changes together with a comment explaining the update

use travsr_core::EdgeKind;
use travsr_indexer::lsif::{ingest_rust, ingest_rust_raw};

// ── Fixture ───────────────────────────────────────────────────────────────────

/// Golden LSIF dump — pinned as of 2026-05-23.
/// Format: rust-analyzer LSIF 0.4.3 (standard, no travsr_vname extension).
const GOLDEN_DUMP: &str = include_str!("fixtures/rust/simple.lsif.jsonl");

/// Expected edge count from the golden fixture.
/// 2 `item/references` edges → 2 RefCall edges.
/// The `item/definitions` edge must be ignored.
const GOLDEN_EDGE_COUNT: usize = 2;

const CORPUS: &str = "test-corpus";

// ── Group 1: Golden counts ────────────────────────────────────────────────────

#[test]
fn golden_edge_count() {
    let out = ingest_rust(GOLDEN_DUMP, CORPUS).expect("ingest must succeed");
    assert_eq!(
        out.edges.len(),
        GOLDEN_EDGE_COUNT,
        "expected {GOLDEN_EDGE_COUNT} RefCall edges from golden fixture; \
         got {}.\nActual edges:\n{:#?}",
        out.edges.len(),
        out.edges
    );
}

#[test]
fn golden_node_count_is_zero() {
    // LSIF adds edges only; Tree-sitter owns node definitions (ADR-002).
    let out = ingest_rust(GOLDEN_DUMP, CORPUS).expect("ingest must succeed");
    assert_eq!(
        out.nodes.len(),
        0,
        "ingest_rust must not emit nodes — got {}",
        out.nodes.len()
    );
}

// ── Group 2: Edge kind correctness ───────────────────────────────────────────

#[test]
fn all_golden_edges_are_ref_call() {
    let edges = ingest_rust_raw(GOLDEN_DUMP, CORPUS).expect("ingest must succeed");
    let non_ref_call: Vec<_> = edges
        .iter()
        .filter(|e| e.kind != EdgeKind::RefCall)
        .collect();
    assert!(
        non_ref_call.is_empty(),
        "all edges must be RefCall; unexpected: {non_ref_call:#?}"
    );
}

#[test]
fn definitions_item_edges_are_not_emitted() {
    // The golden fixture contains one `item` edge with `property: "definitions"`.
    // It must not contribute a RefCall edge — only "references" items do.
    let edges = ingest_rust_raw(GOLDEN_DUMP, CORPUS).expect("ingest must succeed");
    // If definitions were incorrectly emitted we'd get 3 edges, not 2.
    assert_eq!(
        edges.len(),
        GOLDEN_EDGE_COUNT,
        "definitions item-edges must be filtered; got {} edges",
        edges.len()
    );
}

// ── Group 3: Idempotency ──────────────────────────────────────────────────────

#[test]
fn idempotent_ingest_produces_identical_edges() {
    let mut first = ingest_rust_raw(GOLDEN_DUMP, CORPUS).expect("first ingest");
    let mut second = ingest_rust_raw(GOLDEN_DUMP, CORPUS).expect("second ingest");

    first.sort_by_key(|e| (e.src, e.dst, e.kind as u8));
    second.sort_by_key(|e| (e.src, e.dst, e.kind as u8));

    assert_eq!(
        first, second,
        "repeated ingest of the same dump must produce identical edges"
    );
}

// ── Group 4: ADR-002 provenance ───────────────────────────────────────────────

#[test]
fn lsif_ref_call_has_higher_ppr_weight_than_tree_sitter_defines_binding() {
    // ADR-002: LSIF semantic edges take priority over Tree-sitter structural
    // edges. PPR weight encodes this priority: RefCall (LSIF) > DefinesBinding
    // (Tree-sitter). The full ON CONFLICT upsert is tested in travsr-store.
    assert!(
        EdgeKind::RefCall.ppr_weight() > EdgeKind::DefinesBinding.ppr_weight(),
        "RefCall PPR weight ({}) must exceed DefinesBinding weight ({})",
        EdgeKind::RefCall.ppr_weight(),
        EdgeKind::DefinesBinding.ppr_weight()
    );
}

#[test]
fn corpus_is_stamped_into_caller_vname() {
    // Edges from two different corpora must have different src NodeIds.
    let edges_a = ingest_rust_raw(GOLDEN_DUMP, "corpus-a").expect("corpus-a");
    let edges_b = ingest_rust_raw(GOLDEN_DUMP, "corpus-b").expect("corpus-b");

    let srcs_a: std::collections::BTreeSet<_> = edges_a.iter().map(|e| e.src).collect();
    let srcs_b: std::collections::BTreeSet<_> = edges_b.iter().map(|e| e.src).collect();

    assert!(
        srcs_a.is_disjoint(&srcs_b),
        "caller NodeIds must differ across corpora — \
         corpus is not being stamped into caller VNames"
    );

    let dsts_a: std::collections::BTreeSet<_> = edges_a.iter().map(|e| e.dst).collect();
    let dsts_b: std::collections::BTreeSet<_> = edges_b.iter().map(|e| e.dst).collect();

    assert!(
        dsts_a.is_disjoint(&dsts_b),
        "callee NodeIds must also differ across corpora — \
         corpus is not being stamped into callee VNames"
    );
}
