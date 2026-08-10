//! Personalized PageRank hyperparameters (ADR-003) and iterative algorithm.
//!
//! Defaults are the Accepted values from ADR-003. Do NOT change them without
//! updating the ADR. Overridable at runtime via env vars for experimentation
//! (see ADR-003 §Overrides); these are NOT a production configuration surface.

/// Probability of following an outgoing edge at each PPR step.
///
/// The complementary probability (1 − ALPHA) is the teleportation weight
/// back to the seed nodes. Standard PageRank default; see ADR-003 §Decision.
pub const ALPHA: f32 = 0.85;

/// L₁-norm convergence threshold.
///
/// Iteration stops when `‖r_{t+1} − r_t‖₁ < EPSILON`. 1e-6 converges in
/// 15–30 iterations on code graphs ≤ 10M nodes at ALPHA = 0.85 (ADR-003).
pub const EPSILON: f32 = 1e-6;

/// Hard iteration cap — prevents runaway on degenerate graph topologies.
///
/// At ALPHA = 0.85, genuine convergence always occurs before this limit for
/// any connected component within the MVP node ceiling (ADR-003).
pub const MAX_ITERATIONS: u32 = 50;

// Compile-time guard: fires on every build (not just tests) so an accidental
// constant change is caught before release. Update the ADR before changing.
const _: () = assert!(
    MAX_ITERATIONS == 50,
    "MAX_ITERATIONS changed from ADR-003 value — update ADR-003 first"
);

/// Return ALPHA, overridden by `TRAVSR_PPR_ALPHA` env var if set and parseable.
///
/// Accepts only values in (0.0, 1.0) exclusive — values outside this range
/// are mathematically invalid for a damping factor and fall back to the default.
pub fn alpha() -> f32 {
    parse_env_f32_bounded("TRAVSR_PPR_ALPHA", ALPHA, 0.0, 1.0)
}

/// Return EPSILON, overridden by `TRAVSR_PPR_EPSILON` env var if set and parseable.
pub fn epsilon() -> f32 {
    parse_env_f32("TRAVSR_PPR_EPSILON", EPSILON)
}

/// Return MAX_ITERATIONS, overridden by `TRAVSR_PPR_MAX_ITER` env var if set and parseable.
///
/// Rejects `0` — a zero iteration cap produces an uninitialized score vector.
pub fn max_iterations() -> u32 {
    std::env::var("TRAVSR_PPR_MAX_ITER")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(MAX_ITERATIONS)
}

fn parse_env_f32(var: &str, default: f32) -> f32 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&x: &f32| x.is_finite() && x > 0.0)
        .unwrap_or(default)
}

/// Like `parse_env_f32` but also enforces an exclusive upper bound.
fn parse_env_f32_bounded(var: &str, default: f32, lo: f32, hi: f32) -> f32 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&x: &f32| x.is_finite() && x > lo && x < hi)
        .unwrap_or(default)
}

// ── PPR algorithm ─────────────────────────────────────────────────────────────

use std::collections::HashMap;

use anyhow::Result as AnyResult;
use travsr_core::NodeId;
use travsr_error::TravsrError;
use travsr_store::Store;

use crate::rbac::EdgeFilter;

/// Personalized PageRank with uniform seed weights.
///
/// Returns the top-`k` nodes by PPR score, sorted descending.
///
/// # Algorithm
///
/// Standard power-iteration PPR with edge-kind weights (DEBT-016):
///
/// ```text
/// r₀[v] = 1/|seeds| if v ∈ seeds, else 0
/// r_{t+1}[v] = (1 − α) · r₀[v]
///            + α · Σ_{u: (u,v) ∈ E}  w(u→v) / W(u)  ·  r_t[u]
/// ```
///
/// where `W(u) = Σ_{e ∈ out(u)} w(e)` is the total outgoing weight of `u`
/// (normalisation so mass is conserved), and `w(e) = EdgeKind::ppr_weight`.
///
/// Iteration stops when `‖r_{t+1} − r_t‖₁ < ε` or `MAX_ITERATIONS` is reached.
///
/// # Parameters
///
/// - `store` — any `Store` implementor (e.g. SQLite)
/// - `seeds` — query nodes to personalise toward; must be non-empty
/// - `k` — number of top nodes to return (0 = return all scored nodes)
///
/// # Complexity
///
/// O(iterations × |E_reachable|) time.  Space O(|V_reachable|).
pub fn ppr<S: Store>(
    store: &S,
    seeds: &[NodeId],
    k: usize,
    filter: &dyn EdgeFilter,
) -> Result<Vec<(NodeId, f32)>, TravsrError> {
    if seeds.is_empty() {
        return Ok(Vec::new());
    }
    let seed_score = 1.0 / seeds.len() as f32;
    let personalization: HashMap<NodeId, f32> = seeds.iter().fold(HashMap::new(), |mut m, &id| {
        *m.entry(id).or_insert(0.0) += seed_score;
        m
    });
    ppr_inner(store, &personalization, k, filter).map_err(|e| TravsrError::Internal(e.to_string()))
}

/// Personalized PageRank with KNN-derived cosine-similarity weights on seeds.
///
/// Same algorithm as [`ppr`] but the personalisation vector is proportional to
/// the provided weights rather than uniform.  Seeds with higher cosine similarity
/// to the query receive more teleportation mass, steering the walk toward their
/// neighbourhoods.
///
/// `seeds` is a slice of `(NodeId, weight)` pairs.  Weights must be positive
/// and finite; if their sum is ≤ 0 or non-finite the function falls back to
/// uniform weights (identical to calling [`ppr`]).
pub fn ppr_weighted<S: Store>(
    store: &S,
    seeds: &[(NodeId, f32)],
    k: usize,
    filter: &dyn EdgeFilter,
) -> Result<Vec<(NodeId, f32)>, TravsrError> {
    if seeds.is_empty() {
        return Ok(Vec::new());
    }
    let total: f32 = seeds.iter().map(|(_, w)| w).sum();
    if !total.is_finite() || total <= 0.0 {
        // Degenerate weights — fall back to uniform rather than producing NaN scores.
        let ids: Vec<NodeId> = seeds.iter().map(|(id, _)| *id).collect();
        return ppr(store, &ids, k, filter);
    }
    let mut personalization: HashMap<NodeId, f32> = HashMap::with_capacity(seeds.len());
    for &(id, w) in seeds {
        *personalization.entry(id).or_insert(0.0) += w / total;
    }
    ppr_inner(store, &personalization, k, filter).map_err(|e| TravsrError::Internal(e.to_string()))
}

/// Core PPR power-iteration over a pre-normalised personalisation vector.
///
/// `personalization` maps seed node IDs to their teleportation probabilities;
/// values must sum to ~1.0 (callers are responsible for normalisation).
fn ppr_inner<S: Store>(
    store: &S,
    personalization: &HashMap<NodeId, f32>,
    k: usize,
    filter: &dyn EdgeFilter,
) -> AnyResult<Vec<(NodeId, f32)>> {
    let _span = tracing::debug_span!("ppr", seeds = personalization.len(), k = k).entered();

    if personalization.is_empty() {
        return Ok(Vec::new());
    }

    let a = alpha();
    let eps = epsilon();
    let max_iter = max_iterations();

    // ── BFS to collect all reachable nodes + their outgoing edges ─────────────
    // We materialise the subgraph once so the inner iteration loop never hits
    // the store — critical for meeting the p95 < 50ms budget.
    let seed_ids: Vec<NodeId> = personalization.keys().copied().collect();
    let (adj, reverse_adj) = build_weighted_subgraph(store, &seed_ids, filter)?;

    if adj.is_empty() {
        // Seeds not in the graph — return seeds scored by their personalisation weight.
        let mut out: Vec<(NodeId, f32)> = personalization.iter().map(|(&id, &s)| (id, s)).collect();
        out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        if k > 0 {
            out.truncate(k);
        }
        return Ok(out);
    }

    // ── Personalisation vector r₀ ─────────────────────────────────────────────
    let mut r: HashMap<NodeId, f32> = HashMap::with_capacity(adj.len());
    for (&s, &p) in personalization {
        *r.entry(s).or_insert(0.0) += p;
    }

    // ── Power iteration ───────────────────────────────────────────────────────
    let mut iterations_run: u32 = 0;
    for iter in 0..max_iter {
        let mut r_next: HashMap<NodeId, f32> = HashMap::with_capacity(r.len());

        // Teleportation: (1 − α) · p(v)  where p(v) is the personalisation weight.
        for (&s, &p) in personalization {
            *r_next.entry(s).or_insert(0.0) += (1.0 - a) * p;
        }

        // Propagation: α · Σ w(u→v)/W(u) · r[u]
        for (&src, outgoing) in &adj {
            let src_score = r.get(&src).copied().unwrap_or(0.0);
            if src_score == 0.0 {
                continue;
            }
            let total_weight: f32 = outgoing.iter().map(|(_, w)| w).sum();
            if total_weight == 0.0 {
                continue;
            }
            for &(dst, w) in outgoing {
                *r_next.entry(dst).or_insert(0.0) += a * (w / total_weight) * src_score;
            }
        }

        // Include dangling nodes that have no outgoing edges in reverse_adj —
        // they never receive propagated mass but may still have r[v] > 0 from
        // teleportation. Ensure they appear in r_next so convergence is checked.
        for &node in reverse_adj.keys() {
            r_next.entry(node).or_insert(0.0);
        }

        // ── Convergence check: L₁ norm ────────────────────────────────────────
        let delta: f32 = r_next
            .iter()
            .map(|(id, &v)| (v - r.get(id).copied().unwrap_or(0.0)).abs())
            .sum::<f32>()
            + r.iter()
                .filter(|(id, _)| !r_next.contains_key(id))
                .map(|(_, &v)| v)
                .sum::<f32>();

        tracing::trace!(ppr.iteration = iter, ppr.delta = delta, "ppr.iterate");
        iterations_run = iter + 1;
        r = r_next;

        if delta < eps {
            break;
        }
    }
    tracing::debug!(
        ppr.nodes_scored = r.len(),
        ppr.iterations = iterations_run,
        "ppr complete"
    );

    // ── Top-k extraction ──────────────────────────────────────────────────────
    let mut scores: Vec<(NodeId, f32)> = r.into_iter().collect();
    // RET-H1: secondary sort by NodeId ensures identical queries always return
    // nodes in the same order even when multiple nodes share the same PPR score.
    // Without this tie-break, HashMap iteration order makes results non-deterministic.
    scores.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    if k > 0 {
        scores.truncate(k);
    }
    Ok(scores)
}

/// Outgoing weighted adjacency list: `node → [(dst, weight), …]`
type WeightedAdj = HashMap<NodeId, Vec<(NodeId, f32)>>;

/// Build a weighted adjacency list for all nodes reachable from `seeds` via
/// BFS up to the MVP depth ceiling (depth 6 — deeper than BFS's default 3 to
/// allow PPR to discover more of the semantic neighbourhood).
///
/// Returns `(forward_adj, reverse_adj)` where:
/// - `forward_adj[u]` = `Vec<(dst, weight)>` — outgoing weighted edges
/// - `reverse_adj[v]` — presence set, used for dangling node detection
fn build_weighted_subgraph<S: Store>(
    store: &S,
    seeds: &[NodeId],
    filter: &dyn EdgeFilter,
) -> AnyResult<(WeightedAdj, HashMap<NodeId, ()>)> {
    use std::collections::HashSet;
    let _span = tracing::debug_span!("ppr.bfs_expand", seeds = seeds.len()).entered();

    const PPR_BFS_DEPTH: u8 = 6;

    /// Maximum nodes materialised into the PPR subgraph.
    ///
    /// A depth-6 BFS from a single hub node in a graph with fan-out F can
    /// reach O(F^6) nodes. Without a ceiling, a high-fan-out graph (F ≥ 20)
    /// can exhaust memory on a developer machine before iteration starts.
    /// 250,000 is well above the realistic neighbourhood of any single query
    /// at the MVP code-graph scale (< 75M total nodes), so genuine PPR
    /// quality is unaffected for normal codebases.
    const MAX_SUBGRAPH_NODES: usize = 250_000;

    /// Chunk size for batch edge queries.
    ///
    /// Stays well under SQLite's 999-parameter limit while collapsing the
    /// per-node query loop (~131k round-trips on kubernetes) into ≤ 500 batch
    /// queries. Fixed constant — does not depend on repo size.
    const EDGE_BATCH_SIZE: usize = 500;

    let mut adj: HashMap<NodeId, Vec<(NodeId, f32)>> = HashMap::new();
    let mut reverse_adj: HashMap<NodeId, ()> = HashMap::new();
    let mut visited: HashSet<NodeId> = HashSet::new();
    let mut total_edges_seen: usize = 0;

    let mut frontier: Vec<NodeId> = seeds
        .iter()
        .filter(|&&s| visited.insert(s))
        .copied()
        .collect();

    for _depth in 0..PPR_BFS_DEPTH {
        if frontier.is_empty() || visited.len() >= MAX_SUBGRAPH_NODES {
            break;
        }

        let mut next_frontier: Vec<NodeId> = Vec::new();

        for chunk in frontier.chunks(EDGE_BATCH_SIZE) {
            let edges = store.iter_edges_from_batch(chunk)?;

            // #413: gate every edge before it enters the subgraph, not after
            // scoring. Post-filtering a ranked list still lets a denied node's
            // existence show through timing and through the mass it steals from
            // permitted neighbours, which is what RFC-006 enforces against.
            //
            // The corpus of each destination is fetched once per chunk, beside
            // the edge query it belongs to, rather than per edge — a depth-6
            // BFS over a large repo would otherwise turn ~500 batch queries
            // into hundreds of thousands. A filter that ignores the corpus
            // skips the lookup entirely, so the local `OpenFilter` path costs
            // exactly what it did before.
            let dst_corpus: HashMap<NodeId, String> = if filter.needs_corpus() {
                let mut dsts: Vec<NodeId> = edges.iter().map(|e| e.dst).collect();
                dsts.sort_unstable();
                dsts.dedup();
                store
                    .get_nodes(&dsts)?
                    .into_iter()
                    .map(|n| (n.id, n.vname.corpus))
                    .collect()
            } else {
                HashMap::new()
            };

            for edge in &edges {
                if filter.needs_corpus()
                    && !filter.allow(
                        edge.src,
                        edge.dst,
                        dst_corpus.get(&edge.dst).map(String::as_str),
                    )
                {
                    // A destination missing from the store yields `None` here,
                    // which every filter must treat as a denial (fail-closed).
                    continue;
                }
                let w = edge.kind.ppr_weight();
                adj.entry(edge.src).or_default().push((edge.dst, w));
                reverse_adj.entry(edge.dst).or_insert(());
                total_edges_seen += 1;
                if visited.insert(edge.dst) {
                    next_frontier.push(edge.dst);
                }
            }
            // Nodes with no outgoing edges won't appear in edge results — ensure
            // they still have an adj entry so the PPR propagation loop sees them.
            for &src in chunk {
                adj.entry(src).or_default();
            }
            // Per-chunk ceiling: a single hub node in this chunk can add thousands
            // of neighbours before the depth-level check below fires. Break early so
            // the overshoot is bounded to one chunk's worth of new nodes, not the
            // entire remaining frontier × fan-out.
            if visited.len() > MAX_SUBGRAPH_NODES {
                tracing::warn!(
                    visited = visited.len(),
                    limit = MAX_SUBGRAPH_NODES,
                    "PPR subgraph exceeded node ceiling mid-chunk — truncating BFS"
                );
                break;
            }
        }

        if visited.len() > MAX_SUBGRAPH_NODES {
            tracing::warn!(
                visited = visited.len(),
                limit = MAX_SUBGRAPH_NODES,
                "PPR subgraph exceeded node ceiling — truncating BFS"
            );
            break;
        }

        frontier = next_frontier;
    }

    tracing::debug!(
        nodes_visited = adj.len(),
        edges_traversed = total_edges_seen,
        "ppr.bfs_expand complete"
    );
    Ok((adj, reverse_adj))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use travsr_core::{Edge, EdgeKind, VName};
    use travsr_store::{SqliteStore, Store};

    // Serialise env-var mutation tests to avoid data races.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn make_node(sig: &str) -> travsr_core::Node {
        travsr_core::Node::new(VName::new("", "", "test.ts", "typescript", sig), "function")
    }

    fn store_with(
        nodes: &[travsr_core::Node],
        edges: &[(NodeId, NodeId, EdgeKind)],
    ) -> SqliteStore {
        let mut store = SqliteStore::open_in_memory().unwrap();
        for n in nodes {
            store.put_node(n).unwrap();
        }
        for &(src, dst, kind) in edges {
            store.put_edge(&Edge::new(src, dst, kind)).unwrap();
        }
        store
    }

    // ── Constant / env-var tests (kept from ADR-003 PR) ───────────────────────

    #[test]
    fn defaults_match_adr() {
        assert_eq!(ALPHA, 0.85_f32);
        assert_eq!(EPSILON, 1e-6_f32);
        assert_eq!(MAX_ITERATIONS, 50_u32);
    }

    #[test]
    fn alpha_without_env_var_returns_default() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TRAVSR_PPR_ALPHA");
        assert_eq!(alpha(), ALPHA);
    }

    #[test]
    fn alpha_valid_override_is_used() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("TRAVSR_PPR_ALPHA", "0.90");
        let v = alpha();
        std::env::remove_var("TRAVSR_PPR_ALPHA");
        assert!((v - 0.90_f32).abs() < 1e-6, "valid override must be used");
    }

    #[test]
    fn alpha_rejects_zero_and_falls_back() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("TRAVSR_PPR_ALPHA", "0.0");
        let v = alpha();
        std::env::remove_var("TRAVSR_PPR_ALPHA");
        assert_eq!(v, ALPHA, "alpha=0.0 is invalid — must fall back to default");
    }

    #[test]
    fn alpha_rejects_one_and_above() {
        let _g = ENV_LOCK.lock().unwrap();
        for bad in ["1.0", "1.5", "99.0"] {
            std::env::set_var("TRAVSR_PPR_ALPHA", bad);
            let v = alpha();
            std::env::remove_var("TRAVSR_PPR_ALPHA");
            assert_eq!(v, ALPHA, "alpha={bad} >= 1.0 is invalid — must fall back");
        }
    }

    #[test]
    fn alpha_rejects_negative() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("TRAVSR_PPR_ALPHA", "-0.5");
        let v = alpha();
        std::env::remove_var("TRAVSR_PPR_ALPHA");
        assert_eq!(v, ALPHA, "negative alpha must fall back to default");
    }

    #[test]
    fn epsilon_without_env_var_returns_default() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TRAVSR_PPR_EPSILON");
        assert!((epsilon() - EPSILON).abs() < 1e-10);
    }

    #[test]
    fn max_iterations_without_env_var_returns_default() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TRAVSR_PPR_MAX_ITER");
        assert_eq!(max_iterations(), MAX_ITERATIONS);
    }

    #[test]
    fn max_iterations_valid_override_is_used() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("TRAVSR_PPR_MAX_ITER", "100");
        let n = max_iterations();
        std::env::remove_var("TRAVSR_PPR_MAX_ITER");
        assert_eq!(n, 100, "valid override must be used");
    }

    #[test]
    fn max_iterations_rejects_zero() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("TRAVSR_PPR_MAX_ITER", "0");
        let n = max_iterations();
        std::env::remove_var("TRAVSR_PPR_MAX_ITER");
        assert_eq!(
            n, MAX_ITERATIONS,
            "max_iterations=0 must fall back to default"
        );
    }

    #[test]
    fn parse_env_f32_missing_var_returns_default() {
        assert_eq!(parse_env_f32("__TRAVSR_TEST_NON_EXISTENT__", 0.85), 0.85);
    }

    // ── PPR algorithm tests ───────────────────────────────────────────────────

    /// Empty seeds returns empty result — no panic.
    #[test]
    fn ppr_empty_seeds_returns_empty() {
        let store = SqliteStore::open_in_memory().unwrap();
        let result = ppr(&store, &[], 5, &crate::OpenFilter).unwrap();
        assert!(result.is_empty());
    }

    /// Single isolated seed: must return the seed with score 1.0 (or close).
    #[test]
    fn ppr_isolated_seed_returns_seed_with_positive_score() {
        let a = make_node("fn:a");
        let store = store_with(std::slice::from_ref(&a), &[]);
        let result = ppr(&store, &[a.id], 5, &crate::OpenFilter).unwrap();
        assert!(!result.is_empty(), "must return at least the seed");
        assert_eq!(result[0].0, a.id, "seed must be first");
        assert!(result[0].1 > 0.0, "score must be positive");
    }

    /// Direct call edge: callee must score higher than unconnected nodes.
    #[test]
    fn ppr_direct_call_scores_callee_above_unconnected() {
        let caller = make_node("fn:caller");
        let callee = make_node("fn:callee");
        let unrelated = make_node("fn:unrelated");
        let store = store_with(
            &[caller.clone(), callee.clone(), unrelated.clone()],
            &[(caller.id, callee.id, EdgeKind::RefCall)],
        );
        let result = ppr(&store, &[caller.id], 10, &crate::OpenFilter).unwrap();
        let score_of = |id: NodeId| result.iter().find(|&&(n, _)| n == id).map(|&(_, s)| s);

        let callee_score = score_of(callee.id).unwrap_or(0.0);
        let unrelated_score = score_of(unrelated.id).unwrap_or(0.0);
        assert!(
            callee_score > unrelated_score,
            "callee ({callee_score}) must score above unrelated ({unrelated_score})"
        );
    }

    /// Scores are sorted descending.
    #[test]
    fn ppr_results_are_sorted_descending() {
        let a = make_node("fn:a");
        let b = make_node("fn:b");
        let c = make_node("fn:c");
        let store = store_with(
            &[a.clone(), b.clone(), c.clone()],
            &[
                (a.id, b.id, EdgeKind::RefCall),
                (a.id, c.id, EdgeKind::Depends),
            ],
        );
        let result = ppr(&store, &[a.id], 10, &crate::OpenFilter).unwrap();
        for window in result.windows(2) {
            assert!(
                window[0].1 >= window[1].1,
                "results must be sorted descending by score"
            );
        }
    }

    /// k=1 returns at most 1 result.
    #[test]
    fn ppr_k_limits_output_count() {
        let a = make_node("fn:a");
        let b = make_node("fn:b");
        let c = make_node("fn:c");
        let store = store_with(
            &[a.clone(), b.clone(), c.clone()],
            &[
                (a.id, b.id, EdgeKind::RefCall),
                (a.id, c.id, EdgeKind::RefCall),
            ],
        );
        let result = ppr(&store, &[a.id], 1, &crate::OpenFilter).unwrap();
        assert_eq!(result.len(), 1, "k=1 must return exactly 1 result");
    }

    /// RefCall-connected node scores higher than same-distance Depends node.
    #[test]
    fn ppr_ref_call_scores_higher_than_depends() {
        let seed = make_node("fn:seed");
        let via_call = make_node("fn:via_call");
        let via_dep = make_node("fn:via_dep");
        let store = store_with(
            &[seed.clone(), via_call.clone(), via_dep.clone()],
            &[
                (seed.id, via_call.id, EdgeKind::RefCall),
                (seed.id, via_dep.id, EdgeKind::Depends),
            ],
        );
        let result = ppr(&store, &[seed.id], 10, &crate::OpenFilter).unwrap();
        let score_of = |id: NodeId| result.iter().find(|&&(n, _)| n == id).map(|&(_, s)| s);
        let call_score = score_of(via_call.id).unwrap_or(0.0);
        let dep_score = score_of(via_dep.id).unwrap_or(0.0);
        assert!(
            call_score > dep_score,
            "RefCall ({call_score}) must score above Depends ({dep_score})"
        );
    }

    /// Cycle in the graph must not cause infinite loop or panic.
    #[test]
    fn ppr_handles_cycles() {
        let a = make_node("fn:a");
        let b = make_node("fn:b");
        let store = store_with(
            &[a.clone(), b.clone()],
            &[
                (a.id, b.id, EdgeKind::RefCall),
                (b.id, a.id, EdgeKind::RefCall),
            ],
        );
        // Must terminate — no infinite loop.
        let result = ppr(&store, &[a.id], 10, &crate::OpenFilter).unwrap();
        assert!(!result.is_empty());
    }

    // ── ppr_weighted tests ────────────────────────────────────────────────────

    #[test]
    fn ppr_weighted_empty_seeds_returns_empty() {
        let store = SqliteStore::open_in_memory().unwrap();
        let result = ppr_weighted(&store, &[], 5, &crate::OpenFilter).unwrap();
        assert!(result.is_empty());
    }

    /// Neighbor of the high-weight seed must score above neighbor of the low-weight seed
    /// when the graph topology is otherwise symmetric (equal fan-out on both sides).
    #[test]
    fn ppr_weighted_higher_weight_seed_neighbor_scores_above_lower() {
        let a = make_node("fn:a");
        let b = make_node("fn:b");
        let x = make_node("fn:x");
        let y = make_node("fn:y");
        let store = store_with(
            &[a.clone(), b.clone(), x.clone(), y.clone()],
            &[
                (a.id, x.id, EdgeKind::RefCall),
                (b.id, y.id, EdgeKind::RefCall),
            ],
        );
        let result =
            ppr_weighted(&store, &[(a.id, 0.9), (b.id, 0.1)], 10, &crate::OpenFilter).unwrap();
        let score_of = |id: NodeId| {
            result
                .iter()
                .find(|&&(n, _)| n == id)
                .map(|&(_, s)| s)
                .unwrap_or(0.0)
        };
        let x_score = score_of(x.id);
        let y_score = score_of(y.id);
        assert!(
            x_score > y_score,
            "neighbor of high-weight seed ({x_score:.4}) must outscore neighbor of low-weight seed ({y_score:.4})"
        );
    }

    /// Results from ppr_weighted with equal weights must be consistent with ppr (uniform).
    #[test]
    fn ppr_weighted_uniform_weights_consistent_with_ppr() {
        let a = make_node("fn:a");
        let b = make_node("fn:b");
        let store = store_with(&[a.clone(), b.clone()], &[(a.id, b.id, EdgeKind::RefCall)]);
        let uniform = ppr(&store, &[a.id, b.id], 10, &crate::OpenFilter).unwrap();
        let weighted =
            ppr_weighted(&store, &[(a.id, 1.0), (b.id, 1.0)], 10, &crate::OpenFilter).unwrap();
        // Both must return the same set of node IDs in the same order.
        let ids_uniform: Vec<NodeId> = uniform.iter().map(|&(id, _)| id).collect();
        let ids_weighted: Vec<NodeId> = weighted.iter().map(|&(id, _)| id).collect();
        assert_eq!(
            ids_uniform, ids_weighted,
            "uniform ppr_weighted must produce same ordering as ppr"
        );
    }

    /// Degenerate zero-total weights must not panic; falls back to uniform.
    #[test]
    fn ppr_weighted_degenerate_zero_total_falls_back() {
        let a = make_node("fn:a");
        let store = store_with(std::slice::from_ref(&a), &[]);
        let result = ppr_weighted(&store, &[(a.id, 0.0)], 5, &crate::OpenFilter);
        assert!(result.is_ok(), "zero-weight seeds must not error");
        assert!(
            !result.unwrap().is_empty(),
            "must still return the seed node"
        );
    }
}
