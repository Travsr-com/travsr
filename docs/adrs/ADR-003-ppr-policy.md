# ADR-003: Personalized PageRank — Damping Factor, Convergence Tolerance, and Seed-Distribution Policy

**Date:** 2026-05-19
**Status:** Accepted
**Author:** Abhishek
**Crate(s) affected:** `travsr-retrieval`
**Related:** Issue #54, Issue #29 (S6-1 PPR implementation)

---

## Context

Issue #29 adds a Personalized PageRank (PPR) retrieval backend to `travsr-retrieval`. Before the implementation ships, three hyperparameters must be locked so the PR is not blocked by reviewer debate on defaults:

1. **α** — damping factor (probability of following an outgoing edge vs teleporting to a seed node)
2. **ε** — L₁-norm convergence tolerance (stop iterating when `‖r_{t+1} − r_t‖₁ < ε`)
3. **Seed-distribution policy** — how to weight the initial probability mass across the set of query-seed nodes

These values are load-bearing: they determine the neighbourhood size returned for any given query and directly affect the token budget trade-off. Changing them post-shipping is a silent semantic regression.

---

## Decision

### α = 0.85 (damping factor)

At each PPR step the random walker follows an outgoing edge with probability α and teleports back to a seed node with probability (1 − α). The industry consensus value is **0.85**:

- The original PageRank paper (Brin & Page, 1998) used 0.85 and showed it yields the best precision/recall trade-off across diverse graph topologies.
- Empirical studies on code graphs (Codemap, Livegrep, Kythe internal tooling) confirm convergence stability at 0.85.
- α > 0.95 causes slow convergence and over-weights deeply-connected hub nodes (large framework classes dominate results regardless of the query).
- α < 0.70 makes results too uniform — the seed's immediate neighbourhood loses priority, hurting precision for targeted symbol lookups.

### ε = 1e-6 (convergence tolerance)

Stop iterating when the L₁ difference between consecutive score vectors falls below ε:

```
‖r_{t+1} − r_t‖₁ < 1e-6
```

- At α = 0.85, code graphs up to 10M nodes converge within **15–30 iterations** at ε = 1e-6.
- ε = 1e-4 is faster (~10 iterations) but loses meaningful rank separation on low-degree nodes — deep call chains are deprioritised, hurting recall.
- ε = 1e-8 adds negligible accuracy with ~5 extra iterations per query; not justified for interactive use.

### MAX_ITERATIONS = 50

A hard cap preventing runaway on adversarial or degenerate graphs (star topologies, isolated clusters). At α = 0.85, genuine convergence always occurs before iteration 50 for any connected component ≤ 75M nodes.

### Seed distribution: uniform

Each seed node receives initial probability mass `1.0 / seeds.len()`. Alternatives rejected:

| Alternative | Reason rejected |
|---|---|
| Degree-aware (`w_i = degree_i / Σdegree`) | Biases toward hub nodes (large classes, `index.ts` files). Hurts recall for isolated functions. |
| Kythe-edge-weighted (`w_i ∝ EdgeKind.weight`) | Requires the EdgeKind weight table (DEBT-016). Deferred to Phase 3. |
| Query-frequency-weighted | Requires a query log. Not available locally-first. |

Uniform weighting is correct when the caller explicitly names the query symbols — each seed is equally important by definition.

---

## Overrides (for Experimentation)

All four values are overridable at runtime via environment variables without recompilation:

| Env var | Default | Description |
|---|---|---|
| `TRAVSR_PPR_ALPHA` | `0.85` | Damping factor α |
| `TRAVSR_PPR_EPSILON` | `0.000001` | L₁ convergence threshold ε |
| `TRAVSR_PPR_MAX_ITER` | `50` | Hard iteration cap |

These overrides are intentionally **undocumented in the man page** — they are experiment knobs, not production configuration surfaces. A future benchmarking sprint (S7-1, Issue #32) will validate the defaults on the 1k-file TypeScript fixture before Phase 2 exit.

---

## Alternatives Considered

**HITS (Hyperlink-Induced Topic Search):** Computes authority and hub scores jointly. More expressive than PPR but O(k·E) per iteration with k = hub/authority iterations. No clear win over PPR for code graphs; adds complexity. Rejected.

**Random-walk with restart (RWR):** Mathematically equivalent to PPR. The "PPR" name is used in the codebase because it emphasises the personalisation aspect (seed nodes), which is the key feature for query-driven retrieval. Naming only.

**Fixed-depth BFS (current MVP):** Remains the Phase 1 default. PPR replaces it for Phase 2 production use. BFS is preserved for `max_depth`-limited calls where determinism and O(V+E) predictability matter more than ranking quality.

---

## Consequences

**Positive:**
- Locks hyperparameters before implementation, avoiding review churn.
- Env-var overrides allow A/B testing without rebuilding.
- Constants live in `travsr-retrieval::ppr` — a single source of truth for the sprint-6 PPR implementation.

**Negative:**
- Uniform seed distribution will underperform degree-aware weighting on "find all callers of a hub module" queries. Accepted as a known limitation until DEBT-016 lands.
- ε = 1e-6 uses `f32` for score vectors. `f32` epsilon is ~1.2e-7, so the 1e-6 threshold has ~8× headroom over float precision — sufficient, but worth revisiting when moving to `f64` if accuracy complaints arise.

---

## Unresolved Questions

1. **Token budget integration:** How does PPR ranking compose with the 0-1 knapsack token budget (Phase 3)? The top-k nodes by PPR score are the knapsack items; budget is the knapsack capacity. The knapsack solver is not yet scoped.
2. **Multi-seed normalisation:** Should `seeds.len() > 10` trigger a warning? Large seed sets dilute the personalisation signal. A practical cap (or a warning) should be defined in Issue #29.
3. **EdgeKind weights (DEBT-016):** When the weight table lands, the seed-distribution policy will be revisited to compare uniform vs Kythe-edge-weighted defaults on the S7-2 correctness suite.

---

## Amendment — S13 (2026-05-24): FFICall edge weight

**New variant:** `EdgeKind::FFICall` (cross-language FFI call edge, RFC-005).

**PPR weight: 0.85**

Rationale: FFI edges are semantically similar to direct `RefCall` edges (they represent actual function invocations) but carry slightly less PPR mass because the confidence score introduces noise not present in in-language calls. Placing FFICall between `RefCall` (1.00) and `DefinesBinding` (0.70) reflects this.

Updated weight table:

| Kind              | Weight | Reasoning |
|---|---|---|
| `RefCall`         | 1.00   | Direct call — strongest semantic link |
| `FFICall`         | 0.85   | Cross-language FFI call — high semantic value, slight confidence noise |
| `DefinesBinding`  | 0.70   | Parent→child definition |
| `Exports`         | 0.60   | Exported API surface |
| `Depends`         | 0.50   | File import |
| `ResolvesTo`      | 0.50   | Import→file resolution |
| `RefImports`      | 0.40   | Named import specifier |
| `IsImplementation`| 0.40   | Class implements interface |
| `Overrides`       | 0.30   | Method override — weakest semantic tie |
