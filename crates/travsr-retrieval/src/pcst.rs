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

use petgraph::algo::dijkstra;
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
    let costs = dijkstra(&graph, src_idx, Some(dst_idx), |e| *e.weight());

    let Some(&total_cost) = costs.get(&dst_idx) else {
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

    // Include nodes whose cheapest-path cost is within λ × total_cost of optimal.
    let threshold = total_cost * (1.0 + PCST_LAMBDA);
    let mut result_ids: Vec<NodeId> = costs
        .iter()
        .filter(|(_, &c)| c <= threshold)
        .filter_map(|(idx, _)| idx_to_node.get(idx).copied())
        .collect();

    // Always include sink if reachable.
    if !result_ids.contains(&sink) {
        result_ids.push(sink);
    }

    // Retrieve nodes, apply token budget.
    let mut result = Vec::new();
    let mut tokens_used = 0;
    for id in result_ids {
        if let Some(node) = store.get_node(id)? {
            let cost = node.vname.signature.len() + node.kind.len();
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
    let mut fwd_visited: HashSet<NodeId> = HashSet::new();
    let mut rev_visited: HashSet<NodeId> = HashSet::new();
    let mut fwd_queue: VecDeque<(NodeId, u8)> = VecDeque::new();
    let mut rev_queue: VecDeque<(NodeId, u8)> = VecDeque::new();

    fwd_queue.push_back((source, 0));
    fwd_visited.insert(source);
    rev_queue.push_back((sink, 0));
    rev_visited.insert(sink);

    // Forward BFS from source: follow outgoing edges.
    while let Some((current, depth)) = fwd_queue.pop_front() {
        if nodes.len() >= MAX_LOCAL_NODES {
            break;
        }
        if let Ok(Some(node)) = store.get_node(current) {
            nodes.entry(current).or_insert(node);
        }

        let outgoing = store.iter_edges_from(current).unwrap_or_default();
        for edge in &outgoing {
            let dst_corpus = store
                .get_node(edge.dst)
                .ok()
                .flatten()
                .map(|n| n.vname.corpus.clone());
            if !filter.allow(current, edge.dst, dst_corpus.as_deref()) {
                continue;
            }
            let cost = 1.0 - edge.kind.ppr_weight();
            edges.push((current, edge.dst, cost));
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
            nodes.entry(current).or_insert(node);
        }

        let incoming = store.iter_edges_to(current).unwrap_or_default();
        for edge in &incoming {
            // For reverse BFS, src is the predecessor of current.
            let src_corpus = store
                .get_node(edge.src)
                .ok()
                .flatten()
                .map(|n| n.vname.corpus.clone());
            if !filter.allow(edge.src, current, src_corpus.as_deref()) {
                continue;
            }
            // Add the forward edge (src→current) to the local graph.
            let cost = 1.0 - edge.kind.ppr_weight();
            edges.push((edge.src, current, cost));
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

    for &id in nodes.keys() {
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
    use std::collections::{HashSet, VecDeque};

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
            let cost = node.vname.signature.len() + node.kind.len();
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
}
