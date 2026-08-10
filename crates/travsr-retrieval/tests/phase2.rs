//! Phase 2 exit-criteria correctness tests (S7-2).
//!
//! These tests run against the in-memory SQLite store so no fixture files are
//! required.  They are the gating assertions for Issue #33.

use travsr_core::{Edge, EdgeKind, Node, NodeId, VName};
use travsr_retrieval::{ppr, OpenFilter};
use travsr_store::{SqliteStore, Store};

fn make_node(sig: &str) -> Node {
    Node::new(
        VName::new("test-corpus", "", format!("{sig}.ts"), "typescript", sig),
        "function",
    )
}

/// Build a 1 000-node fan graph: hub → 999 leaves via DefinesBinding.
/// Returns (store, hub_id, leaf_ids).
fn build_1k_fan() -> (SqliteStore, NodeId, Vec<NodeId>) {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let hub = make_node("hub");
    store.put_node(&hub).unwrap();
    let mut leaf_ids = Vec::with_capacity(999);
    for i in 0..999usize {
        let leaf = make_node(&format!("leaf_{i}"));
        store.put_node(&leaf).unwrap();
        store
            .put_edge(&Edge::new(hub.id, leaf.id, EdgeKind::DefinesBinding))
            .unwrap();
        leaf_ids.push(leaf.id);
    }
    (store, hub.id, leaf_ids)
}

// ── PPR top-k ─────────────────────────────────────────────────────────────────

/// PPR seeded at the hub of the 1k fan must rank hub as the top node.
///
/// Because all 999 leaves are dangling (no outgoing edges), teleportation mass
/// is the dominant term for the hub.  The hub's personalisation score is
/// `1/|seeds| = 1.0`, so convergence leaves hub as the highest-ranked node
/// regardless of `k`.
#[test]
fn ppr_top_k_hub_is_highest_on_1k_fan() {
    let (store, hub_id, _) = build_1k_fan();
    let scores = ppr(&store, &[hub_id], 10, &OpenFilter).expect("PPR must succeed");
    assert!(
        !scores.is_empty(),
        "PPR must return at least one scored node"
    );
    assert_eq!(
        scores[0].0, hub_id,
        "hub must be the top-ranked node on the 1k fan fixture"
    );
}

/// PPR with k = 0 (return all scored nodes) must return all 1 000 nodes.
#[test]
fn ppr_returns_all_nodes_when_k_is_zero() {
    let (store, hub_id, _) = build_1k_fan();
    let scores = ppr(&store, &[hub_id], 0, &OpenFilter).expect("PPR must succeed");
    assert_eq!(
        scores.len(),
        1_000,
        "k=0 must return all 1 000 reachable nodes"
    );
}

/// PPR with k = 5 must return exactly 5 nodes, sorted descending.
#[test]
fn ppr_top_5_descending() {
    let (store, hub_id, _) = build_1k_fan();
    let scores = ppr(&store, &[hub_id], 5, &OpenFilter).expect("PPR must succeed");
    assert_eq!(scores.len(), 5);
    for window in scores.windows(2) {
        assert!(
            window[0].1 >= window[1].1,
            "scores must be non-increasing: {} < {}",
            window[0].1,
            window[1].1
        );
    }
}

// ── BFS depth gate ────────────────────────────────────────────────────────────

/// BFS from the hub at depth 1 on the 1k fan must return all 1 000 nodes.
#[test]
fn bfs_1k_fan_depth_1_returns_all_nodes() {
    use travsr_retrieval::bfs;
    let (store, hub_id, _) = build_1k_fan();
    let result = bfs(&store, hub_id, 1, usize::MAX).expect("BFS must succeed");
    assert_eq!(
        result.len(),
        1_000,
        "BFS depth-1 must visit all 1 000 nodes"
    );
}
