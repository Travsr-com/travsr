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

/// Personalized PageRank over the code graph.
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
/// - `store` — any `Store` implementor (SQLite or Kùzu)
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
) -> Result<Vec<(NodeId, f32)>, TravsrError> {
    ppr_inner(store, seeds, k).map_err(|e| TravsrError::Internal(e.to_string()))
}

fn ppr_inner<S: Store>(store: &S, seeds: &[NodeId], k: usize) -> AnyResult<Vec<(NodeId, f32)>> {
    let _span = tracing::debug_span!("ppr", seeds = seeds.len(), k = k).entered();

    if seeds.is_empty() {
        return Ok(Vec::new());
    }

    let a = alpha();
    let eps = epsilon();
    let max_iter = max_iterations();

    // ── BFS to collect all reachable nodes + their outgoing edges ─────────────
    // We materialise the subgraph once so the inner iteration loop never hits
    // the store — critical for meeting the p95 < 50ms budget.
    let (adj, reverse_adj) = build_weighted_subgraph(store, seeds)?;

    if adj.is_empty() {
        // Seeds not in the graph — return seeds with score 1/n each.
        let score = 1.0 / seeds.len() as f32;
        let mut out: Vec<(NodeId, f32)> = seeds.iter().map(|&id| (id, score)).collect();
        if k > 0 {
            out.truncate(k);
        }
        return Ok(out);
    }

    // ── Personalisation vector r₀ ─────────────────────────────────────────────
    let seed_score = 1.0 / seeds.len() as f32;
    let mut r: HashMap<NodeId, f32> = HashMap::with_capacity(adj.len());
    for &s in seeds {
        *r.entry(s).or_insert(0.0) += seed_score;
    }

    // ── Power iteration ───────────────────────────────────────────────────────
    let mut iterations_run: u32 = 0;
    for iter in 0..max_iter {
        let mut r_next: HashMap<NodeId, f32> = HashMap::with_capacity(r.len());

        // Teleportation: (1 − α) · r₀
        for &s in seeds {
            *r_next.entry(s).or_insert(0.0) += (1.0 - a) * seed_score;
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
    scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
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
) -> AnyResult<(WeightedAdj, HashMap<NodeId, ()>)> {
    use std::collections::{HashSet, VecDeque};
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

    let mut adj: HashMap<NodeId, Vec<(NodeId, f32)>> = HashMap::new();
    let mut reverse_adj: HashMap<NodeId, ()> = HashMap::new();
    let mut visited: HashSet<NodeId> = HashSet::new();
    let mut queue: VecDeque<(NodeId, u8)> = VecDeque::new();
    // Accumulated inside the BFS loop (same pass) — no extra O(V) post-BFS sum.
    let mut total_edges_seen: usize = 0;

    for &s in seeds {
        if visited.insert(s) {
            queue.push_back((s, 0));
        }
    }

    while let Some((node, depth)) = queue.pop_front() {
        if visited.len() > MAX_SUBGRAPH_NODES {
            tracing::warn!(
                visited = visited.len(),
                limit = MAX_SUBGRAPH_NODES,
                "PPR subgraph exceeded node ceiling — truncating BFS"
            );
            break;
        }

        let edges = store.iter_edges_from(node)?;
        let mut outgoing: Vec<(NodeId, f32)> = Vec::with_capacity(edges.len());

        for edge in edges {
            let w = edge.kind.ppr_weight();
            outgoing.push((edge.dst, w));
            reverse_adj.entry(edge.dst).or_insert(());
            total_edges_seen += 1;

            if depth < PPR_BFS_DEPTH && visited.insert(edge.dst) {
                queue.push_back((edge.dst, depth + 1));
            }
        }

        adj.insert(node, outgoing);
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
        let result = ppr(&store, &[], 5).unwrap();
        assert!(result.is_empty());
    }

    /// Single isolated seed: must return the seed with score 1.0 (or close).
    #[test]
    fn ppr_isolated_seed_returns_seed_with_positive_score() {
        let a = make_node("fn:a");
        let store = store_with(std::slice::from_ref(&a), &[]);
        let result = ppr(&store, &[a.id], 5).unwrap();
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
        let result = ppr(&store, &[caller.id], 10).unwrap();
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
        let result = ppr(&store, &[a.id], 10).unwrap();
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
        let result = ppr(&store, &[a.id], 1).unwrap();
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
        let result = ppr(&store, &[seed.id], 10).unwrap();
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
        let result = ppr(&store, &[a.id], 10).unwrap();
        assert!(!result.is_empty());
    }
}
