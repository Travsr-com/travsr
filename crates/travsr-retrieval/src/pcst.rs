//! Execution-path retrieval — shortest path plus a λ-corridor of context.
//!
//! Finds a low-cost subgraph connecting `source` to `sink` through the indexed
//! graph, applying the `EdgeFilter` at every traversal step (RBAC enforcement).
//!
//! **This is not Prize-Collecting Steiner Tree.** Despite the module path and
//! ADR-007, no Goemans-Williamson primal-dual approximation is implemented
//! here — the algorithm below is Dijkstra with a threshold-based padding step.
//! Real PCST is scoped as S16 and gated on a benchmark showing it beats this
//! heuristic; see issue #527.
//!
//! # Algorithm (ADR-007 for λ only)
//! 1. Build a bounded local subgraph via bidirectional BFS from source + sink (depth 5).
//! 2. Map to a petgraph `DiGraph` with edge costs = `1.0 / ppr_weight`
//!    (#527: was `1.0 - ppr_weight`, which made `ref/call` free and the λ
//!    corridor unbounded — see `edge_cost`).
//! 3. Run Dijkstra from source; extract the minimum-cost path to sink.
//! 4. Include nodes within λ of the optimal path cost (λ = 0.5, ADR-007).
//! 5. On missing source/sink, or when no path exists: fall back to BFS depth-3.
//!    There is no wall-clock timeout — see the rationale in `pcst_path`.
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

/// λ, overridable via `TRAVSR_PCST_LAMBDA` for experimentation.
///
/// Mirrors the `TRAVSR_PPR_*` knobs in `ppr.rs` (ADR-003 §Overrides): a way to
/// sweep the constant against a benchmark, **not** a production configuration
/// surface. Added for #527, where λ could not previously be evaluated at all —
/// `ref/call` cost 0 made the corridor threshold 0 for every λ, so no sweep
/// could have distinguished one value from another.
///
/// Rejects non-finite and negative values; 0.0 is allowed and means "route
/// only, no corridor".
fn pcst_lambda() -> f32 {
    parse_lambda(std::env::var("TRAVSR_PCST_LAMBDA").ok())
}

/// The validation half of [`pcst_lambda`], split out so it can be tested.
///
/// Reading the variable inside the test would race: `pcst_path` calls
/// `pcst_lambda`, so a test that sets `TRAVSR_PCST_LAMBDA` changes the corridor
/// under every other pcst test running concurrently in the same binary. Taking
/// the raw value as an argument tests the shipped predicate without touching
/// the process environment.
fn parse_lambda(raw: Option<String>) -> f32 {
    raw.and_then(|v| v.parse().ok())
        .filter(|x: &f32| x.is_finite() && *x >= 0.0)
        .unwrap_or(PCST_LAMBDA)
}

/// Traversal cost of an edge: the reciprocal of its PPR weight.
///
/// #527: this was `1.0 - ppr_weight`, which gives `ref/call` — the strongest
/// edge kind, weight 1.00 — a cost of **exactly zero**. A route made of
/// `ref/call` edges then has `total_cost = 0`, and the λ corridor threshold is
///
/// ```text
/// total_cost * (1.0 + PCST_LAMBDA)  ==  0 * anything  ==  0
/// ```
///
/// so `c <= threshold` admits every node sitting at cost 0 — the source's
/// entire `ref/call`-reachable component. That is why ~78% of a
/// `get_execution_path` response was corridor padding, and why λ had no
/// observable effect at any value: the corridor was never actually bounded by
/// it. Tuning λ first would have been measuring a knob that was disconnected.
///
/// The reciprocal keeps the ordering the weights encode (a stronger edge is
/// still cheaper to traverse) while making nothing free, so λ becomes a real
/// bound. Weights are a closed set in `EdgeKind::ppr_weight` with no zero and
/// no catch-all arm, but a non-positive weight would produce an infinite or
/// negative cost and quietly corrupt Dijkstra, so it is clamped rather than
/// trusted.
fn edge_cost(kind: &travsr_core::EdgeKind) -> f32 {
    cost_from_weight(kind.ppr_weight())
}

/// The arithmetic half of [`edge_cost`], split out so the clamp can be tested.
///
/// `EdgeKind::ppr_weight` is a closed match with no zero and no catch-all, so
/// the clamp branch is unreachable through `edge_cost` and there is no way to
/// exercise it from the enum. Taking the weight directly is the only way to
/// prove the guard does what its comment claims.
fn cost_from_weight(w: f32) -> f32 {
    if w > 0.0 {
        1.0 / w
    } else {
        // Unreachable with today's table; a new zero-weight kind should be
        // maximally expensive, not free or infinite.
        MAX_EDGE_COST
    }
}

/// Cost assigned to an edge whose PPR weight is non-positive. Above every real
/// cost in the table (`overrides` is the most expensive at 1/0.30 = 3.33).
const MAX_EDGE_COST: f32 = 10.0;

/// Maximum nodes in the local subgraph built by the bidirectional BFS expansion.
const MAX_LOCAL_NODES: usize = 2_000;

/// BFS expansion depth for the local subgraph.
const EXPAND_DEPTH: u8 = 5;

/// Find a path from `source` to `sink`, respecting `filter`, within `token_budget`.
///
/// Returns nodes on (or near) the optimal PCST path, in approximate traversal
/// order. Falls back to BFS depth-3 when either endpoint is missing from the
/// local subgraph, or when no path exists. There is no wall-clock timeout.
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
    // Observability only — this is NOT a budget. Nothing below reads `start`
    // except the `elapsed_ms` field on the closing debug! span. There is no
    // wall-clock gate in this function; see the rationale above
    // `expand_local_subgraph` for why one was deliberately not added.
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

    // Build local subgraph via bidirectional BFS expansion. Expansion is
    // self-bounded: `expand_local_subgraph` stops at `MAX_LOCAL_NODES` node
    // pops, so the cost of everything below is bounded regardless of fan-out.
    // We deliberately do NOT gate the rest of the function on a wall-clock
    // budget here: such a check can only fire *after* expansion has already
    // completed (it cannot interrupt the work it nominally guards), so its sole
    // effect would be to discard a complete, bounded subgraph and fall back to
    // a strictly-less-informative BFS whenever the machine is briefly loaded —
    // making the result nondeterministic. dijkstra/astar over a ≤2000-node
    // subgraph is microsecond-scale and needs no separate time budget.
    let local = expand_local_subgraph(store, source, sink, filter, EXPAND_DEPTH);

    // Build petgraph from local subgraph.
    let (graph, node_to_idx, idx_to_node) = build_graph(&local.nodes, &local.edges);

    let Some(&src_idx) = node_to_idx.get(&source) else {
        return bfs_fallback(store, source, filter, token_budget);
    };
    let Some(&dst_idx) = node_to_idx.get(&sink) else {
        return bfs_fallback(store, source, filter, token_budget);
    };

    // Dijkstra from source: cost = 1.0 / ppr_weight (lower = more traversable).
    // Determinism: pass `None` as the goal so the FULL local component settles.
    // Cost depends only on edge kind, so every route of the same shape scores
    // the same and ties are common (every `ref/call` hop costs exactly 1.0).
    // With an early-exit goal, WHICH of the tied nodes settle before the sink
    // varies with HashMap iteration order run-to-run. The component is capped
    // at MAX_LOCAL_NODES (2000), so settling the full component is cheap.
    let costs = dijkstra(&graph, src_idx, None, |e| *e.weight());

    // Reconstruct the actual source→sink route. The route must LEAD the result:
    // downstream consumers truncate (token budget here, 4 KiB sanitizer at the
    // MCP boundary), and the λ corridor still admits far more than the route —
    // measured at ~37-43 nodes for a 3-to-5 node route on this repo's own graph
    // (#527). A sink appended after that blob would be cut off every time.
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
            "pcst: no path found, falling back to BFS"
        );
        return bfs_fallback(store, source, filter, token_budget);
    };

    // Route nodes first, in traversal order (source → … → sink).
    let mut result_ids: Vec<NodeId> = route
        .iter()
        .filter_map(|idx| idx_to_node.get(idx).copied())
        .collect();
    let on_route: HashSet<NodeId> = result_ids.iter().copied().collect();

    // Then context: nodes whose cheapest-path cost is within λ × total_cost of
    // optimal, cheapest first (deterministic — `costs` is a HashMap).
    let threshold = total_cost * (1.0 + pcst_lambda());
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
                let cost = edge_cost(&edge.kind);
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
                let cost = edge_cost(&edge.kind);
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
mod edge_cost_tests {
    use super::{
        cost_from_weight, edge_cost, parse_lambda, pcst_lambda, MAX_EDGE_COST, PCST_LAMBDA,
    };
    use travsr_core::EdgeKind;

    /// #527: the defect. `1.0 - ppr_weight` gave `ref/call` (weight 1.00) a
    /// cost of exactly zero, so a route of `ref/call` edges had `total_cost =
    /// 0` and the corridor threshold `total_cost * (1.0 + λ)` was zero for
    /// **every** λ. The filter `c <= 0` then admitted the source's whole
    /// zero-cost component, and no λ value could have changed that.
    #[test]
    fn no_edge_kind_is_free_to_traverse() {
        for kind in [
            EdgeKind::RefCall,
            EdgeKind::FFICall,
            EdgeKind::DefinesBinding,
            EdgeKind::Exports,
            EdgeKind::Depends,
            EdgeKind::ResolvesTo,
            EdgeKind::RefImports,
            EdgeKind::IsImplementation,
            EdgeKind::Configures,
            EdgeKind::Overrides,
            EdgeKind::ExternalDependency,
        ] {
            let c = edge_cost(&kind);
            assert!(
                c > 0.0 && c.is_finite(),
                "{kind:?} costs {c}, which makes the λ corridor unbounded"
            );
        }
    }

    /// The weights encode a preference order; the cost function must not
    /// invert it. A stronger edge stays cheaper to traverse.
    #[test]
    fn a_stronger_edge_stays_cheaper() {
        assert!(edge_cost(&EdgeKind::RefCall) < edge_cost(&EdgeKind::Depends));
        assert!(edge_cost(&EdgeKind::Depends) < edge_cost(&EdgeKind::Overrides));
        assert!(edge_cost(&EdgeKind::FFICall) < edge_cost(&EdgeKind::DefinesBinding));
    }

    /// `ppr_weight` is a closed match with no zero and no catch-all today, so
    /// this is unreachable — which is the point. A future zero-weight kind
    /// would divide to infinity and corrupt Dijkstra silently; it is clamped
    /// to the most expensive cost instead, never to free.
    #[test]
    fn a_hypothetical_zero_weight_is_expensive_not_infinite() {
        assert_eq!(
            cost_from_weight(0.0),
            MAX_EDGE_COST,
            "a zero weight must clamp, not divide to infinity"
        );
        assert_eq!(
            cost_from_weight(-1.0),
            MAX_EDGE_COST,
            "a negative weight must clamp, not produce a negative cost"
        );
        assert!(
            cost_from_weight(0.0).is_finite(),
            "an infinite cost would corrupt Dijkstra silently"
        );
        assert!(
            MAX_EDGE_COST > edge_cost(&EdgeKind::Overrides),
            "the fallback must be costlier than every real edge"
        );
    }

    /// The λ override exists to sweep the constant against the #533 harness,
    /// not as a configuration surface. It must refuse values that would make
    /// the threshold meaningless.
    #[test]
    fn the_lambda_override_rejects_nonsense_and_defaults_otherwise() {
        // No env var set in this process: the ADR-007 constant stands.
        assert_eq!(pcst_lambda(), PCST_LAMBDA);

        // Accepted: values the sweep would legitimately pass.
        let lam = |v: &str| parse_lambda(Some(v.to_string()));
        assert_eq!(lam("0.0"), 0.0, "0 means route-distance only");
        assert_eq!(lam("0.25"), 0.25);
        assert_eq!(lam("1.0"), 1.0);

        // Rejected: each of these falls back to the constant rather than
        // reaching the threshold. A negative λ shrinks the corridor below the
        // route itself; NaN makes every `c <= threshold` comparison false and
        // empties the corridor; inf admits the whole local subgraph.
        for bad in ["-0.1", "NaN", "inf", "-inf", "abc", ""] {
            assert_eq!(
                lam(bad),
                PCST_LAMBDA,
                "{bad:?} must fall back to the ADR-007 default"
            );
        }
        // Unset behaves the same as unparseable.
        assert_eq!(parse_lambda(None), PCST_LAMBDA);
    }
}

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
        // Regression (#317): the λ threshold admits far more than the route —
        // here the source's whole fanout sits one hop away, inside the
        // corridor. The route must lead the result: consumers truncate output,
        // and a sink buried after thousands of context nodes is as bad as no
        // sink at all.
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
