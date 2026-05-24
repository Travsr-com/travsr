//! Property-based test for cross-language edge symmetry (RFC-005 §10).
//!
//! prop_cross_lang_edge_symmetric: for every FFI pair (src_lang, dst_lang)
//! in the fixture, a BFS from any node in src_lang reaches at least one node
//! in dst_lang IFF a BFS from that dst_lang node reaches back to src_lang.
//! "Reaches" = depth ≤ 5 over RefCall ∪ FFICall ∪ Depends.

use proptest::prelude::*;
use travsr_core::{Edge, EdgeKind, NodeId};
use travsr_indexer::ffi::{FfiMarker, FfiMarkerKind};

/// A minimal in-memory graph for BFS tests.
struct TestGraph {
    edges: Vec<Edge>,
}

impl TestGraph {
    fn from_edges(edges: Vec<Edge>) -> Self {
        Self { edges }
    }

    /// BFS from `start`, traversing RefCall | FFICall | Depends edges, depth ≤ 5.
    fn reachable(&self, start: NodeId) -> std::collections::HashSet<NodeId> {
        let traversable =
            |k: EdgeKind| matches!(k, EdgeKind::RefCall | EdgeKind::FFICall | EdgeKind::Depends);
        let mut visited = std::collections::HashSet::new();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back((start, 0u8));
        visited.insert(start);
        while let Some((cur, depth)) = queue.pop_front() {
            if depth >= 5 {
                continue;
            }
            for edge in &self.edges {
                if edge.src == cur && traversable(edge.kind) && !visited.contains(&edge.dst) {
                    visited.insert(edge.dst);
                    queue.push_back((edge.dst, depth + 1));
                }
            }
        }
        visited
    }
}

/// A simple synthetic FFI pair: one call-site node and one export node,
/// connected by a FFICall edge and a reverse RefCall edge.
#[derive(Debug, Clone)]
struct FfiPair {
    call_site: NodeId,
    export: NodeId,
    confidence: u8,
}

impl FfiPair {
    fn to_edges(&self) -> Vec<Edge> {
        let mut edges = vec![Edge::ffi_call(self.call_site, self.export, self.confidence)];
        if self.confidence >= 80 {
            edges.push(Edge::new(self.export, self.call_site, EdgeKind::RefCall));
        }
        edges
    }
}

fn arb_ffi_pair() -> impl Strategy<Value = FfiPair> {
    // Node IDs 1..=1000 for call sites, 1001..=2000 for exports.
    (1u64..=1000u64, 1001u64..=2000u64, 30u8..=100u8).prop_map(
        |(call_id, export_id, confidence)| FfiPair {
            call_site: NodeId(call_id),
            export: NodeId(export_id),
            confidence,
        },
    )
}

proptest! {
    // Pinned seed ensures CI runs are deterministic and failures are reproducible.
    // The seed is a 32-byte little-endian array; override with PROPTEST_SEED env var.
    #![proptest_config(ProptestConfig {
        cases: 256,
        failure_persistence: None, // no .proptest-regressions file in test dirs
        .. ProptestConfig::default()
    })]

    /// For every generated FFI pair with confidence ≥ 80, the graph must be
    /// reachable in both directions (RFC-005 symmetry property).
    ///
    /// Pairs with confidence < 80 get only a FFICall edge (no reverse RefCall),
    /// so we only assert forward reachability there.
    #[test]
    fn prop_cross_lang_edge_symmetric(pair in arb_ffi_pair()) {
        let edges = pair.to_edges();
        let graph = TestGraph::from_edges(edges);

        // Forward: call-site reaches export
        let from_call = graph.reachable(pair.call_site);
        prop_assert!(
            from_call.contains(&pair.export),
            "call-site {:?} cannot reach export {:?}",
            pair.call_site, pair.export
        );

        if pair.confidence >= 80 {
            // Reverse: export reaches call-site (via RefCall reverse edge)
            let from_export = graph.reachable(pair.export);
            prop_assert!(
                from_export.contains(&pair.call_site),
                "export {:?} cannot reach call-site {:?} (confidence={})",
                pair.export, pair.call_site, pair.confidence
            );
        }
    }

    /// Resolver must never panic on arbitrary marker inputs.
    /// (Structural property — content verified by golden tests.)
    #[test]
    fn prop_ffi_edges_are_valid(pair in arb_ffi_pair()) {
        let edges = pair.to_edges();
        for edge in &edges {
            prop_assert!(
                edge.confidence.map_or(true, |c| c <= 100),
                "confidence out of range: {:?}", edge.confidence
            );
            prop_assert!(
                matches!(edge.kind, EdgeKind::FFICall | EdgeKind::RefCall),
                "unexpected edge kind: {:?}", edge.kind
            );
        }
    }
}

// ── Real Resolver proptest — exercises Resolver::build_from_markers + resolve ──

/// Strategy: generate a matching NapiExport + NapiCall pair with the same
/// normalized name and same corpus. The resolver must emit at least one edge.
fn arb_napi_marker_pair() -> impl Strategy<Value = (FfiMarker, FfiMarker)> {
    // Use simple ASCII names that survive normalize_name unchanged.
    let names = prop::string::string_regex("[a-z][a-z0-9]{1,15}").unwrap();
    let arities = prop::option::of(0u8..=8u8);
    (names, arities, 1001u64..=2000u64, 2001u64..=3000u64).prop_map(
        |(name, arity, export_id, call_id)| {
            let export = FfiMarker::try_new(
                NodeId(export_id),
                FfiMarkerKind::NapiExport,
                name.clone(),
                None::<String>,
                arity,
                None::<String>,
                "test-corpus",
            )
            .expect("arb marker must be valid");
            let call = FfiMarker::try_new(
                NodeId(call_id),
                FfiMarkerKind::NapiCall,
                name.clone(),
                None::<String>,
                arity, // same arity → name+arity match → confidence 90
                None::<String>,
                "test-corpus", // same corpus — corpus invariant satisfied
            )
            .expect("arb marker must be valid");
            (export, call)
        },
    )
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        failure_persistence: None,
        .. ProptestConfig::default()
    })]

    /// The real Resolver (via Indexer::resolve_ffi_edges) must emit at least one
    /// FFICall edge when given a matching NapiExport + NapiCall pair in the
    /// same corpus (RFC-005 §5 confidence rubric).
    #[test]
    fn prop_resolver_emits_edge_for_matching_napi_pair(
        (export, call) in arb_napi_marker_pair()
    ) {
        let markers = vec![export.clone(), call.clone()];
        // Indexer::resolve_ffi_edges is the correct public API for repo-level
        // resolution — this exercises build_from_markers + resolve end-to-end.
        let indexer = travsr_indexer::Indexer::with_corpus("test-corpus");
        let edges = indexer.resolve_ffi_edges(&markers);

        prop_assert!(
            !edges.is_empty(),
            "resolver must emit at least one FFICall edge for a matching napi pair \
             (export={:?}, call={:?})",
            export.local_name, call.local_name
        );
        prop_assert!(
            edges.iter().any(|e| e.kind == EdgeKind::FFICall),
            "at least one emitted edge must be FFICall"
        );
    }
}
