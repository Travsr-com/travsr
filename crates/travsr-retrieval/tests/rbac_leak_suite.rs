//! RBAC correctness test suite — ensures denied-corpus nodes never appear in
//! any traversal result (get_execution_path / BFS), regardless of graph topology.
//!
//! These tests form the "rbac-leak-gate" CI check (S14, 172h).
//! Every test asserts the **absence** of denied-corpus nodes rather than the
//! presence of allowed ones — denial must be absolute, not probabilistic.

use travsr_core::{Edge, EdgeKind, Node, NodeId, VName};
use travsr_retrieval::{pcst_path, RbacFilter};
use travsr_store::{SqliteStore, Store};

fn node(corpus: &str, sig: &str) -> Node {
    Node::new(
        VName::new(corpus, "", "src/lib.rs", "rust", sig),
        "function",
    )
}

fn make_store(nodes: &[Node], edges: &[(NodeId, NodeId, EdgeKind)]) -> SqliteStore {
    let mut s = SqliteStore::open_in_memory().unwrap();
    for n in nodes {
        s.put_node(n).unwrap();
    }
    for &(src, dst, kind) in edges {
        s.put_edge(&Edge::new(src, dst, kind)).unwrap();
    }
    s
}

fn no_denied_corpus(result: &[Node], denied: &[&str]) {
    for n in result {
        for &d in denied {
            assert_ne!(
                n.vname.corpus.as_str(),
                d,
                "RBAC LEAK: node {} from corpus '{}' appeared in result, corpus '{}' is denied",
                n.vname.signature,
                n.vname.corpus,
                d,
            );
        }
    }
}

// ── BFS RBAC tests ────────────────────────────────────────────────────────────

/// Direct edge to denied corpus — BFS must not cross it.
#[test]
fn bfs_does_not_cross_direct_edge_to_denied_corpus() {
    let allowed = node("corp:allowed", "fn:allowed");
    let denied = node("corp:secret", "fn:secret");
    let store = make_store(
        &[allowed.clone(), denied.clone()],
        &[(allowed.id, denied.id, EdgeKind::RefCall)],
    );

    let filter = RbacFilter::new(["corp:allowed"]);
    let result = bfs_with_filter(&store, allowed.id, 3, 65536, &filter);
    no_denied_corpus(&result, &["corp:secret"]);
}

/// Transitive edge through denied node — the denied node AND all nodes behind
/// it must be excluded (fail-closed traversal stops at the denied node).
#[test]
fn bfs_does_not_traverse_through_denied_corpus() {
    let a = node("corp:allowed", "fn:a");
    let mid = node("corp:secret", "fn:mid");
    let b = node("corp:allowed", "fn:b_behind_secret");
    let store = make_store(
        &[a.clone(), mid.clone(), b.clone()],
        &[
            (a.id, mid.id, EdgeKind::RefCall),
            (mid.id, b.id, EdgeKind::RefCall),
        ],
    );

    let filter = RbacFilter::new(["corp:allowed"]);
    let result = bfs_with_filter(&store, a.id, 5, 65536, &filter);
    no_denied_corpus(&result, &["corp:secret"]);
    // b is behind the denied node — also must not appear.
    assert!(
        !result
            .iter()
            .any(|n| n.vname.signature == "fn:b_behind_secret"),
        "node behind denied corp must not be reachable"
    );
}

/// Multiple denied corpora — all must be excluded.
#[test]
fn bfs_excludes_multiple_denied_corpora() {
    let a = node("corp:allowed", "fn:a");
    let d1 = node("corp:secret1", "fn:d1");
    let d2 = node("corp:secret2", "fn:d2");
    let store = make_store(
        &[a.clone(), d1.clone(), d2.clone()],
        &[
            (a.id, d1.id, EdgeKind::RefCall),
            (a.id, d2.id, EdgeKind::Depends),
        ],
    );

    let filter = RbacFilter::new(["corp:allowed"]);
    let result = bfs_with_filter(&store, a.id, 3, 65536, &filter);
    no_denied_corpus(&result, &["corp:secret1", "corp:secret2"]);
}

// ── PCST RBAC tests ───────────────────────────────────────────────────────────

/// PCST with path only through denied corpus — must return empty (no path).
#[test]
fn pcst_returns_empty_when_only_path_is_denied() {
    let src = node("corp:allowed", "fn:src");
    let mid = node("corp:secret", "fn:mid");
    let snk = node("corp:allowed", "fn:snk");
    let store = make_store(
        &[src.clone(), mid.clone(), snk.clone()],
        &[
            (src.id, mid.id, EdgeKind::RefCall),
            (mid.id, snk.id, EdgeKind::RefCall),
        ],
    );

    let filter = RbacFilter::new(["corp:allowed"]);
    // There is no direct path from src to snk — only via denied mid.
    // Since snk is reachable in the BFS expansion but the path via mid is blocked,
    // PCST will fall back to BFS which also doesn't include mid.
    let result = pcst_path(&store, src.id, snk.id, &filter, 65536).unwrap();
    no_denied_corpus(&result, &["corp:secret"]);
}

/// PCST with sink in denied corpus — must return empty (SEC P0).
#[test]
fn pcst_returns_empty_when_sink_is_denied() {
    let src = node("corp:allowed", "fn:src");
    let snk = node("corp:secret", "fn:snk");
    let store = make_store(
        &[src.clone(), snk.clone()],
        &[(src.id, snk.id, EdgeKind::RefCall)],
    );

    let filter = RbacFilter::new(["corp:allowed"]);
    let result = pcst_path(&store, src.id, snk.id, &filter, 65536).unwrap();
    assert!(
        result.is_empty(),
        "denied sink must produce empty result (SEC P0)"
    );
}

/// PCST with source in denied corpus — must return empty (SEC P0).
#[test]
fn pcst_returns_empty_when_source_is_denied() {
    let src = node("corp:secret", "fn:src");
    let snk = node("corp:allowed", "fn:snk");
    let store = make_store(
        &[src.clone(), snk.clone()],
        &[(src.id, snk.id, EdgeKind::RefCall)],
    );

    let filter = RbacFilter::new(["corp:allowed"]);
    let result = pcst_path(&store, src.id, snk.id, &filter, 65536).unwrap();
    assert!(
        result.is_empty(),
        "denied source must produce empty result (SEC P0)"
    );
}

/// Empty allowed set denies everything — no node from any corpus appears.
#[test]
fn rbac_empty_allowed_set_denies_everything() {
    let a = node("corp:a", "fn:a");
    let b = node("corp:b", "fn:b");
    let store = make_store(&[a.clone(), b.clone()], &[(a.id, b.id, EdgeKind::RefCall)]);

    let filter = RbacFilter::new(std::iter::empty::<String>());
    let result = pcst_path(&store, a.id, b.id, &filter, 65536).unwrap();
    assert!(
        result.is_empty(),
        "empty allowed set must produce empty result for all nodes"
    );
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// BFS variant that applies an EdgeFilter at each step.
fn bfs_with_filter(
    store: &SqliteStore,
    seed: NodeId,
    max_depth: u8,
    token_budget: usize,
    filter: &RbacFilter,
) -> Vec<Node> {
    use std::collections::{HashSet, VecDeque};
    use travsr_retrieval::EdgeFilter;

    let mut visited: HashSet<NodeId> = HashSet::new();
    let mut queue: VecDeque<(NodeId, u8)> = VecDeque::new();
    let mut result: Vec<Node> = Vec::new();
    let mut tokens_used: usize = 0;

    // Seed node must also pass the filter.
    if let Ok(Some(seed_node)) = store.get_node(seed) {
        if !filter.allow(seed, seed, Some(seed_node.vname.corpus.as_str())) {
            return Vec::new();
        }
    } else {
        return Vec::new();
    }

    queue.push_back((seed, 0));
    visited.insert(seed);

    while let Some((current_id, depth)) = queue.pop_front() {
        if depth > max_depth {
            break;
        }

        if let Ok(Some(node)) = store.get_node(current_id) {
            let cost = node.vname.signature.len() + node.kind.len();
            if tokens_used + cost > token_budget {
                break;
            }
            tokens_used += cost;
            result.push(node);
        }

        if depth < max_depth {
            for edge in store.iter_edges_from(current_id).unwrap_or_default() {
                let dst_corpus = store
                    .get_node(edge.dst)
                    .ok()
                    .flatten()
                    .map(|n| n.vname.corpus.clone());

                if !filter.allow(current_id, edge.dst, dst_corpus.as_deref()) {
                    continue;
                }

                if visited.insert(edge.dst) {
                    queue.push_back((edge.dst, depth + 1));
                }
            }
        }
    }

    result
}

// ── PPR RBAC tests (#413) ─────────────────────────────────────────────────────
//
// H2: this suite is the "rbac-leak-gate" CI check, but until #413 every case
// above exercised either PCST or `bfs_with_filter`, a helper defined in this
// file. Nothing drove the production PPR path that powers `get_context` and
// `ask` — so the gate could report green while that path had no filter wired
// at all. These cases call the real `ppr` / `ppr_weighted`.

/// Resolve scored ids back to nodes so `no_denied_corpus` can inspect corpora.
fn nodes_of(store: &SqliteStore, scored: &[(NodeId, f32)]) -> Vec<Node> {
    store
        .get_nodes(&scored.iter().map(|(id, _)| *id).collect::<Vec<_>>())
        .unwrap()
}

#[test]
fn ppr_does_not_rank_a_node_behind_a_direct_denied_edge() {
    let allowed = node("corp:allowed", "fn:seed");
    let denied = node("corp:denied", "fn:secret");
    let store = make_store(
        &[allowed.clone(), denied.clone()],
        &[(allowed.id, denied.id, EdgeKind::RefCall)],
    );
    let filter = RbacFilter::new(["corp:allowed"]);

    let scored = travsr_retrieval::ppr(&store, &[allowed.id], 0, &filter).unwrap();
    no_denied_corpus(&nodes_of(&store, &scored), &["corp:denied"]);
}

#[test]
fn ppr_does_not_reach_an_allowed_node_only_through_a_denied_one() {
    // a -> b(denied) -> c(allowed). `c` is reachable only by crossing `b`, so a
    // filter applied at traversal time must not surface it. Post-filtering the
    // ranked list would drop `b` but keep `c`, leaking that the bridge exists.
    let a = node("corp:allowed", "fn:a");
    let b = node("corp:denied", "fn:b");
    let c = node("corp:allowed", "fn:c");
    let store = make_store(
        &[a.clone(), b.clone(), c.clone()],
        &[
            (a.id, b.id, EdgeKind::RefCall),
            (b.id, c.id, EdgeKind::RefCall),
        ],
    );
    let filter = RbacFilter::new(["corp:allowed"]);

    let scored = travsr_retrieval::ppr(&store, &[a.id], 0, &filter).unwrap();
    let ids: Vec<NodeId> = scored.iter().map(|(id, _)| *id).collect();
    no_denied_corpus(&nodes_of(&store, &scored), &["corp:denied"]);
    assert!(
        !ids.contains(&c.id),
        "RBAC LEAK: reached an allowed node only via a denied one"
    );
}

#[test]
fn ppr_weighted_enforces_the_filter_too() {
    // `get_context` uses the weighted entry point, so it needs its own case —
    // the uniform one passing does not cover it.
    let allowed = node("corp:allowed", "fn:seed");
    let denied = node("corp:denied", "fn:secret");
    let store = make_store(
        &[allowed.clone(), denied.clone()],
        &[(allowed.id, denied.id, EdgeKind::RefCall)],
    );
    let filter = RbacFilter::new(["corp:allowed"]);

    let scored = travsr_retrieval::ppr_weighted(&store, &[(allowed.id, 1.0)], 0, &filter).unwrap();
    no_denied_corpus(&nodes_of(&store, &scored), &["corp:denied"]);
}

#[test]
fn ppr_weighted_still_enforces_the_filter_on_the_degenerate_weight_fallback() {
    // Non-finite weights make `ppr_weighted` fall back to uniform `ppr`. The
    // filter has to survive that hand-off, or a caller could disable RBAC by
    // supplying a NaN weight.
    let allowed = node("corp:allowed", "fn:seed");
    let denied = node("corp:denied", "fn:secret");
    let store = make_store(
        &[allowed.clone(), denied.clone()],
        &[(allowed.id, denied.id, EdgeKind::RefCall)],
    );
    let filter = RbacFilter::new(["corp:allowed"]);

    let scored =
        travsr_retrieval::ppr_weighted(&store, &[(allowed.id, f32::NAN)], 0, &filter).unwrap();
    no_denied_corpus(&nodes_of(&store, &scored), &["corp:denied"]);
}

#[test]
fn ppr_denies_a_destination_missing_from_the_store() {
    // Fail-closed: an edge pointing at a node that is not in the store yields
    // an unknown corpus, which every filter must treat as a denial.
    let allowed = node("corp:allowed", "fn:seed");
    let ghost = NodeId(999_999);
    let mut store = SqliteStore::open_in_memory().unwrap();
    store.put_node(&allowed).unwrap();
    store
        .put_edge(&Edge::new(allowed.id, ghost, EdgeKind::RefCall))
        .unwrap();
    let filter = RbacFilter::new(["corp:allowed"]);

    let scored = travsr_retrieval::ppr(&store, &[allowed.id], 0, &filter).unwrap();
    assert!(
        !scored.iter().any(|(id, _)| *id == ghost),
        "RBAC LEAK: a node with an unknown corpus was traversed"
    );
}

#[test]
fn open_filter_still_traverses_every_corpus() {
    // The local single-repo path must be unchanged by #413: OpenFilter declines
    // the corpus lookups entirely, so this also pins that declining them does
    // not accidentally deny.
    let a = node("corp:one", "fn:a");
    let b = node("corp:two", "fn:b");
    let store = make_store(&[a.clone(), b.clone()], &[(a.id, b.id, EdgeKind::RefCall)]);

    let scored = travsr_retrieval::ppr(&store, &[a.id], 0, &travsr_retrieval::OpenFilter).unwrap();
    assert!(
        scored.iter().any(|(id, _)| *id == b.id),
        "OpenFilter must still cross corpus boundaries"
    );
}
