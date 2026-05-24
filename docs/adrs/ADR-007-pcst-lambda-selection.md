# ADR-007: PCST λ Parameter Selection (λ = 0.5)

**Date:** 2026-05-24  
**Status:** Accepted

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

PCST computation on large graphs (>10k nodes) can exceed p95 50ms budget. A hard 80ms wall-clock timeout is enforced; on timeout the algorithm falls back to BFS depth-3. This is logged at WARN level so operators can track fallback frequency.
