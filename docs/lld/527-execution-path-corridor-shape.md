# LLD 527: execution-path corridor shape (design note, no code change)

**Status:** design analysis. Refs #527 (P1/P2), RFC PR #654.
**Verdict:** the remaining work is an RFC-scale production retrieval change. It is
deferred to #654 and its P2 benchmark gate. This document records the root cause,
why the surface reading of #527 is incomplete, and what #654 still needs before
sign-off.

---

## Problem

`get_execution_path` returns roughly 37 to 43 nodes for a route of 3 to 5 nodes
(#527 P2 measurements, 36 pairs on this repo's own graph). About 78% of the
response is padding rather than route. The issue title frames this as a naming
and documentation defect: the module is called `pcst` and ADR-007 claims
Goemans-Williamson, but the code is Dijkstra plus a threshold.

The naming half is already fixed. `crates/travsr-retrieval/src/pcst.rs:6-10`
states plainly that this is not PCST, and
`docs/adrs/ADR-007-pcst-lambda-selection.md:8-33` carries a dated correction
block. Neither changed a single node of output.

## Root cause

The defect is not which algorithm computes the route. It is the predicate that
selects the context around it.

`crates/travsr-retrieval/src/pcst.rs:215-217`:

```rust
let threshold = total_cost * (1.0 + pcst_lambda());
let mut context: Vec<(f32, NodeId)> = costs
    .iter()
    .filter(|(_, &c)| c <= threshold)
```

`costs` comes from a single source-seeded Dijkstra at `pcst.rs:184`:
`dijkstra(&graph, src_idx, None, |e| *e.weight())`. So the predicate expands to

```
d(source, v) <= (1 + lambda) * d(source, sink)
```

The sink appears exactly once, as a scalar radius. Nothing in the predicate asks
whether `v` lies *between* source and sink. This is a ball centred on the
source, not a corridor. A node one hop from the source and pointing directly
away from the sink is admitted. A node with no path to the sink at all is
admitted. Both are indistinguishable from an on-route node under a
source-distance-only test.

Three consequences follow, and all three are observed:

1. **A floor that lambda cannot reach.** At `lambda = 0` the threshold is
   `d(source, sink)` and the ball is still the full disc of that radius: 37.1
   mean nodes, path share 0.172 (#654). Sweeping lambda moves the result
   monotonically and never approaches the route size.
2. **Query-independent padding.** The ball depends on the source and on a scalar
   radius, so the same high-in-degree symbols recur across unrelated queries
   (the hub cascade, #527 judgement pass).
3. **Unreachable nodes ranked as context.** `d_t = infinity` is never computed,
   so it can never prune.

**Why the surface reading is incomplete.** Substituting Goemans-Williamson for
the Dijkstra pass replaces the route computation and the prize *solver*. It does
not change the argument the prize is a function of. If the prize stays a
source-distance or centrality quantity, GW re-derives the same ball with more
machinery, and on a centrality prize it plausibly makes the hub cascade worse,
since the hubs score highest. The naming defect and the output defect share a
file, not a cause.

This was empirically masked until recently: #648 fixed `edge_cost` from
`1.0 - ppr_weight` to `1.0 / ppr_weight`, because `ref/call` weighted 1.00 cost
exactly zero, making `threshold = 0 * (1 + lambda) = 0` for every lambda. Before
that fix no lambda sweep could distinguish anything, and the ball shape was
invisible behind a knob that had never been connected.

## Scope assessment: why this is not a drive-by fix

The mechanical change implied by the root cause is small: a second Dijkstra on
the reversed subgraph for `d_t`, then swap the predicate to
`detour(v) = d_s(v) + d_t(v) - d(source, sink) <= lambda * d(source, sink)`.
Perhaps twenty lines. Small diff, large blast radius:

- It changes the output of a production MCP tool on every call, for every user.
- Retrieval algorithm replacement is on this repo's explicit sign-off list
  (Principal Architect plus Tech Lead), which #527's own phase plan restates as
  P1, and #654 is still an open draft, not yet proposed for sign-off.
- #527 P2 defines a benchmark gate with three arms (C1 shipped, C2 detour
  corridor, C3 GW). Landing C2 by hand pre-empts that gate: gate 3 asks C3 to
  beat C2, which is unanswerable once C2 has shipped unmeasured.
- The reduction is one-directional. C2 returns strictly fewer nodes than C1.
  Recall staying at 100% is an empirical claim, and the only pair set that can
  test it (`bench/queries-execpath-travsr.json`) is currently unreproducible: the
  #533 pairs score 36.1% recall against a fresh index where regenerated pairs
  score 100% on unmodified code (#654 Unresolved Q2).

A change that is twenty lines to write and needs a benchmark harness fixed, a
three-arm comparison run, and two sign-offs to justify, is RFC-scale by every
measure except diff size. Hence: no code PR here.

## Options considered

**Implement Goemans-Williamson now.** Rejected. NP-hard formulation, a
primal-dual moat-growing phase with genuine simultaneous-event ties (a new
determinism surface next to `pcst_is_deterministic_on_tied_cost_topology` at
`pcst.rs:747`), and a tree-to-route extraction contract that #527 itself lists as
undecided. It also does not address the root cause on its own, per above.

**Land the detour corridor (C2) as a bounded fix.** Rejected for the sign-off and
gate reasons above, not on technical grounds. It is the likely right answer and
belongs in P3 under #654 gate 2.

**Tune lambda down.** Measured and insufficient: 0.5 to 0.25 saves about 5 nodes
at no recall cost, and lambda 0 still floors at 37. Worth doing as an ADR-007
amendment, independent of the corridor shape. It cannot be the fix.

**Special-case high-degree nodes out of the corridor.** Rejected. It treats the
hub cascade (a symptom) rather than the absent sink conditioning (the cause), and
adds a second tunable that would need its own ADR.

**Close #527 by re-affirming the heuristic as intentional.** Rejected today. P2's
escape clause permits this, but it is only honest once the ball has been measured
against an alternative. Re-affirming a shape nobody chose is not the same as
documenting a tradeoff somebody made.

## Recommendation

1. Keep #527 open. The P0 doc correction discharged the naming defect only.
2. Take #654 to sign-off with the amendments below, then run P2.
3. Whichever arm wins, the P4 ADR must record the *predicate*, not just lambda.
   ADR-007 documents a penalty parameter for an algorithm that was never
   implemented; the successor should document the selection rule that ships.

### What #654 covers

The corridor-versus-ball diagnosis (D2), `detour` as the prize argument with the
reverse Dijkstra, C2 as a first-class arm rather than a straw man (D3), the
`(detour, NodeId)` tie-break discipline (D4), preservation of the route-first
knapsack ordering pinned by the #317 regression test at `pcst.rs:657` (D5), and
the unchanged `pcst_path` boundary, RBAC placement, SEC P0 equivalence and BFS
fallback triggers (D6). This analysis reproduces every load-bearing claim of D2
and D3 against current master and agrees with them.

### What #654 does not yet cover

1. **The RFC number is already taken.** `docs/rfcs/RFC-025-sidecar-version-contract.md`
   is on master (merged in #702), and #654 adds a second `RFC-025-*`. Next free
   number is RFC-028 (RFC-027 landed in #795). Purely clerical, but two RFC-025s
   is exactly the kind of drift #527 exists to punish.
2. **Gate 2 contradicts the RFC's own corpus note.** Acceptance Criteria states
   that one corpus is "enough to disqualify an arm, not enough to generalise a
   win", then gate 2 ships C2 into production on the travsr self-graph alone.
   Either gate 2 requires the k8s pair set (which must be generated first, per
   the same section), or the RFC should say explicitly that a single-corpus win
   is accepted for C2 and why.
3. **Lambda is reused with a different meaning, silently.** Under C1 lambda is a
   radius multiplier on a source ball; under C2 it is a detour tolerance. The two
   are not comparable, so ADR-007's lambda = 0.5 does not carry over and
   `PCST_LAMBDA` at `pcst.rs:37` must be re-derived, not inherited. The env knob
   `TRAVSR_PCST_LAMBDA` (`pcst.rs:49`) changes meaning at the same moment. The
   RFC says "lambda swept" for C2 but does not state that the constant and
   ADR-007 are invalidated by the substitution.
4. **No rollout phase for a C2 win, and a float-boundary determinism gap.** Both
   raised in the #654 review and both correct. Gate 2 says C2 ships, but Rollout
   defines P3 only for GW. And `detour` is a three-term float sum, so on-route
   nodes will not land at exactly `0.0` and summation order can jitter the
   boundary; the D4 contract needs an epsilon on the comparison plus a test that
   exercises near-zero detour ties, not only the `(detour, NodeId)` sort.
5. **Node count is reported but not gated.** The complaint is 10x too many nodes,
   yet gates test recall, path share and query-independence. An arm could improve
   path share while barely denting the padding. Also raised in review.

## Test plan (for P3, whichever arm wins)

No tests change here; this document is inert. When the predicate is replaced:

- Extend `pcst_respects_rbac_filter` (`pcst.rs:693`) to the reverse pass. The
  reverse Dijkstra must run on the same `expand_local_subgraph` output, which is
  already filtered at every frontier (`filter.allow` at `pcst.rs:296, 317, 335,
  358, 376`) and reaches petgraph only through `build_graph` (`pcst.rs:396`). A
  reverse pass built from any other edge source would be an RBAC leak.
- New: a node adjacent to the source but with no path to the sink must be absent
  from the result (`d_t = infinity` pruning). This fails against current code and
  is the sharpest single regression test for the ball.
- New: a node adjacent to the source and pointing away from the sink must rank
  below an on-route node.
- New: determinism on near-zero detour ties, alongside
  `pcst_is_deterministic_on_tied_cost_topology` (`pcst.rs:747`).
- Unchanged and must still pass: `pcst_high_fanout_source_does_not_starve_sink`
  (`pcst.rs:657`, the #317 route-first contract) and
  `pcst_falls_back_to_bfs_when_no_path` (`pcst.rs:645`).

## Risks

- **Deferring leaves the defect shipped.** Users keep getting a ball. Mitigated
  only by #654 moving; if it stalls, the padding problem needs its own issue so
  it does not get closed alongside #527, as #654's Rollout section already notes.
- **C2 under-returns.** Fewer nodes is the goal, but `get_context`-style
  consumers may be relying on the padding. P2 must confirm recall holds before
  P3, not after.
- **Reverse-pass RBAC.** See test plan. Structurally safe today because the edge
  list is pre-filtered, and that property must be asserted rather than assumed.
- **P2 is currently unreproducible.** Unresolved Q2 in #654 blocks the gate that
  blocks everything else. It needs an owner before P2 starts, not during.
