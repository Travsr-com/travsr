//! Parity test harness: SQLite vs Kùzu query oracle (QA-010).
//!
//! All tests are gated `#[cfg(feature = "kuzu")]` — the file compiles with
//! zero tests when the feature is absent, so normal CI is unaffected.

#[cfg(feature = "kuzu")]
use travsr_core::{Edge, EdgeKind, Node, NodeId, VName};
#[cfg(feature = "kuzu")]
use travsr_store::{SqliteStore, Store};
#[cfg(feature = "kuzu")]
use travsr_store::KuzuStore;

// ── Fixture helpers (all gated — only needed when kuzu feature is active) ─────

/// Build a Node with explicit corpus/root/language kept minimal.
#[cfg(feature = "kuzu")]
fn make_node(sig: &str, kind: &str, path: &str) -> Node {
    Node::new(
        VName::new("parity-corpus", "", path, "typescript", sig),
        kind,
    )
}

/// The five canonical nodes used by every parity test.
#[cfg(feature = "kuzu")]
struct ParityNodes {
    alpha:   Node,
    beta:    Node,
    gamma:   Node,
    service: Node,
    main:    Node,
}

#[cfg(feature = "kuzu")]
impl ParityNodes {
    fn new() -> Self {
        Self {
            alpha:   make_node("fn:alpha",      "function", "src/a.ts"),
            beta:    make_node("fn:beta",       "function", "src/b.ts"),
            gamma:   make_node("fn:gamma",      "function", "src/c.ts"),
            service: make_node("class:Service", "class",    "src/svc.ts"),
            main:    make_node("file:main",     "file",     "src/main.ts"),
        }
    }
}

/// Insert all 5 nodes and 7 edges into a SqliteStore.
#[cfg(feature = "kuzu")]
fn build_parity_graph_sqlite(store: &mut SqliteStore) {
    let n = ParityNodes::new();
    for node in [&n.alpha, &n.beta, &n.gamma, &n.service, &n.main] {
        store.put_node(node).expect("sqlite put_node");
    }
    let edges = parity_edges(&n);
    for e in &edges {
        store.put_edge(e).expect("sqlite put_edge");
    }
}

/// Insert all 5 nodes and 7 edges into a KuzuStore.
#[cfg(feature = "kuzu")]
fn build_parity_graph_kuzu(store: &mut KuzuStore) {
    let n = ParityNodes::new();
    for node in [&n.alpha, &n.beta, &n.gamma, &n.service, &n.main] {
        store.put_node(node).expect("kuzu put_node");
    }
    let edges = parity_edges(&n);
    for e in &edges {
        store.put_edge(e).expect("kuzu put_edge");
    }
}

/// The 7 canonical edges for the parity fixture.
#[cfg(feature = "kuzu")]
fn parity_edges(n: &ParityNodes) -> Vec<Edge> {
    vec![
        Edge::new(n.alpha.id,   n.beta.id,    EdgeKind::RefCall),
        Edge::new(n.alpha.id,   n.gamma.id,   EdgeKind::RefCall),
        Edge::new(n.beta.id,    n.gamma.id,   EdgeKind::RefCall),
        Edge::new(n.service.id, n.alpha.id,   EdgeKind::DefinesBinding),
        Edge::new(n.service.id, n.beta.id,    EdgeKind::DefinesBinding),
        Edge::new(n.main.id,    n.service.id, EdgeKind::Depends),
        Edge::new(n.main.id,    n.alpha.id,   EdgeKind::Depends),
    ]
}

// ── Order-independent comparison helpers ─────────────────────────────────────

#[cfg(feature = "kuzu")]
fn sorted_ids(edges: &[Edge]) -> Vec<(u64, u64)> {
    let mut v: Vec<_> = edges.iter().map(|e| (e.src.0, e.dst.0)).collect();
    v.sort_unstable();
    v
}

#[cfg(feature = "kuzu")]
fn sorted_node_ids(nodes: &[Node]) -> Vec<u64> {
    let mut v: Vec<_> = nodes.iter().map(|n| n.id.0).collect();
    v.sort_unstable();
    v
}

// ── Parity tests (all gated on feature = "kuzu") ──────────────────────────────

#[cfg(feature = "kuzu")]
#[test]
fn parity_node_count() {
    let mut sq = SqliteStore::open_in_memory().expect("sqlite open");
    build_parity_graph_sqlite(&mut sq);

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut kz = KuzuStore::open(tmp.path()).expect("kuzu open");
    build_parity_graph_kuzu(&mut kz);

    assert_eq!(
        sq.node_count().expect("sqlite node_count"),
        kz.node_count().expect("kuzu node_count"),
        "node_count must match"
    );
}

#[cfg(feature = "kuzu")]
#[test]
fn parity_edge_count() {
    let mut sq = SqliteStore::open_in_memory().expect("sqlite open");
    build_parity_graph_sqlite(&mut sq);

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut kz = KuzuStore::open(tmp.path()).expect("kuzu open");
    build_parity_graph_kuzu(&mut kz);

    assert_eq!(
        sq.edge_count().expect("sqlite edge_count"),
        kz.edge_count().expect("kuzu edge_count"),
        "edge_count must match"
    );
}

#[cfg(feature = "kuzu")]
#[test]
fn parity_get_node_alpha() {
    let n = ParityNodes::new();

    let mut sq = SqliteStore::open_in_memory().expect("sqlite open");
    build_parity_graph_sqlite(&mut sq);

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut kz = KuzuStore::open(tmp.path()).expect("kuzu open");
    build_parity_graph_kuzu(&mut kz);

    let sq_node = sq.get_node(n.alpha.id).expect("sqlite get_node").expect("sqlite: alpha missing");
    let kz_node = kz.get_node(n.alpha.id).expect("kuzu get_node").expect("kuzu: alpha missing");

    assert_eq!(sq_node, kz_node, "get_node(alpha) must return same Node");
}

#[cfg(feature = "kuzu")]
#[test]
fn parity_get_node_service() {
    let n = ParityNodes::new();

    let mut sq = SqliteStore::open_in_memory().expect("sqlite open");
    build_parity_graph_sqlite(&mut sq);

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut kz = KuzuStore::open(tmp.path()).expect("kuzu open");
    build_parity_graph_kuzu(&mut kz);

    let sq_node = sq.get_node(n.service.id).expect("sqlite get_node").expect("sqlite: service missing");
    let kz_node = kz.get_node(n.service.id).expect("kuzu get_node").expect("kuzu: service missing");

    assert_eq!(sq_node, kz_node, "get_node(service) must return same Node");
}

#[cfg(feature = "kuzu")]
#[test]
fn parity_get_node_missing() {
    let mut sq = SqliteStore::open_in_memory().expect("sqlite open");
    build_parity_graph_sqlite(&mut sq);

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut kz = KuzuStore::open(tmp.path()).expect("kuzu open");
    build_parity_graph_kuzu(&mut kz);

    let missing_id = NodeId(999_999);
    assert!(
        sq.get_node(missing_id).expect("sqlite get_node missing").is_none(),
        "sqlite: NodeId(999999) must return None"
    );
    assert!(
        kz.get_node(missing_id).expect("kuzu get_node missing").is_none(),
        "kuzu: NodeId(999999) must return None"
    );
}

#[cfg(feature = "kuzu")]
#[test]
fn parity_iter_edges_from_alpha() {
    let n = ParityNodes::new();

    let mut sq = SqliteStore::open_in_memory().expect("sqlite open");
    build_parity_graph_sqlite(&mut sq);

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut kz = KuzuStore::open(tmp.path()).expect("kuzu open");
    build_parity_graph_kuzu(&mut kz);

    let sq_edges = sq.iter_edges_from(n.alpha.id).expect("sqlite iter_edges_from alpha");
    let kz_edges = kz.iter_edges_from(n.alpha.id).expect("kuzu iter_edges_from alpha");

    assert_eq!(
        sorted_ids(&sq_edges),
        sorted_ids(&kz_edges),
        "iter_edges_from(alpha) edge sets must match"
    );
}

#[cfg(feature = "kuzu")]
#[test]
fn parity_iter_edges_from_main() {
    let n = ParityNodes::new();

    let mut sq = SqliteStore::open_in_memory().expect("sqlite open");
    build_parity_graph_sqlite(&mut sq);

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut kz = KuzuStore::open(tmp.path()).expect("kuzu open");
    build_parity_graph_kuzu(&mut kz);

    let sq_edges = sq.iter_edges_from(n.main.id).expect("sqlite iter_edges_from main");
    let kz_edges = kz.iter_edges_from(n.main.id).expect("kuzu iter_edges_from main");

    assert_eq!(
        sorted_ids(&sq_edges),
        sorted_ids(&kz_edges),
        "iter_edges_from(main) edge sets must match"
    );
}

#[cfg(feature = "kuzu")]
#[test]
fn parity_iter_edges_from_leaf() {
    let n = ParityNodes::new();

    let mut sq = SqliteStore::open_in_memory().expect("sqlite open");
    build_parity_graph_sqlite(&mut sq);

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut kz = KuzuStore::open(tmp.path()).expect("kuzu open");
    build_parity_graph_kuzu(&mut kz);

    let sq_edges = sq.iter_edges_from(n.gamma.id).expect("sqlite iter_edges_from gamma");
    let kz_edges = kz.iter_edges_from(n.gamma.id).expect("kuzu iter_edges_from gamma");

    assert!(sq_edges.is_empty(), "sqlite: gamma has no outgoing edges");
    assert!(kz_edges.is_empty(), "kuzu: gamma has no outgoing edges");
}

#[cfg(feature = "kuzu")]
#[test]
fn parity_iter_edges_to_gamma() {
    let n = ParityNodes::new();

    let mut sq = SqliteStore::open_in_memory().expect("sqlite open");
    build_parity_graph_sqlite(&mut sq);

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut kz = KuzuStore::open(tmp.path()).expect("kuzu open");
    build_parity_graph_kuzu(&mut kz);

    let sq_edges = sq.iter_edges_to(n.gamma.id).expect("sqlite iter_edges_to gamma");
    let kz_edges = kz.iter_edges_to(n.gamma.id).expect("kuzu iter_edges_to gamma");

    assert_eq!(sq_edges.len(), 2, "sqlite: gamma must have 2 incoming edges");
    assert_eq!(kz_edges.len(), 2, "kuzu: gamma must have 2 incoming edges");
    assert_eq!(
        sorted_ids(&sq_edges),
        sorted_ids(&kz_edges),
        "iter_edges_to(gamma) must match"
    );
}

#[cfg(feature = "kuzu")]
#[test]
fn parity_iter_edges_to_alpha() {
    let n = ParityNodes::new();

    let mut sq = SqliteStore::open_in_memory().expect("sqlite open");
    build_parity_graph_sqlite(&mut sq);

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut kz = KuzuStore::open(tmp.path()).expect("kuzu open");
    build_parity_graph_kuzu(&mut kz);

    let sq_edges = sq.iter_edges_to(n.alpha.id).expect("sqlite iter_edges_to alpha");
    let kz_edges = kz.iter_edges_to(n.alpha.id).expect("kuzu iter_edges_to alpha");

    // Service→alpha (DefinesBinding) and main→alpha (Depends)
    assert_eq!(sq_edges.len(), 2, "sqlite: alpha must have 2 incoming edges");
    assert_eq!(kz_edges.len(), 2, "kuzu: alpha must have 2 incoming edges");
    assert_eq!(
        sorted_ids(&sq_edges),
        sorted_ids(&kz_edges),
        "iter_edges_to(alpha) must match"
    );
}

#[cfg(feature = "kuzu")]
#[test]
fn parity_iter_edges_to_root() {
    let n = ParityNodes::new();

    let mut sq = SqliteStore::open_in_memory().expect("sqlite open");
    build_parity_graph_sqlite(&mut sq);

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut kz = KuzuStore::open(tmp.path()).expect("kuzu open");
    build_parity_graph_kuzu(&mut kz);

    let sq_edges = sq.iter_edges_to(n.main.id).expect("sqlite iter_edges_to main");
    let kz_edges = kz.iter_edges_to(n.main.id).expect("kuzu iter_edges_to main");

    assert!(sq_edges.is_empty(), "sqlite: main has no incoming edges");
    assert!(kz_edges.is_empty(), "kuzu: main has no incoming edges");
}

#[cfg(feature = "kuzu")]
#[test]
fn parity_search_alpha() {
    let n = ParityNodes::new();

    let mut sq = SqliteStore::open_in_memory().expect("sqlite open");
    build_parity_graph_sqlite(&mut sq);

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut kz = KuzuStore::open(tmp.path()).expect("kuzu open");
    build_parity_graph_kuzu(&mut kz);

    let sq_results = sq.search_nodes_by_name("alpha").expect("sqlite search alpha");
    let kz_results = kz.search_nodes_by_name("alpha").expect("kuzu search alpha");

    assert_eq!(sq_results.len(), 1, "sqlite: search 'alpha' must return 1 node");
    assert_eq!(kz_results.len(), 1, "kuzu: search 'alpha' must return 1 node");
    assert_eq!(sq_results[0].id, n.alpha.id, "sqlite: must find fn:alpha");
    assert_eq!(kz_results[0].id, n.alpha.id, "kuzu: must find fn:alpha");
}

#[cfg(feature = "kuzu")]
#[test]
fn parity_search_service() {
    let n = ParityNodes::new();

    let mut sq = SqliteStore::open_in_memory().expect("sqlite open");
    build_parity_graph_sqlite(&mut sq);

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut kz = KuzuStore::open(tmp.path()).expect("kuzu open");
    build_parity_graph_kuzu(&mut kz);

    let sq_results = sq.search_nodes_by_name("Service").expect("sqlite search Service");
    let kz_results = kz.search_nodes_by_name("Service").expect("kuzu search Service");

    assert_eq!(sq_results.len(), 1, "sqlite: search 'Service' must return 1 node");
    assert_eq!(kz_results.len(), 1, "kuzu: search 'Service' must return 1 node");
    assert_eq!(sq_results[0].id, n.service.id, "sqlite: must find class:Service");
    assert_eq!(kz_results[0].id, n.service.id, "kuzu: must find class:Service");
}

#[cfg(feature = "kuzu")]
#[test]
fn parity_search_partial_fn() {
    let mut sq = SqliteStore::open_in_memory().expect("sqlite open");
    build_parity_graph_sqlite(&mut sq);

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut kz = KuzuStore::open(tmp.path()).expect("kuzu open");
    build_parity_graph_kuzu(&mut kz);

    let sq_results = sq.search_nodes_by_name("fn:").expect("sqlite search fn:");
    let kz_results = kz.search_nodes_by_name("fn:").expect("kuzu search fn:");

    assert_eq!(sq_results.len(), 3, "sqlite: search 'fn:' must return 3 function nodes");
    assert_eq!(kz_results.len(), 3, "kuzu: search 'fn:' must return 3 function nodes");
    assert_eq!(
        sorted_node_ids(&sq_results),
        sorted_node_ids(&kz_results),
        "search 'fn:' node ID sets must match"
    );
}

#[cfg(feature = "kuzu")]
#[test]
fn parity_search_missing() {
    let mut sq = SqliteStore::open_in_memory().expect("sqlite open");
    build_parity_graph_sqlite(&mut sq);

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut kz = KuzuStore::open(tmp.path()).expect("kuzu open");
    build_parity_graph_kuzu(&mut kz);

    let sq_results = sq.search_nodes_by_name("zzz_nonexistent").expect("sqlite search missing");
    let kz_results = kz.search_nodes_by_name("zzz_nonexistent").expect("kuzu search missing");

    assert!(sq_results.is_empty(), "sqlite: nonexistent search must return empty");
    assert!(kz_results.is_empty(), "kuzu: nonexistent search must return empty");
}

#[cfg(feature = "kuzu")]
#[test]
fn parity_search_by_path() {
    let n = ParityNodes::new();

    let mut sq = SqliteStore::open_in_memory().expect("sqlite open");
    build_parity_graph_sqlite(&mut sq);

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut kz = KuzuStore::open(tmp.path()).expect("kuzu open");
    build_parity_graph_kuzu(&mut kz);

    let sq_results = sq.search_nodes_by_name("src/a.ts").expect("sqlite search by path");
    let kz_results = kz.search_nodes_by_name("src/a.ts").expect("kuzu search by path");

    assert_eq!(sq_results.len(), 1, "sqlite: path search must return 1 node");
    assert_eq!(kz_results.len(), 1, "kuzu: path search must return 1 node");
    assert_eq!(sq_results[0].id, n.alpha.id, "sqlite: path src/a.ts must find fn:alpha");
    assert_eq!(kz_results[0].id, n.alpha.id, "kuzu: path src/a.ts must find fn:alpha");
}

#[cfg(feature = "kuzu")]
#[test]
fn parity_all_nodes_count() {
    let mut sq = SqliteStore::open_in_memory().expect("sqlite open");
    build_parity_graph_sqlite(&mut sq);

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut kz = KuzuStore::open(tmp.path()).expect("kuzu open");
    build_parity_graph_kuzu(&mut kz);

    let sq_nodes = sq.all_nodes().expect("sqlite all_nodes");
    let kz_nodes = kz.all_nodes().expect("kuzu all_nodes");

    assert_eq!(sq_nodes.len(), 5, "sqlite: all_nodes must return 5");
    assert_eq!(kz_nodes.len(), 5, "kuzu: all_nodes must return 5");
}

#[cfg(feature = "kuzu")]
#[test]
fn parity_all_nodes_ids_match() {
    let mut sq = SqliteStore::open_in_memory().expect("sqlite open");
    build_parity_graph_sqlite(&mut sq);

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut kz = KuzuStore::open(tmp.path()).expect("kuzu open");
    build_parity_graph_kuzu(&mut kz);

    let sq_nodes = sq.all_nodes().expect("sqlite all_nodes");
    let kz_nodes = kz.all_nodes().expect("kuzu all_nodes");

    assert_eq!(
        sorted_node_ids(&sq_nodes),
        sorted_node_ids(&kz_nodes),
        "all_nodes NodeId sets must match between SQLite and Kùzu"
    );
}

#[cfg(feature = "kuzu")]
#[test]
fn parity_all_edges_count() {
    let mut sq = SqliteStore::open_in_memory().expect("sqlite open");
    build_parity_graph_sqlite(&mut sq);

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut kz = KuzuStore::open(tmp.path()).expect("kuzu open");
    build_parity_graph_kuzu(&mut kz);

    let sq_edge_count = sq.all_edges().expect("sqlite all_edges").len();

    // KuzuStore has no all_edges(); sum iter_edges_from over all nodes.
    let all_nodes_kuzu = kz.all_nodes().expect("kuzu all_nodes for edge count");
    let kz_edge_count: usize = all_nodes_kuzu
        .iter()
        .map(|n| {
            kz.iter_edges_from(n.id)
                .expect("kuzu iter_edges_from for edge sum")
                .len()
        })
        .sum();

    assert_eq!(sq_edge_count, 7, "sqlite: all_edges must return 7");
    assert_eq!(kz_edge_count, 7, "kuzu: summed edge count must be 7");
    assert_eq!(
        sq_edge_count, kz_edge_count,
        "total edge count must match between SQLite and Kùzu"
    );
}

#[cfg(feature = "kuzu")]
#[test]
fn parity_double_put_node_idempotent() {
    let n = ParityNodes::new();

    let mut sq = SqliteStore::open_in_memory().expect("sqlite open");
    sq.put_node(&n.alpha).expect("sqlite put_node 1");
    sq.put_node(&n.alpha).expect("sqlite put_node 2 (duplicate)");

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut kz = KuzuStore::open(tmp.path()).expect("kuzu open");
    kz.put_node(&n.alpha).expect("kuzu put_node 1");
    kz.put_node(&n.alpha).expect("kuzu put_node 2 (duplicate)");

    assert_eq!(
        sq.node_count().expect("sqlite node_count"),
        1,
        "sqlite: double put_node must keep node_count at 1"
    );
    assert_eq!(
        kz.node_count().expect("kuzu node_count"),
        1,
        "kuzu: double put_node must keep node_count at 1"
    );
}

#[cfg(feature = "kuzu")]
#[test]
fn parity_put_edge_idempotent() {
    let n = ParityNodes::new();
    let edge = Edge::new(n.alpha.id, n.beta.id, EdgeKind::RefCall);

    let mut sq = SqliteStore::open_in_memory().expect("sqlite open");
    sq.put_node(&n.alpha).expect("sqlite put alpha");
    sq.put_node(&n.beta).expect("sqlite put beta");
    sq.put_edge(&edge).expect("sqlite put_edge 1");
    sq.put_edge(&edge).expect("sqlite put_edge 2 (duplicate)");

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut kz = KuzuStore::open(tmp.path()).expect("kuzu open");
    kz.put_node(&n.alpha).expect("kuzu put alpha");
    kz.put_node(&n.beta).expect("kuzu put beta");
    kz.put_edge(&edge).expect("kuzu put_edge 1");
    kz.put_edge(&edge).expect("kuzu put_edge 2 (duplicate)");

    assert_eq!(
        sq.edge_count().expect("sqlite edge_count"),
        1,
        "sqlite: duplicate put_edge must keep edge_count at 1"
    );
    let kz_edges = kz
        .iter_edges_from(n.alpha.id)
        .expect("kuzu iter_edges_from alpha");
    assert_eq!(
        kz_edges.len(),
        1,
        "kuzu: duplicate put_edge must result in 1 edge from alpha"
    );
}

#[cfg(feature = "kuzu")]
#[test]
fn parity_edges_from_after_add() {
    let n = ParityNodes::new();

    let mut sq = SqliteStore::open_in_memory().expect("sqlite open");
    build_parity_graph_sqlite(&mut sq);

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut kz = KuzuStore::open(tmp.path()).expect("kuzu open");
    build_parity_graph_kuzu(&mut kz);

    // Add a new edge: beta → service (not in the original fixture)
    let new_edge = Edge::new(n.beta.id, n.service.id, EdgeKind::Depends);
    sq.put_edge(&new_edge).expect("sqlite add new edge");
    kz.put_edge(&new_edge).expect("kuzu add new edge");

    let sq_edges = sq.iter_edges_from(n.beta.id).expect("sqlite iter_edges_from beta after add");
    let kz_edges = kz.iter_edges_from(n.beta.id).expect("kuzu iter_edges_from beta after add");

    assert_eq!(
        sorted_ids(&sq_edges),
        sorted_ids(&kz_edges),
        "iter_edges_from(beta) after adding a new edge must match"
    );
    // beta originally had 1 outgoing edge (beta→gamma); now it should have 2
    assert_eq!(sq_edges.len(), 2, "sqlite: beta must have 2 outgoing edges after add");
    assert_eq!(kz_edges.len(), 2, "kuzu: beta must have 2 outgoing edges after add");
}
