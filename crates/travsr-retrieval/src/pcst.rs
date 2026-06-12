//! Prize-Collecting Steiner Tree (PCST) retrieval — S14 approximation.
//!
//! Finds a low-cost subgraph connecting `source` to `sink` through the indexed
//! graph, applying the `EdgeFilter` at every traversal step (RBAC enforcement).
//!
//! # Algorithm (ADR-007)
//! S14 ships a practical PCST approximation:
//! 1. Build a bounded local subgraph via bidirectional BFS from source + sink (depth 5).
//! 2. Map to a petgraph `DiGraph` with edge costs = `1.0 - ppr_weight`.
//! 3. Run Dijkstra from source; extract the minimum-cost path to sink.
//! 4. Include nodes within λ of the optimal path cost (λ = 0.5, ADR-007).
//! 5. On timeout (> 80ms) or no path found: fall back to BFS depth-3.
//!
//! Full Goemans-Williamson (1995) is deferred to S16 when benchmark data
//! from real repos is available to tune λ.
//!
//! # Complexity
//! O((V + E) log V) for the Dijkstra pass over the local subgraph,
//! where V and E are bounded by the BFS expansion depth (≤ 5).

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use petgraph::algo::{astar, dijkstra};
use petgraph::graph::{DiGraph, NodeIndex};
use travsr_core::{Node, NodeId};
use travsr_store::{SqliteStore, Store};

use crate::rbac::EdgeFilter;

/// PCST penalty parameter λ (ADR-007). Controls node-exclusion penalty weight.
const PCST_LAMBDA: f32 = 0.5;

/// Hard wall-clock timeout for the PCST pass. On expiry, falls back to BFS.
const PCST_TIMEOUT_MS: u128 = 80;

/// Maximum nodes in the local subgraph built by the bidirectional BFS expansion.
const MAX_LOCAL_NODES: usize = 2_000;

/// BFS expansion depth for the local subgraph.
const EXPAND_DEPTH: u8 = 5;

/// Find a path from `source` to `sink`, respecting `filter`, within `token_budget`.
///
/// Returns nodes on (or near) the optimal PCST path, in approximate traversal
/// order. Falls back to BFS depth-3 on timeout or when no path is found.
///
/// The returned vector includes both `source` and `sink` when they exist and
/// are accessible. Returns an empty vector when either node is not found (SEC
/// P0: identical response for "not found" and "access denied").
pub fn pcst_path(
    store: &SqliteStore,
    source: NodeId,
    sink: NodeId,
    filter: &dyn EdgeFilter,
    token_budget: usize,
) -> Result<Vec<Node>, travsr_error::TravsrError> {
    let start = Instant::now();

    // Validate both endpoints are accessible via the filter.
    let source_node = match store.get_node(source)? {
        Some(n) if filter.allow(source, source, Some(n.vname.corpus.as_str())) => n,
        _ => return Ok(Vec::new()), // SEC P0: not found == access denied
    };
    let sink_node = match store.get_node(sink)? {
        Some(n) if filter.allow(sink, sink, Some(n.vname.corpus.as_str())) => n,
        _ => return Ok(Vec::new()), // SEC P0: not found == access denied
    };

    // Special case: source == sink.
    if source == sink {
        return Ok(vec![source_node]);
    }

    // Build local subgraph via bidirectional BFS expansion.
    let local = expand_local_subgraph(store, source, sink, filter, EXPAND_DEPTH);

    if start.elapsed().as_millis() > PCST_TIMEOUT_MS {
        tracing::warn!(
            src = source.0,
            sink = sink.0,
            "pcst: local expansion timed out — falling back to BFS"
        );
        return bfs_fallback(store, source, filter, token_budget);
    }

    // Build petgraph from local subgraph.
    let (graph, node_to_idx, idx_to_node) = build_graph(&local.nodes, &local.edges);

    let Some(&src_idx) = node_to_idx.get(&source) else {
        return bfs_fallback(store, source, filter, token_budget);
    };
    let Some(&dst_idx) = node_to_idx.get(&sink) else {
        return bfs_fallback(store, source, filter, token_budget);
    };

    // Dijkstra from source: cost = 1.0 - ppr_weight (lower = more traversable).
    // Determinism: pass `None` as the goal so the FULL local component settles.
    // With an early-exit goal, zero-cost ref/call edges leave many nodes tied
    // at cost 0 and WHICH of them settle before the sink varies with HashMap
    // iteration order run-to-run. The component is capped at MAX_LOCAL_NODES
    // (2000), so the extra work is negligible vs PCST_TIMEOUT_MS.
    let costs = dijkstra(&graph, src_idx, None, |e| *e.weight());

    // Reconstruct the actual source→sink route. The route must LEAD the result:
    // downstream consumers truncate (token budget here, 4 KiB sanitizer at the
    // MCP boundary), and ref/call edges cost 0 (ppr_weight 1.0) so the λ
    // threshold can admit the source's whole zero-cost component — a sink
    // appended after that blob would be cut off every time.
    let Some((total_cost, route)) = astar(
        &graph,
        src_idx,
        |idx| idx == dst_idx,
        |e| *e.weight(),
        |_| 0.0,
    ) else {
        tracing::debug!(
            src = source.0,
            sink = sink.0,
            "pcst: no path found — falling back to BFS"
        );
        return bfs_fallback(store, source, filter, token_budget);
    };

    if start.elapsed().as_millis() > PCST_TIMEOUT_MS {
        tracing::warn!(
            src = source.0,
            sink = sink.0,
            "pcst: Dijkstra timed out — falling back to BFS"
        );
        return bfs_fallback(store, source, filter, token_budget);
    }

    // Route nodes first, in traversal order (source → … → sink).
    let mut result_ids: Vec<NodeId> = route
        .iter()
        .filter_map(|idx| idx_to_node.get(idx).copied())
        .collect();
    let on_route: HashSet<NodeId> = result_ids.iter().copied().collect();

    // Then context: nodes whose cheapest-path cost is within λ × total_cost of
    // optimal, cheapest first (deterministic — `costs` is a HashMap).
    let threshold = total_cost * (1.0 + PCST_LAMBDA);
    let mut context: Vec<(f32, NodeId)> = costs
        .iter()
        .filter(|(_, &c)| c <= threshold)
        .filter_map(|(idx, &c)| idx_to_node.get(idx).map(|&id| (c, id)))
        .filter(|(_, id)| !on_route.contains(id))
        .collect();
    context.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1 .0.cmp(&b.1 .0)));
    result_ids.extend(context.into_iter().map(|(_, id)| id));

    // Retrieve nodes, apply token budget.
    let mut result = Vec::new();
    let mut tokens_used = 0;
    for id in result_ids {
        if let Some(node) = store.get_node(id)? {
            let cost = crate::knapsack::token_cost(&node);
            if tokens_used + cost > token_budget {
                break;
            }
            tokens_used += cost;
            result.push(node);
        }
    }

    // Ensure source_node and sink_node appear (they may be within budget already).
    if !result.iter().any(|n| n.id == source) {
        result.insert(0, source_node);
    }
    if !result.iter().any(|n| n.id == sink) {
        result.push(sink_node);
    }

    tracing::debug!(
        path_nodes = result.len(),
        optimal_cost = total_cost,
        threshold,
        elapsed_ms = start.elapsed().as_millis(),
        "pcst: path found"
    );

    Ok(result)
}

// ── Local subgraph expansion ─────────────────────────────────────────────────

struct LocalGraph {
    nodes: HashMap<NodeId, Node>,
    /// (src, dst, edge_cost)
    edges: Vec<(NodeId, NodeId, f32)>,
}

fn expand_local_subgraph(
    store: &SqliteStore,
    source: NodeId,
    sink: NodeId,
    filter: &dyn EdgeFilter,
    max_depth: u8,
) -> LocalGraph {
    let mut nodes: HashMap<NodeId, Node> = HashMap::new();
    let mut edges: Vec<(NodeId, NodeId, f32)> = Vec::new();
    // C-1: track edges already added to prevent duplicates from overlapping
    // forward and reverse BFS frontiers. petgraph::DiGraph allows parallel
    // edges; duplicates would inflate Dijkstra cost without affecting
    // correctness but waste allocations and violate the simple-graph model.
    let mut edge_set: HashSet<(NodeId, NodeId)> = HashSet::new();
    let mut fwd_visited: HashSet<NodeId> = HashSet::new();
    let mut rev_visited: HashSet<NodeId> = HashSet::new();
    let mut fwd_queue: VecDeque<(NodeId, u8)> = VecDeque::new();
    let mut rev_queue: VecDeque<(NodeId, u8)> = VecDeque::new();

    fwd_queue.push_back((source, 0));
    fwd_visited.insert(source);
    rev_queue.push_back((sink, 0));
    rev_visited.insert(sink);

    // Seed both endpoints upfront: on high-fanout graphs the forward pass can
    // exhaust the node budget before the reverse pass runs, and the sink must
    // never be starved out of the local graph (it would force a BFS fallback
    // even when a direct edge exists).
    for endpoint in [source, sink] {
        if let Ok(Some(node)) = store.get_node(endpoint) {
            if filter.allow(endpoint, endpoint, Some(node.vname.corpus.as_str())) {
                nodes.entry(endpoint).or_insert(node);
            }
        }
    }

    // The forward pass may consume at most half the node budget so the
    // reverse pass always has room to connect the sink side.
    let fwd_cap = MAX_LOCAL_NODES / 2;

    // Forward BFS from source: follow outgoing edges.
    while let Some((current, depth)) = fwd_queue.pop_front() {
        if nodes.len() >= fwd_cap {
            break;
        }
        if let Ok(Some(node)) = store.get_node(current) {
            // P0-2: Defense-in-depth — re-check filter on node content before
            // including it in the local graph. The edge filter already gated
            // enqueue, but an explicit node-level check mirrors the SEC P0
            // endpoint validation at pcst_path's top and prevents any subtle
            // divergence between edge-filter and node-filter semantics.
            if filter.allow(current, current, Some(node.vname.corpus.as_str())) {
                nodes.entry(current).or_insert(node);
            }
        }

        let outgoing = store.iter_edges_from(current).unwrap_or_default();
        for edge in &outgoing {
            // P-1: Check the in-memory node cache before issuing a SQLite round-trip.
            let dst_corpus = nodes
                .get(&edge.dst)
                .map(|n| n.vname.corpus.clone())
                .or_else(|| {
                    store
                        .get_node(edge.dst)
                        .ok()
                        .flatten()
                        .map(|n| n.vname.corpus.clone())
                });
            if !filter.allow(current, edge.dst, dst_corpus.as_deref()) {
                continue;
            }
            // C-1: Only add edge if not already present (dedup forward+reverse overlap).
            if edge_set.insert((current, edge.dst)) {
                let cost = 1.0 - edge.kind.ppr_weight();
                edges.push((current, edge.dst, cost));
            }
            if depth < max_depth && fwd_visited.insert(edge.dst) {
                fwd_queue.push_back((edge.dst, depth + 1));
            }
        }
    }

    // Reverse BFS from sink: follow INCOMING edges to find predecessors.
    // This ensures bidirectional coverage: long chains where source and sink
    // are separated by more than max_depth are still fully connected.
    while let Some((current, depth)) = rev_queue.pop_front() {
        if nodes.len() >= MAX_LOCAL_NODES {
            break;
        }
        if let Ok(Some(node)) = store.get_node(current) {
            // P0-2: same defense-in-depth node filter as forward pass.
            if filter.allow(current, current, Some(node.vname.corpus.as_str())) {
                nodes.entry(current).or_insert(node);
            }
        }

        let incoming = store.iter_edges_to(current).unwrap_or_default();
        for edge in &incoming {
            // For reverse BFS, src is the predecessor of current.
            let src_corpus = nodes
                .get(&edge.src)
                .map(|n| n.vname.corpus.clone())
                .or_else(|| {
                    store
                        .get_node(edge.src)
                        .ok()
                        .flatten()
                        .map(|n| n.vname.corpus.clone())
                });
            if !filter.allow(edge.src, current, src_corpus.as_deref()) {
                continue;
            }
            // Add the forward edge (src→current) to the local graph.
            // C-1: deduplicate against edges already added by the forward pass.
            if edge_set.insert((edge.src, current)) {
                let cost = 1.0 - edge.kind.ppr_weight();
                edges.push((edge.src, current, cost));
            }
            if depth < max_depth && rev_visited.insert(edge.src) {
                rev_queue.push_back((edge.src, depth + 1));
            }
        }
    }

    LocalGraph { nodes, edges }
}

// ── petgraph construction ────────────────────────────────────────────────────

fn build_graph(
    nodes: &HashMap<NodeId, Node>,
    edges: &[(NodeId, NodeId, f32)],
) -> (
    DiGraph<NodeId, f32>,
    HashMap<NodeId, NodeIndex>,
    HashMap<NodeIndex, NodeId>,
) {
    let mut graph = DiGraph::new();
    let mut node_to_idx: HashMap<NodeId, NodeIndex> = HashMap::new();
    let mut idx_to_node: HashMap<NodeIndex, NodeId> = HashMap::new();

    // Determinism: insert nodes in sorted NodeId order. HashMap key order
    // varies run-to-run, and NodeIndex assignment order is astar's implicit
    // tie-breaker among equal-cost routes — unsorted insertion makes the
    // chosen route nondeterministic on tied-cost topologies.
    let mut ids: Vec<NodeId> = nodes.keys().copied().collect();
    ids.sort_by_key(|id| id.0);
    for id in ids {
        let idx = graph.add_node(id);
        node_to_idx.insert(id, idx);
        idx_to_node.insert(idx, id);
    }

    for &(src, dst, cost) in edges {
        if let (Some(&si), Some(&di)) = (node_to_idx.get(&src), node_to_idx.get(&dst)) {
            graph.add_edge(si, di, cost);
        }
    }

    (graph, node_to_idx, idx_to_node)
}

// ── BFS fallback ─────────────────────────────────────────────────────────────

fn bfs_fallback(
    store: &SqliteStore,
    seed: NodeId,
    filter: &dyn EdgeFilter,
    token_budget: usize,
) -> Result<Vec<Node>, travsr_error::TravsrError> {
    // P-3: HashSet and VecDeque are already imported at the top of the module.
    let mut visited: HashSet<NodeId> = HashSet::new();
    let mut queue: VecDeque<(NodeId, u8)> = VecDeque::new();
    let mut result: Vec<Node> = Vec::new();
    let mut tokens_used: usize = 0;

    queue.push_back((seed, 0));
    visited.insert(seed);

    while let Some((current_id, depth)) = queue.pop_front() {
        if depth > 3 {
            break;
        }

        if let Some(node) = store.get_node(current_id)? {
            // P0-2: Defense-in-depth — re-check filter on node content before
            // adding it to the result. Edge enqueue is filtered below, but the
            // seed node and each dequeued neighbor must also pass a node-level
            // check to prevent corpus leakage via timing or content divergence.
            if !filter.allow(current_id, current_id, Some(node.vname.corpus.as_str())) {
                continue;
            }
            let cost = crate::knapsack::token_cost(&node);
            if tokens_used + cost > token_budget {
                break;
            }
            tokens_used += cost;
            result.push(node);
        }

        if depth < 3 {
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

    Ok(result)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rbac::OpenFilter;
    use travsr_core::{Edge, EdgeKind, VName};
    use travsr_store::Store;

    fn node(path: &str, sig: &str) -> Node {
        Node::new(VName::new("test-corpus", "", path, "rust", sig), "function")
    }

    fn store_with(
        nodes: &[Node],
        edges: &[(NodeId, NodeId, EdgeKind)],
    ) -> travsr_store::SqliteStore {
        let mut s = travsr_store::SqliteStore::open_in_memory().unwrap();
        for n in nodes {
            s.put_node(n).unwrap();
        }
        for &(src, dst, kind) in edges {
            s.put_edge(&Edge::new(src, dst, kind)).unwrap();
        }
        s
    }

    #[test]
    fn pcst_finds_direct_path() {
        let a = node("a.rs", "fn:a");
        let b = node("b.rs", "fn:b");
        let store = store_with(&[a.clone(), b.clone()], &[(a.id, b.id, EdgeKind::RefCall)]);

        let result = pcst_path(&store, a.id, b.id, &OpenFilter, 4096).unwrap();
        let ids: Vec<_> = result.iter().map(|n| n.id).collect();
        assert!(ids.contains(&a.id), "source must be in path");
        assert!(ids.contains(&b.id), "sink must be in path");
    }

    #[test]
    fn pcst_returns_empty_for_missing_source() {
        let store = travsr_store::SqliteStore::open_in_memory().unwrap();
        let result = pcst_path(&store, NodeId(999), NodeId(888), &OpenFilter, 4096).unwrap();
        assert!(result.is_empty(), "missing source must return empty");
    }

    #[test]
    fn pcst_same_node_returns_single() {
        let a = node("a.rs", "fn:a");
        let store = store_with(std::slice::from_ref(&a), &[]);
        let result = pcst_path(&store, a.id, a.id, &OpenFilter, 4096).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, a.id);
    }

    #[test]
    fn pcst_falls_back_to_bfs_when_no_path() {
        // Two disconnected nodes — no path, fallback BFS returns seed neighborhood.
        let a = node("a.rs", "fn:a");
        let b = node("b.rs", "fn:b");
        let store = store_with(&[a.clone(), b.clone()], &[]);
        // Should not panic; returns BFS from source.
        let result = pcst_path(&store, a.id, b.id, &OpenFilter, 4096).unwrap();
        // BFS from a will find only a (no outgoing edges).
        assert!(result.iter().any(|n| n.id == a.id));
    }

    #[test]
    fn pcst_high_fanout_source_does_not_starve_sink() {
        // Regression (#317): source with fanout larger than the local-node
        // budget plus a direct edge to the sink. The forward pass used to
        // consume the whole budget, the sink never entered the local graph,
        // and PCST silently fell back to BFS instead of returning the
        // one-hop path.
        let src = node("src.rs", "fn:src");
        let sink = node("sink.rs", "fn:sink");
        let mut store = travsr_store::SqliteStore::open_in_memory().unwrap();
        store.put_node(&src).unwrap();
        store.put_node(&sink).unwrap();
        for i in 0..(MAX_LOCAL_NODES + 10) {
            let filler = node(&format!("filler{i}.rs"), &format!("fn:filler{i}"));
            store.put_node(&filler).unwrap();
            store
                .put_edge(&Edge::new(src.id, filler.id, EdgeKind::RefCall))
                .unwrap();
        }
        store
            .put_edge(&Edge::new(src.id, sink.id, EdgeKind::RefCall))
            .unwrap();

        let result = pcst_path(&store, src.id, sink.id, &OpenFilter, 1 << 20).unwrap();
        let ids: Vec<_> = result.iter().map(|n| n.id).collect();
        assert!(ids.contains(&src.id), "source must be in path");
        assert!(ids.contains(&sink.id), "sink must be in path");
        // Regression (#317): ref/call edges cost 0, so the λ threshold admits
        // the entire zero-cost component. The route must lead the result —
        // consumers truncate output, and a sink buried after thousands of
        // context nodes is as bad as no sink at all.
        assert_eq!(ids[0], src.id, "route must start at source");
        assert_eq!(ids[1], sink.id, "direct route must place sink second");
    }

    #[test]
    fn pcst_respects_rbac_filter() {
        use crate::rbac::RbacFilter;

        let a = node("a.rs", "fn:a"); // corpus = "test-corpus"
        let b = Node::new(
            VName::new("evil-corpus", "", "b.rs", "rust", "fn:b"),
            "function",
        );
        let mut store = travsr_store::SqliteStore::open_in_memory().unwrap();
        store.put_node(&a).unwrap();
        store.put_node(&b).unwrap();
        store
            .put_edge(&Edge::new(a.id, b.id, EdgeKind::RefCall))
            .unwrap();

        // Only allow test-corpus; b is in evil-corpus.
        let filter = RbacFilter::new(["test-corpus"]);
        // b is in evil-corpus — SEC P0: returns empty (sink inaccessible).
        let result = pcst_path(&store, a.id, b.id, &filter, 4096).unwrap();
        assert!(result.is_empty(), "sink in denied corpus must return empty");
    }

    #[test]
    fn pcst_diamond_topology_no_duplicate_edges() {
        // Diamond: A→B, A→C, B→D, C→D, source=A, sink=D.
        // Forward BFS from A reaches B, C, D.
        // Reverse BFS from D reaches B, C, A.
        // Both passes cover B→D and C→D — without deduplication these edges
        // would appear twice in the local graph (C-1 regression test).
        let a = node("a.rs", "fn:a");
        let b = node("b.rs", "fn:b");
        let c = node("c.rs", "fn:c");
        let d = node("d.rs", "fn:d");
        let store = store_with(
            &[a.clone(), b.clone(), c.clone(), d.clone()],
            &[
                (a.id, b.id, EdgeKind::RefCall),
                (a.id, c.id, EdgeKind::RefCall),
                (b.id, d.id, EdgeKind::RefCall),
                (c.id, d.id, EdgeKind::RefCall),
            ],
        );

        let result = pcst_path(&store, a.id, d.id, &OpenFilter, 4096).unwrap();
        let ids: Vec<_> = result.iter().map(|n| n.id).collect();
        assert!(ids.contains(&a.id), "source must be in result");
        assert!(ids.contains(&d.id), "sink must be in result");
        // No node should appear twice (duplicate edges would not cause this,
        // but they would produce non-simple graphs that violate the PCST model).
        let unique_ids: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(ids.len(), unique_ids.len(), "no duplicate nodes in result");
    }

    #[test]
    fn pcst_is_deterministic_on_tied_cost_topology() {
        // Diamond with zero-cost ref/call edges: A→B→D and A→C→D both cost 0,
        // so route selection ties and the λ threshold admits the whole
        // component. Before sorting node insertion in build_graph and settling
        // the full component in Dijkstra (no early-exit goal), the output
        // order varied run-to-run with HashMap iteration order.
        let a = node("a.rs", "fn:a");
        let b = node("b.rs", "fn:b");
        let c = node("c.rs", "fn:c");
        let d = node("d.rs", "fn:d");
        let nodes = [a.clone(), b.clone(), c.clone(), d.clone()];
        let edges = [
            (a.id, b.id, EdgeKind::RefCall),
            (a.id, c.id, EdgeKind::RefCall),
            (b.id, d.id, EdgeKind::RefCall),
            (c.id, d.id, EdgeKind::RefCall),
        ];

        // Fresh store per run: rebuilds all in-memory state so any
        // HashMap-iteration-order nondeterminism gets a chance to surface.
        let run = || {
            let store = store_with(&nodes, &edges);
            pcst_path(&store, a.id, d.id, &OpenFilter, 4096)
                .unwrap()
                .iter()
                .map(|n| n.id)
                .collect::<Vec<_>>()
        };

        let first = run();
        let second = run();
        assert_eq!(
            first, second,
            "pcst_path must return identical node order across runs"
        );
        assert_eq!(first[0], a.id, "route must start at source");
        assert!(first.contains(&d.id), "sink must be in result");
    }
}
