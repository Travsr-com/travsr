# ADR-007: PCST λ Parameter Selection (λ = 0.5)

**Date:** 2026-05-24  
**Status:** Accepted, with corrections (see below)

---

> ## ⚠️ Correction — 2026-08-03 (issue #527)
>
> Two statements in this ADR do not describe what shipped. The λ = 0.5 decision
> itself is accurate and stands; the surrounding description is not. Recorded
> here as an amendment rather than an in-place edit, so the original decision
> record survives for the eventual supersede.
>
> **1. "Context" below calls PCST the production algorithm.** It is not
> implemented. `crates/travsr-retrieval/src/pcst.rs` runs Dijkstra (plus A* with
> a zero heuristic, which degenerates to Dijkstra) over a bounded local
> subgraph, then pads the result with nodes whose cheapest-path cost is within
> `λ × total_cost`. That padding step is the only PCST-flavoured part; there is
> no Goemans-Williamson primal-dual approximation anywhere in the crate.
>
> **2. The "Timeout" section below is inverted.** No wall-clock timeout exists.
> `pcst.rs` deliberately does *not* gate on elapsed time — the comment above
> `expand_local_subgraph` explains why: such a check can only fire after the
> work it nominally guards has already completed, so its only effect would be to
> discard a complete bounded subgraph and make results nondeterministic under
> load. The BFS fallback fires when source or sink is absent from the local
> subgraph, or when no path exists — never on a timer.
>
> Real PCST is scoped as S16 and gated on a benchmark demonstrating it beats the
> current heuristic. If that gate is not cleared, this ADR should be superseded
> by one that records the heuristic as the intended design rather than a
> deferral.

---

## Context

Prize-Collecting Steiner Tree (PCST) is the production retrieval algorithm for `get_execution_path`. PCST requires a penalty parameter λ that controls the trade-off between including more nodes (lower λ) and minimising total edge cost (higher λ). The parameter is defined in the Goemans-Williamson 1995 approximation formulation:

```
minimise: Σ edge_costs + λ · Σ node_penalties_for_excluded_nodes
```

A λ too close to 0 degrades to BFS (include everything). A λ too close to 1 produces a minimal Steiner path that misses important intermediate context.

---

## Decision

**λ = 0.5** for all PCST calls in S14.

This value is hardcoded in `pcst.rs` as `const PCST_LAMBDA: f32 = 0.5;` and referenced in `ADR-007`. It is tunable via `travsr.toml` in S16 when benchmark data from real repos is available.

---

## Consequences

### Positive

- Balanced trade-off: includes intermediate context nodes without degenerating to BFS.
- Empirically chosen value from graph retrieval literature (see Ghuge & Nagarajan 2022 PCST survey).
- Simple to understand and explain to operators.
- Allows BFS fallback path to activate on large graphs without operator configuration.

### Negative

- Not tuned to Travsr-specific graph structure. Real graph PPR scores may benefit from λ closer to 0.3 or 0.7.
- A single global λ may be sub-optimal for different query types (short path vs. wide context).
- Will require empirical tuning in S16 once Criterion benchmarks have baseline data.

---

## Rationale for not using λ = 0.85 (PPR weight)

PPR weights (ADR-003) measure edge _transition probability_. PCST λ measures node _exclusion penalty_ relative to edge cost. These are different concepts in different algorithms. Reusing 0.85 would conflate them.

---

## Timeout

> **Superseded by the correction at the top of this file (issue #527).** The
> paragraph below describes a wall-clock timeout that was never shipped, or was
> removed without updating this ADR. Retained verbatim as the original record.

~~PCST computation on large graphs (>10k nodes) can exceed p95 50ms budget. A hard 80ms wall-clock timeout is enforced; on timeout the algorithm falls back to BFS depth-3. This is logged at WARN level so operators can track fallback frequency.~~

**Actual behaviour.** Cost is bounded structurally, not temporally:
`expand_local_subgraph` stops after `MAX_LOCAL_NODES` (2000) node pops, so the
Dijkstra/A* pass below it is microsecond-scale regardless of graph fan-out. The
BFS depth-3 fallback triggers only when source or sink is missing from the local
subgraph, or when `astar` finds no path.
