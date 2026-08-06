# RFC-024: Graph Edge Direction Fidelity

**Status:** Draft — proposed, awaiting review
**Author:** SWE (drafted from the #564 investigation)
**Date:** 2026-08-05
**Crates affected:** `travsr-mcp` (`query.rs`), `travsr-cli` (`graph.rs`)
**Fixes:** #564
**Related:** #517 (graph callers UX), #529 (triage session that surfaced the defect), DEBT(travsr-75) (edge provenance, adjacent but out of scope)

---

## Summary

`travsr graph <symbol> --direction both` emits every incoming edge with `from`/`to`
reversed, in every output format (tree, JSON, DOT). The defect makes the one
invocation this repo's own agent workflow mandates before every edit — `travsr graph
<symbol> --direction both` — return structurally inverted data, and it has already
produced a wrong root-cause hypothesis in a real triage session (#529). This RFC
formalises the invariant that **edge orientation in every output is the stored edge
orientation**, specifies the data-model change (`TreeStep` gains a direction), the
renderer change (in-edges become visually distinct), the dedup change (bidirectional
pairs must survive), and the regression fixtures that pin all of it.

The MCP tool `get_graph_json` was verified as part of this investigation and is
**not** affected — it constructs `source`/`target` per edge branch and is correct in
all direction modes. The defect is confined to `travsr_mcp::query::graph_query`,
which serves the CLI `travsr graph` command (cold path and daemon path alike).

---

## Motivation

`fn:SandboxedSpawn.output` has 3 genuine outgoing calls and 31 incoming ones. In
`--direction both` mode all 34 edges are emitted as `from = seed`, so a reader
cannot separate the 3 true claims from the 31 false ones. A cleaner minimal case:
`i64_to_node_id` is a 2-line cast with zero calls in its body, and `--direction
both` claims it calls 27 `SqliteStore` methods.

This is not cosmetic. The follow-up comment on #529 reported those in-edges as
outgoing and concluded a second, distinct root cause existed ("caller
misattribution, possibly a span/line-range bug in Phase B"). No such bug exists;
the graph was right and the report was inverted. The hypothesis was only discarded
because a human re-checked direction by hand. A code-intelligence tool whose core
output cannot be trusted on direction actively costs its users triage cycles.

---

## Root cause

Three code-level facts, all in `crates/travsr-mcp/src/query.rs` unless noted:

1. **Orientation is derived from the query mode, not from the edge.**
   `next_edges()` merges outgoing edges (`iter_edges_from`, neighbour = `e.dst`)
   and incoming edges (`iter_edges_to`, neighbour = `e.src`) into one
   `Vec<(kind, next_id, expand)>` that carries **no record of which branch an
   edge came from**. `graph_query()` then orients every edge with a single match
   on the global direction argument:

   ```rust
   let (src, dst) = match args.direction {
       QueryDirection::Callers => (next_id, current_id),
       _ => (current_id, next_id),   // Deps AND Both
   };
   ```

   In `Callers` mode all edges are incoming, so the flip is right. In `Deps` mode
   all are outgoing, so the pass-through is right. In `Both` mode the merged list
   contains both kinds and the pass-through arm silently reverses every in-edge.

2. **The dedup key discards direction.** `next_edges()` collapses duplicates with
   `seen.insert((kind.clone(), *id))`. When A and B genuinely reference each other
   (mutual recursion, callback registration), the out-edge A→B and the in-edge
   B→A from A's expansion share the key `("ref/call", B)` and one of them is
   dropped before orientation is even decided. Fixing fact 1 without fixing the
   key would still lose one direction of every bidirectional pair.

3. **The tree data model cannot express direction.** `TreeStep { parent,
   edge_kind, child }` records only the BFS spanning-tree shape, and the renderer
   (`crates/travsr-cli/src/graph.rs`, `print_tree_level`) hardcodes the glyph:

   ```rust
   println!("{prefix}{connector}{edge_kind} → {} ({})", child.label, child.kind);
   ```

   Even with facts 1–2 fixed, the tree format would keep drawing every edge as if
   it pointed away from the seed. This also means `--direction callers` tree
   output today reads "seed —ref/call→ caller", which is tolerated only because
   the mode name supplies the context.

**Verified unaffected:**

- `get_graph_json` / `get_graph_json_raw` (`crates/travsr-mcp/src/tools.rs`):
  every `edges_out.push` site sets `source`/`target` from the correct endpoint of
  the specific branch being walked (deps and callers, file mode and symbol mode).
  VS Code graph panel and MCP agent consumers receive correct orientation.
- `graph_all_payload` (`--all` mode): edges come verbatim from `store.all_edges()`.
- `--direction deps` and `--direction callers` JSON: single-kind lists, correctly
  oriented by the existing match.

---

## Constraints being formalised

Each constraint has a rule, a rationale, a failure mode if violated, and a test
identifier. QA owns the tests; the fix PR must land them together with the code.

### C-01 — Edge orientation is store truth

**Rule:** `EdgeEntry.src`/`EdgeEntry.dst` (and therefore JSON `edges[]` and DOT
`src -> dst`) MUST equal the stored edge's `src`/`dst` for every edge, in every
`--direction` mode.

**Mechanism:** `next_edges()` returns a per-edge direction:

```rust
enum EdgeDir { Outgoing, Incoming }
// next_edges → Vec<(String, NodeId, bool, EdgeDir)>
```

`graph_query()` orients from `EdgeDir`, and the `match args.direction` block is
deleted:

```rust
let (src, dst) = match dir {
    EdgeDir::Outgoing => (current_id, next_id),
    EdgeDir::Incoming => (next_id, current_id),
};
```

The file-node caller splice (#517 DD-2) yields `Incoming` edges whose `src` is the
actual calling definition — the splice already has the true endpoints and needs no
special case beyond tagging.

**Failure mode:** exactly #564 — `--direction both` asserts that a leaf function
calls its 27 callers, and agent workflows built on `--direction both` reason from
inverted structure.

**Test:** `T-01` — fixture with seed S, out-edge S→A, in-edges B→S and C→S.
For each of `deps`, `callers`, `both`: every emitted `(src, dst)` pair matches the
fixture's stored orientation. The `both` edge set equals the union of the `deps`
and `callers` edge sets.

### C-02 — Bidirectional pairs survive dedup

**Rule:** the `next_edges()` dedup key MUST include direction:
`(kind, next_id, dir)`. Two stored edges A→B and B→A are two output edges.

**Rationale:** collapsing them presents mutual recursion as one-way calling, a
direction lie by omission that C-01 alone does not cure.

**Failure mode:** a mutual pair renders as a single arbitrary-direction edge; which
one survives depends on store iteration order, so output is also nondeterministic.

**Test:** `T-02` — fixture with M→S and S→M (`ref/call` both ways). `--direction
both --format json` emits both edges with opposite orientations; the tree renders M
twice (once under each direction) or once per the spanning-tree rule with the edge
list still carrying both.

### C-03 — `TreeStep` carries direction, additively

**Rule:** `TreeStep` gains a direction field with a serde default, so old payloads
(and old daemons talking to new CLIs, and vice versa) keep deserialising:

```rust
pub struct TreeStep {
    pub parent: u64,
    pub edge_kind: String,
    pub child: u64,
    /// True when the stored edge points child → parent (an in-edge relative to
    /// the traversal). Absent in payloads from older daemons → false, which
    /// reproduces the old rendering rather than failing.
    #[serde(default)]
    pub incoming: bool,
}
```

**Rationale:** the CLI cold path and daemon path both serialise `GraphPayload`
across a version boundary (`daemon_client::try_query`); an additive field with a
default is the only change that keeps mixed-version CLI/daemon pairs working. A
`from`/`to` re-derivation in the renderer was considered and rejected: the tree is
a spanning tree over `nodes`, and reconstructing direction from `edges` in the
renderer duplicates the traversal's knowledge behind a lossy join.

**Failure mode:** renderer has no data to draw direction; or a breaking schema
change strands mixed-version installs.

**Test:** `T-03` — serde round-trip of a pre-change `TreeStep` JSON (no `incoming`
key) deserialises with `incoming == false`; a post-change payload round-trips
losslessly.

### C-04 — The tree renderer distinguishes in-edges from out-edges

**Rule:** `print_tree_level` renders out-edges as today (`ref/call → X`) and
in-edges with a reversed glyph (`ref/call ← X`), in **all** direction modes.

**Rationale:** the glyph must carry the truth on its own because `both` mode has
no single contextual direction. Applying the same rendering in `callers` mode makes
that mode honest too (today it draws `→` for edges that point at the seed);
`deps` mode output is unchanged by construction.

**Failure mode:** data is correct but unreadable — the reader is back to guessing
from the mode name.

**Test:** `T-04` — tree-format snapshot over the T-01 fixture: `S ─ref/call→ A`,
`S ─ref/call← B`, `S ─ref/call← C` (glyph assertions, not full-line snapshots, so
label changes do not churn the test).

### C-05 — DOT output orientation follows C-01

**Rule:** DOT emission uses `EdgeEntry` verbatim; arrows in rendered graphs point
in the stored direction. No renderer-side flipping.

**Test:** `T-05` — DOT output over the T-01 fixture contains `S -> A`, `B -> S`,
`C -> S` (id-level containment assertions).

### C-06 — MCP `get_graph_json` parity is pinned

**Rule:** the already-correct MCP orientation is protected by a regression test so
the two implementations cannot drift apart again.

**Rationale:** the CLI and MCP traversals are separate code paths today; #564
happened in one and not the other. Until they are unified (out of scope here), a
shared fixture asserts identical orientation semantics on both.

**Test:** `T-06` — `get_graph_json` over the T-01 fixture: every `edges[]` entry's
`source`/`target` matches stored orientation in `deps`, `callers`, and `both`.

---

## What deliberately does not change

1. **Edge provenance** (`"tree-sitter"` hardcoded for BFS-traversed edges) is
   DEBT(travsr-75) and stays out of scope; this RFC neither fixes nor worsens it.
2. **`--all` mode and overview mode** are already orientation-correct and untouched.
3. **Traversal semantics** (which nodes are discovered, depth, noise filtering,
   #517 DD-1/DD-2 rules, token budget) are unchanged — this RFC only corrects how
   discovered edges are *reported*.
4. **No unification of the CLI and MCP traversals.** Worth doing someday; doing it
   here would couple a correctness fix to a refactor with its own risk budget.

---

## Compatibility

1. **JSON consumers of `travsr graph --format json`:** `edges[].src/dst` semantics
   are corrected, which is the bug fix itself. The only consumers that could
   "break" are ones compensating for the reversed orientation; none exist in this
   repo (the VS Code panel consumes `get_graph_json`, which was never wrong), and
   external consumers were receiving data documented as oriented, so the fix
   restores the documented contract.
2. **`TreeStep.incoming`** is additive with a serde default (C-03): old CLI + new
   daemon renders old glyphs for in-edges (no worse than today); new CLI + old
   daemon likewise. New CLI + new daemon renders correctly.
3. **Tree output text changes** in `both` and `callers` modes (new `←` glyph).
   Scripts scraping tree output (discouraged; JSON exists) may need adjustment;
   called out in the CHANGELOG entry.

---

## Test plan

Fixture: an in-memory store (same helper as `seeded_store()` in `query.rs` tests)
with seed S, A (S→A `ref/call`), B and C (B→S, C→S `ref/call`), M (M→S and S→M).
Tests T-01 through T-06 as specified per constraint, plus:

1. `T-07` — the #564 minimal repro shape: a leaf node with zero out-edges and
   N in-edges reports zero `src == leaf` edges in `both` mode.
2. `T-08` — existing #517 tests (`graph_query_finds_callers_with_tree_steps` and
   friends) keep passing unmodified, proving `callers`-mode data semantics are
   untouched.

Manual verification: re-run the two repro commands from #564 against the fixed
binary on this repo's own graph and confirm the 3-out/31-in split for
`SandboxedSpawn.output` and the 0-out/27-in split for `i64_to_node_id`.

---

## Rollout

Single PR implementing C-01 through C-06 with tests T-01 through T-08, a CHANGELOG
entry under Unreleased noting both the correction and the tree-glyph change, and
closure of #564. No config, no migration, no version gate: the wire change is
additive and the behavior change is the fix itself.

---

## Open questions

1. Should `both`-mode tree output group children into `deps:` / `callers:`
   sections instead of (or in addition to) the `←`/`→` glyphs? The glyphs are
   strictly more information-dense and section grouping can be layered on later
   without another data-model change; this RFC ships glyphs only.
2. Whether #517's scope included direction correctness (the issue asks). From the
   #517 test inventory the answer appears to be no — its tests assert discovery
   and ordering, not orientation — so this is a long-standing defect surfaced by
   `both` mode usage, not a regression. T-08 preserves #517's guarantees either way.
