# RFC-025: `get_execution_path` — Corridor Shape and Prize Function, and the Gate That Decides Whether GW Is Built

**Status:** Draft — for discussion, not yet proposed for sign-off
**Author:** Ritik
**Date:** 2026-08-11
**Crates affected:** `travsr-retrieval`
**Depends on:** ADR-007 (λ selection, corrected), RFC-006 (graph RBAC session model), RFC-010 (knapsack budget enforcer)
**Issue:** #527 — phase P1

---

## Summary

`get_execution_path` returns roughly ten times more nodes than the route it was asked
for, and the padding is largely the same nodes regardless of the query. This RFC argues
the cause is not the missing Goemans-Williamson (GW) solver that #527 scoped for S16.
It is the shape of the node-selection rule: the shipped filter is a **ball around the
source**, and it will keep admitting off-route nodes no matter which algorithm computes
the route.

It proposes a specific replacement — a **detour-bounded corridor** requiring the sink
distance as well as the source distance — defines the prize function that a GW
implementation would need, and specifies the benchmark that decides between three
candidates. The intended outcome of P1 is a decision on *whether to build GW at all*,
not a commitment to build it.

---

## Motivation

### What ships today

After #648, `pcst.rs` computes `cost = 1.0 / ppr_weight`, runs Dijkstra from the source
over a local subgraph (≤ `MAX_LOCAL_NODES` = 2000, `EXPAND_DEPTH` = 5), extracts the
source→sink route with A*, and then pads the result with every node satisfying

```
d(source, v)  ≤  (1 + λ) · d(source, sink)
```

That predicate mentions the sink exactly once, as a scalar radius. It never asks whether
`v` is *between* source and sink. Any node within the radius qualifies, including nodes
pointing directly away from the sink. It is a ball, not a corridor, whatever the module
doc has called it.

### The measurement

Two corpora appear below and they are **not** comparable to each other; each is
internally consistent. Corpus A is the #533 checked-in pair set measured on 2026-08-03.
Corpus B is a regenerated pair set on current master (the checked-in pairs score 36.1%
recall against a current index, so they are stale — see *Unresolved Questions*).

Corpus B, λ swept with costs non-zero (36 pairs):

| λ | recall | mean nodes | path share |
|---|---|---|---|
| 0.0 | 100% | 37.1 | 0.172 |
| 0.1 | 100% | 37.1 | 0.172 |
| 0.25 | 100% | 37.3 | 0.171 |
| 0.5 | 100% | 42.0 | 0.150 |
| 1.0 | 100% | 43.3 | 0.146 |

**λ = 0 still returns 37 nodes for a 3-to-5 node route.** That floor is the ball. λ
cannot reach below it, because at λ = 0 the predicate is still `d(source, v) ≤
d(source, sink)` — the entire disc of radius `d(source, sink)`, not the route.

Corpus A, on the padding's composition: 911 padding nodes drawn from only **170
distinct** symbols, 25 of which appear in ≥ 10 of 36 queries. The padding is
query-independent, which is what a source-radius rule predicts and a genuine relevance
rule does not.

### Why the obvious diagnosis is wrong

I assumed the repeated nodes were high-degree hubs. Only two are: `fn:get` (in-degree
178) and `Session.filter` (119), against a median of 1 and p95 of 11. The rest are
low-degree — `status_for` 1, `spec` 3, `global_path` 5, `load_table` 5, `repo_path` 4.

So the mechanism is indirect: a hub is cheaply reachable from nearly anywhere, and once
the ball reaches the hub, the hub's entire low-degree neighbourhood falls inside it too.
One gateway drags in a cluster. This is the "hub cascade" in the #527 thread.

This matters for GW specifically. **If prize is a global centrality score — PPR, the
obvious choice — then `fn:get` at in-degree 178 scores high and GW will keep including
it.** On this evidence GW could make the hub cascade worse, not better. The prize
function, not the algorithm, decides whether S16 succeeds.

---

## Detailed Design

### D1 — Formulation: 2-terminal rooted PCST

`source` and `sink` are mandatory terminals (infinite prize). Every other node in the
local subgraph carries an optional non-negative prize. Edges keep the `1.0 / ppr_weight`
cost model from #648. RBAC filtering stays in `expand_local_subgraph`, so everything
below operates on an already-filtered subgraph and inherits RFC-006 unchanged.

This is NP-hard; `MAX_LOCAL_NODES` bounds the input to an approximation, it does not
make an exact solver free.

### D2 — Prize must be sink-conditioned, not a global centrality

**This is the load-bearing decision of this RFC.**

Define, over the local subgraph:

- `d_s(v)` — cheapest cost `source → v`
- `d_t(v)` — cheapest cost `v → sink`, computed by a second Dijkstra on the **reversed**
  graph, seeded at the sink
- `detour(v) = d_s(v) + d_t(v) − d(source, sink)`, which is `≥ 0`, and `= 0` exactly when
  `v` lies on some optimal route

`detour` is the quantity the current filter should have been using and is not. A node
one hop from the source but pointing away from the sink has small `d_s` and large `d_t`;
the ball admits it, `detour` rejects it. A node with no path to the sink has
`d_t = ∞` and is excluded outright — today it is admitted whenever it is near the source.

Prize is then a decreasing function of `detour`, not of centrality. The exact form is a
P2 tuning question; the decision here is the **argument**, not the curve.

This also disarms the hub cascade at its root. An off-route gateway hub has large `d_t`,
so neither it nor its neighbourhood enters the result — without any special-casing of
degree.

### D3 — Candidate C2: detour corridor without GW

D2 yields a cheap change that needs no primal-dual solver. Replace the selection
predicate with

```
detour(v)  ≤  λ · d(source, sink)
```

Cost: one additional Dijkstra on the reversed subgraph. Same `O((n + m) log n)`, same
`n ≤ 2000`, microseconds. No new failure surface, no change to the call boundary.

**If C2 captures most of the available win, GW is unjustified and #527 closes by
re-affirming the heuristic** — an outcome P2's gate explicitly permits. C2 must
therefore be benchmarked as a first-class candidate, not as a straw man.

### D4 — Determinism

Ties are common and get more common under D3, because cost depends only on edge kind:
every `ref/call` hop costs exactly 1.0, so equal-shaped routes score equal. Current code
handles this by settling the full component (`dijkstra(.., None, ..)`) and breaking ties
on `NodeId`.

Both remaining candidates must preserve that discipline:

- C2: sort context by `(detour, NodeId)`, mirroring today's `(cost, NodeId)`.
- C3 (GW): the moat-growing phase has genuine simultaneous-event ties. Every
  edge-tighten event must carry a `NodeId`-based tie-break, with a determinism test
  analogous to `pcst_is_deterministic_on_tied_cost_topology`.

### D5 — Output ordering contract for knapsack

Today's contract — **route first in traversal order, then context ascending by cost** —
is pinned by the #317 regression test and relied on downstream: consumers truncate (token
budget in `pcst_path`, 4 KiB sanitizer at the MCP boundary), so a sink appended after the
padding is cut off every time.

This contract is **preserved, not renegotiated**:

- C2: route first, then context ascending by `detour`. Direct substitution.
- C3: GW returns a *tree*, not a linear route. Route extraction is defined as the
  source→sink path within the returned tree (unique in a tree), emitted first; remaining
  tree nodes follow ascending by `detour`. Any GW implementation that cannot produce
  this ordering fails this RFC.

### D6 — What does not change

`pcst_path`'s signature, the MCP tool contract, the BFS fallback triggers (endpoint
missing from the local subgraph, or no path found), the SEC P0 equivalence of "not
found" and "access denied", and the absence of a wall-clock gate.

---

## Alternatives Considered

**C1 — Re-affirm the heuristic, change nothing.** Cheapest. Rejected as a *default*
because the ball is a genuine defect, not a documented tradeoff, and λ demonstrably
cannot fix it. Remains the correct outcome if C2 also fails to beat the baseline.

**C2 — Detour corridor (D3).** Proposed.

**C3 — Goemans-Williamson.** What #527 originally scoped. Not rejected, but no longer
the presumed answer: it is expensive, adds a new failure surface, and on the hub-cascade
evidence its benefit depends entirely on D2 — which C2 also gets, for far less.

**Tune λ downward.** Already measured: λ 0.5 → 0.25 saves ~5 nodes at no recall cost.
Worth doing as an ADR-007 amendment, independent of this RFC, but it does not touch the
37-node floor.

**Degree-penalised prize.** Special-cases the symptom (hubs) rather than the cause
(no sink conditioning), and needs a new tunable. D2 gets it for free.

---

## Drawbacks

- C2 can return *fewer* nodes than consumers currently expect. Fewer-but-relevant is the
  goal, but the harness must confirm recall holds at 100% and that `get_context`-style
  consumers are not silently starved.
- `detour` requires the reverse Dijkstra to be computed on the same filtered subgraph;
  getting that wrong is an RBAC leak. Covered by the existing `pcst_respects_rbac_filter`
  tests, which must be extended to the reverse pass.
- This RFC narrows S16 to a decision rather than delivering the algorithm it promised.
  That is deliberate, and the honest reading of the evidence.

---

## Unresolved Questions

1. **The functional form of prize under C3.** Deliberately unsettled; D2 fixes only the
   argument. If C2 wins, this question dies with it.
2. **The #533 pair set goes stale.** It scores 36.1% recall against a current index; the
   same pairs regenerated score 100% with unmodified code. P2 is not reproducible until
   the harness either regenerates at bench time or the pairs are refreshed on a schedule.
   This needs an owner regardless of the outcome here.
3. **Does `d_t = ∞` prune too aggressively on real graphs?** Directed reachability to the
   sink may be rarer than intuition suggests. Measurable in P2 as a distribution before
   anything is shipped.

---

## Acceptance Criteria

P2 runs the #533 harness over regenerated pairs, comparing three arms.

Corpus note, because #527's P2 text gets this wrong: the k8s data under `bench/` is
**`get_context` query sets**, and none of it contains a `(source, sink)` pair. The only
execution-path pairs that exist are `bench/queries-execpath-travsr.json`. A k8s arm
requires generating pairs first with `bench/gen-execpath-pairs.mjs` against a k8s index.
Whether P2 is gated on that second corpus, or runs on the travsr self-graph alone, is a
scoping call for sign-off — one corpus is enough to disqualify an arm, not enough to
generalise a win.

| arm | description |
|---|---|
| C1 | shipped code, post-#648, λ = 0.5 |
| C2 | detour corridor, λ swept |
| C3 | GW prototype, prize per D2 |

Reported per arm: recall (sink returned), mean nodes returned, path share, and the
count of symbols appearing in ≥ 10 of 36 queries (the query-independence measure that
exposed the ball).

Gates, in order:

1. **Recall must stay at 100%** for any arm to be eligible. An arm that loses the sink is
   disqualified regardless of its node count.
2. If **C2 clears C1** on path share and query-independence, C2 ships and **C3 is not
   built**. #527 closes with a P4 ADR recording that GW was evaluated and declined on
   evidence — *intentional and documented, not deferred*.
3. C3 proceeds to P3 only if it **clears C2**, not merely C1. Beating the arm we are
   replacing is not the bar; beating the cheap alternative is.
4. If neither clears C1, C1 is re-affirmed and #527 closes on the same terms as (2).

---

## Rollout

P1 is this document plus sign-off (Principal Architect + Tech Lead — retrieval algorithm
change per CLAUDE.md). P2 is script-level only, under `bench/`, no production code. P3
happens only under gate (3), behind the unchanged `pcst_path` boundary. P4 supersedes
ADR-007 by marking it Superseded rather than rewriting it in place; CLAUDE.md and the
retrieval stack table are updated only after ship and verification.

The hub cascade needs an owner under **every** outcome above. If C2 or C3 ships, it is
subsumed; if C1 is re-affirmed, the padding problem survives and should be tracked as its
own issue rather than closed along with #527.
