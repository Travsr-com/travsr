# RFC-010: 0-1 Knapsack Token-Budget Enforcer

> **Superseded in part (#457):** Kùzu was dropped as a storage backend; SQLite+WAL is the only backend. The `KuzuStore` stub requirements below no longer apply and are kept for historical context. See ADR-018.

| Field | Value |
|---|---|
| **Status** | Draft |
| **Author** | Tech Lead |
| **Date** | 2026-05-29 |
| **Issue** | #TBD |
| **Phase** | 3 (Sprint 11) |
| **Crate(s) affected** | `travsr-retrieval`, `travsr-mcp`, `travsr-store` |
| **Depends on** | ADR-003 (PPR policy), RFC-003 (multi-language indexer / `Node` struct), RFC-004 (MCP tool JSON schema) |

---

## Summary

Introduce a 0-1 knapsack token-budget enforcer in `travsr-retrieval` and wire it into a new `get_context` MCP tool that exposes the full PPR → knapsack pipeline. This closes ADR-003's explicit open question on knapsack integration and ships the `get_context` tool that makes the "80% fewer tokens" thesis concrete and measurable.

---

## Motivation

**ADR-003 deferred this work explicitly:**

> *Token budget integration: How does PPR ranking compose with the 0-1 knapsack token budget (Phase 3)? The top-k nodes by PPR score are the knapsack items; budget is the knapsack capacity. The knapsack solver is not yet scoped.*

**`get_context` is the missing keystone tool.** Every other MCP tool (`get_dependencies`, `get_callers`, `get_blast_radius`) answers a single structural question. `get_context` is the general-purpose query interface — "give me everything an LLM needs to understand this symbol within N tokens." Without it, an AI agent must chain multiple tool calls and assemble context manually, defeating the purpose.

**The existing token-limit approach is greedy and suboptimal.** Both `bfs()` and `pcst_path()` apply a token budget by stopping early once accumulated cost exceeds the limit. This works as a safety valve but is not a selection algorithm: it may include a large-cost low-value node and exclude several small-cost high-value nodes that would have fit. The 0-1 knapsack solves this optimally.

**Token cost is duplicated in three places.** The formula `node.vname.signature.len() + node.kind.len()` appears verbatim in `travsr-retrieval/src/lib.rs` (BFS) and `travsr-retrieval/src/pcst.rs` (PCST). A centralized `token_cost()` function fixes this.

---

## Detailed Design

### 1. `token_cost` — centralized cost model

```rust
// crates/travsr-retrieval/src/knapsack.rs

/// Character-to-token divisor, calibrated against cl100k_base on a 1k-node
/// sample (QA-301). A single-line change here updates all callers.
pub const TOKEN_CHARS_PER_TOKEN: usize = 4;

/// Approximates the LLM token cost of rendering a node in context.
///
/// Counts **UTF-8 bytes** (via `.len()`), consistent with the existing
/// BFS and PCST inline formulas this replaces. For non-ASCII identifiers
/// (e.g. CJK) this over-counts by ~3× relative to Unicode scalar values —
/// acceptable for an approximation; QA-301 calibration validates the total
/// error is within ±20%.
///
/// The formula accounts for three components an LLM sees in the output line:
///   `<signature> (<kind>) — <path>`
/// Path is included because it is always rendered and is the dominant cost
/// for deeply-nested files (e.g. `crates/travsr-retrieval/src/knapsack.rs`).
/// Returns at least 1 to prevent zero-weight items from trivially filling
/// any budget.
pub fn token_cost(node: &Node) -> usize {
    let bytes = node.vname.signature.len()
        + node.kind.len()
        + node.vname.path.len();
    (bytes / TOKEN_CHARS_PER_TOKEN).max(1)
}
```

**Why add `path.len()`:** The current formula (`signature + kind`) undercounts for nodes in deep paths. An LLM receiving `fn:charge (function) — crates/travsr-retrieval/src/knapsack.rs` needs ~15 tokens for the path alone. Omitting it causes the budget to be silently exceeded when nodes are formatted for output.

**Migration for BFS and PCST:** Both existing call sites are replaced with `knapsack::token_cost(node)`. No behaviour change — the divisor `/4` on the new formula produces values within ±10% of the old formula for typical Travsr graph nodes (median signature ≈ 18 bytes, median path ≈ 32 bytes → old: ~5 units, new: ~12 at `/4`). The relative ordering is identical; BFS and PCST budget behaviour is preserved.

**Calibration task QA-301:** Before Sprint 11 ships, run `tokenize_sample` on 1k random graph nodes. If the mean error between `token_cost(node)` and `cl100k_base(render(node))` exceeds ±20%, adjust `TOKEN_CHARS_PER_TOKEN`. No API changes required.

---

### 2. `knapsack()` — 0-1 DP with greedy fallback

```rust
// crates/travsr-retrieval/src/knapsack.rs

/// Maximum DP table cells (n × W) before switching to greedy fallback.
/// 500k cells × 4 bytes = 2 MB. Acts as a TIME limit: O(n×W) operations.
/// Typical call: n=200 nodes, budget=2,000 tokens → 400k cells, within limit.
pub const DP_CELL_LIMIT: usize = 500_000;

/// Score multiplier: converts f32 PPR scores in [0, 1] to u32 DP values.
/// 1_000_000 ensures scores as small as 0.000001 map to a non-zero u32,
/// preserving full ordering. With n ≤ 500 items, max total DP value ≤
/// 500 × 1_000_000 = 500_000_000, well within u32::MAX (4_294_967_295).
pub const SCORE_SCALE: u32 = 1_000_000;

/// Maximum allowed token budget. Validated both at the MCP layer and
/// inside knapsack() as defense-in-depth.
pub const MAX_CONTEXT_BUDGET: usize = 32_000;

/// Selects the highest-value subset of `items` that fits within `token_budget`.
///
/// Items are `(Node, value: f32)` — PPR scores from `ppr()`, sorted
/// descending. Returns selected nodes ordered by descending value.
///
/// **Algorithm selection:**
/// - 0-1 DP (full 2-D table) when `n × token_budget ≤ DP_CELL_LIMIT` (optimal).
/// - Greedy (sort by value/cost ratio) otherwise (near-optimal).
///
/// Greedy is near-optimal in practice because PPR output nodes tend toward
/// similar token costs (most signatures are 10–50 bytes). A `tracing::warn!`
/// is emitted on greedy fallback so operators can tune `DP_CELL_LIMIT`
/// or reduce PPR candidate count if needed.
///
/// # Budget enforcement
/// - The DP loop uses `w >= item.cost` (inclusive — an item whose cost equals
///   the remaining budget exactly is a valid selection).
/// - `token_budget > MAX_CONTEXT_BUDGET`: immediately returns `vec![]` with a
///   `tracing::error!`. This is defense-in-depth; the MCP layer also rejects
///   oversized budgets before reaching this function.
/// - `token_budget + 1` uses `checked_add`; on overflow falls through to greedy.
///
/// # Edge cases
/// - Empty `items` → returns `vec![]`.
/// - `token_budget == 0` → returns `vec![]`.
/// - All items fit within budget → returns all items sorted by descending value.
pub fn knapsack(items: Vec<(Node, f32)>, token_budget: usize) -> Vec<Node>
```

**DP algorithm (full 2-D table, O(n×W) space, exact backtracking):**

```
n        = items.len()
costs[i] = token_cost(&items[i].0)
values[i] = (items[i].1 * SCORE_SCALE as f32).round() as u32).max(1)

dp: Vec<Vec<u32>> of shape (n+1) × (token_budget+1), initialised to 0

for i in 1..=n:
    for w in 0..=token_budget:
        dp[i][w] = dp[i-1][w]                                 // exclude item i
        if w >= costs[i-1]:
            let include = dp[i-1][w - costs[i-1]] + values[i-1]
            if include > dp[i][w]:
                dp[i][w] = include                             // include item i

Backtrack to recover selected items:
    w = token_budget
    for i from n down to 1:
        if dp[i][w] != dp[i-1][w]:   // item i was included
            select items[i-1]
            w -= costs[i-1]
```

Space is O(n × W), bounded in practice by `DP_CELL_LIMIT = 500_000` cells (2 MB).
The time guard `n × token_budget > DP_CELL_LIMIT` is checked before allocating the
table; if it fires, the greedy fallback runs instead.

**Score normalization:** PPR scores are f32 in [0, 1]. Multiplied by `SCORE_SCALE =
1_000_000` and rounded (not truncated) before casting to u32. `.max(1)` ensures no
item has a zero DP value — zero-value items would pass the backtracking inclusion
test for free and silently consume budget for nodes the algorithm considered
worthless. With `SCORE_SCALE = 1_000_000`, scores as small as 0.0000005 round to
1u32, fully preserving ordering across the entire [0, 1] domain.

**Greedy fallback:**

```
sort items by (value / token_cost) descending, breaking ties by descending value
accumulated_cost = 0
for each item in sorted order:
    if accumulated_cost + item.cost <= token_budget:
        select item
        accumulated_cost += item.cost
```

The `accumulated_cost + item.cost <= token_budget` check is per-item (not prefix-sum
after insertion), ensuring the budget invariant `total_cost ≤ token_budget` holds
for every selected item.

---

### 3. `get_context` MCP tool

**Full pipeline:**

```
get_context(query, token_budget, repo?)
  1. validate_mcp_arg(query)                       — SEC-002 input sanitization
  2. validate token_budget ≤ MAX_CONTEXT_BUDGET    — reject with structured error if exceeded
  3. store.search_nodes_by_name(query)
       .filter(|n| filter.allow(n.id, n.id, Some(&n.vname.corpus)))
                                                   → seed NodeIds (up to 5 seeds, corpus-filtered)
  4. ppr(store, &seeds, k = context_candidates())  → Vec<(NodeId, f32)>
  5. store.get_nodes(&node_ids)                    → Vec<Node>   (see §4 for batch contract)
  6. keyed join: HashMap<NodeId, Node> + ppr scores → Vec<(Node, f32)>
  7. knapsack(items, token_budget)                 → Vec<Node>   (optimal subset)
  8. format_context_response(&nodes)              → String   (node lines only, no footer)
  9. sanitize_for_mcp(body)                        → String   (truncation + escaping + bidi strip)
 10. append footer "[N nodes, ~T tokens]\n"        → final String
```

**Step 3 — RBAC seed filtering (SEC P0):**

`search_nodes_by_name` returns nodes from all corpora. Step 3 must filter seeds
through the session's `EdgeFilter` before they enter PPR — identical to the
`get_execution_path_authed` pattern (`tools.rs` line 597–614). Seeds from
unauthorized corpora must be silently dropped. If all seeds are filtered out, the
pipeline returns the "no symbols found" message without revealing that nodes exist
in the denied corpus (SEC P0: identical response for "not found" vs "access denied").

PPR (`ppr_inner`) does not currently accept an `EdgeFilter`. Until it does
(prerequisite task PERF-012), the workaround is to post-filter PPR output: after
step 4, drop any `(NodeId, f32)` pair whose resolved `Node.vname.corpus` is not in
the allowed set before passing to step 6. This is weaker than filtering during
traversal (PPR may waste cycles on disallowed subgraphs) but maintains SEC P0.
PERF-012 tracks adding `EdgeFilter` support to `ppr_inner` for Sprint 12.

**Step 6 — keyed join (not positional zip):**

`get_nodes` returns nodes in unspecified order and silently omits missing IDs. A
positional zip would produce misaligned `(Node, score)` pairs and corrupt the
knapsack input. The join must be keyed by `NodeId`:

```rust
let node_map: HashMap<NodeId, Node> =
    nodes.into_iter().map(|n| (n.id, n)).collect();
let items: Vec<(Node, f32)> = ppr_scores
    .into_iter()
    .filter_map(|(id, score)| node_map.remove(&id).map(|n| (n, score)))
    .collect();
```

**Step 9/10 — sanitizer and footer ordering:**

The existing `sanitize_for_mcp` enforces a fixed `MAX_OUTPUT_BYTES = 4,096` cap.
For `get_context`, which may return up to `token_budget` tokens of content, 4,096
bytes is insufficient. The sanitizer call in step 9 must use a proportional cap:

```rust
let max_bytes = (token_budget * TOKEN_CHARS_PER_TOKEN * 2).min(1_024_000);
let body = sanitize_for_mcp_with_limit(&node_lines, max_bytes);
```

The footer (`[N nodes, ~T tokens]`) is appended in step 10, **after** sanitization
and truncation, so it is never cut off. This is the correctness signal that lets the
LLM verify it received a complete response.

The sanitizer must also strip Unicode bidi-override characters (known prompt-injection
vector — T8 in the threat model):

```rust
// In strip_control_chars — add alongside existing cp <= 0x1F check:
if (0x202A..=0x202E).contains(&cp) || (0x2066..=0x2069).contains(&cp) {
    return false;  // bidi overrides: RLO, LRO, PDF, LRI, RLI, FSI, PDI
}
```

**Function signatures:**

```rust
// crates/travsr-mcp/src/tools.rs

pub fn get_context(store: &SqliteStore, query: &str, token_budget: usize) -> String

pub(crate) fn get_context_authed(
    store: &SqliteStore,
    query: &str,
    token_budget: usize,
    filter: &dyn EdgeFilter,
) -> String
```

The `_authed` variant is the implementation; the public wrapper passes `&OpenFilter`.
This matches the pattern already used by `get_execution_path` / `get_execution_path_authed`.

**PPR candidate count:**

```rust
/// Number of PPR candidate nodes fed into the knapsack.
/// Overridable via TRAVSR_CONTEXT_CANDIDATES env var (same pattern as PPR hyperparams).
/// Rejects 0 (would produce an empty PPR result regardless of seeds).
pub fn context_candidates() -> usize {
    std::env::var("TRAVSR_CONTEXT_CANDIDATES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(200)
}
```

200 candidates is the default. The knapsack selects the optimal subset from these;
callers never see more than `token_budget / min_node_cost` nodes in the output.

**Tool parameters (RFC-004 schema extension — version bump to 1.1.0):**

| Parameter | Type | Required | Constraints | Default | Notes |
|---|---|---|---|---|---|
| `query` | `string` | yes | 1–200 chars | — | Symbol name or free-text query |
| `token_budget` | `integer` | yes | 100–32,000 | — | Hard ceiling on output token cost |
| `repo` | `string` | no | — | all repos | Restrict traversal to one corpus |

`MAX_CONTEXT_BUDGET = 32_000` — aligns with Claude's max context window.
The MCP server rejects `token_budget > MAX_CONTEXT_BUDGET` with a structured error.
`knapsack()` also guards against oversized budgets internally (defense-in-depth).

**Output format:**

```
fn:PaymentService.charge (function) — src/payment/service.ts [package: @acme/payments]
fn:validateCard (function) — src/payment/validate.ts [package: @acme/payments]
class:StripeClient (class) — src/integrations/stripe.ts [package: @acme/stripe]
fn:StripeClient.chargeCard (method) — src/integrations/stripe.ts [package: @acme/stripe]
fn:PaymentService.refund (function) — src/payment/service.ts [package: @acme/payments]

[5 nodes, ~310 tokens]
```

The footer `[N nodes, ~T tokens]` is always the last line. It is appended after
sanitization so it is never truncated. Empty result:

```
No symbols matching 'query' found in the graph.
```

Plain-text, no error code — consistent with `search_symbol` empty results.

---

### 4. New `Store::get_nodes` method

`ppr()` returns `Vec<(NodeId, f32)>` — node IDs, not `Node` objects. Step 5 of the
pipeline requires bulk-resolving IDs to `Node` structs. No such method exists today.

```rust
// crates/travsr-store/src/lib.rs  (Store trait)

/// Fetch a batch of nodes by ID. IDs not found in the store are silently
/// omitted from the result (not an error). Order of results is unspecified.
/// Callers must use a keyed join (not a positional zip) to associate results
/// with their input IDs.
fn get_nodes(&self, ids: &[NodeId]) -> Result<Vec<Node>, TravsrError>;
```

**`SqliteStore` implementation:** Chunks the `ids` slice into batches of ≤ 999
(SQLite `SQLITE_MAX_VARIABLE_NUMBER` default) and issues one
`SELECT … WHERE id IN (…)` per chunk, merging results. Callers passing > 999 IDs
must not be aware of this chunking — it is an implementation detail of
`SqliteStore`, not a contract of the `Store` trait.

`KuzuStore` must implement the same method before the Kùzu backend ships (a stub
returning `Err(TravsrError::NotImplemented)` is acceptable until then).

---

### 5. `travsr-retrieval` export additions

```rust
// crates/travsr-retrieval/src/lib.rs

pub mod knapsack;
pub use knapsack::{
    knapsack, token_cost,
    DP_CELL_LIMIT, SCORE_SCALE, TOKEN_CHARS_PER_TOKEN, MAX_CONTEXT_BUDGET,
};
```

BFS and PCST call sites are updated to use `knapsack::token_cost(node)` in place
of the inline formula. No functional change to BFS or PCST behaviour.

---

### 6. RFC-004 schema version bump

`RFC-004` governs the MCP tool JSON schema. Adding `get_context` requires bumping
the schema version from `"1.0.0"` to `"1.1.0"`. The version is a semver string
embedded in the `initialize` handshake response. Clients that pin `"1.0.0"` are
unaffected — `get_context` simply will not appear in their `tools/list` response
until they accept `"1.1.0"`.

---

## Alternatives Considered

### A. Greedy selection only (sort by value/cost ratio)

O(n log n), simple, near-optimal when per-item costs are uniform. Rejected as the
primary algorithm because degenerate inputs — a few large-cost nodes mixed with many
small-cost high-value nodes — produce poor results. Retained as a fallback for large
DP tables where the optimality gap is acceptable.

### B. Fractional knapsack (take fractions of nodes)

Optimal for continuous relaxation. Rejected: nodes are atomic. Including 40% of a
function definition is meaningless to an LLM.

### C. PPR top-k then truncate

Take the k highest-scoring nodes, stop when budget is full. Rejected: wastes budget
on large-cost nodes when many small-cost nodes would fit within the remaining
capacity. Example: a `class:PaymentService` node costs 80 tokens; 16 small
`fn:*` nodes cost 5 tokens each and have higher aggregate value. Top-k truncation
includes `class:PaymentService` and misses the 16 functions. Knapsack includes
all 16.

### D. Put knapsack inside `ppr()`

Rejected: `ppr()` returns `(NodeId, f32)` pairs with no `Node` objects. Mixing
retrieval and budget optimization inside the PPR function violates the
single-responsibility boundary and would require `ppr()` to depend on `travsr-store`
for node resolution, introducing a circular-style coupling.

### E. Rolling 1-D array (O(W) space)

A rolling 1-D DP array would reduce per-call memory from O(n×W) to O(W). Rejected:
backtracking to recover the selected item set is impossible after a single-pass
rolling update — the previous-row values are overwritten. Without backtracking,
the optimal *value* is known but not *which* items achieve it. Using the 2-D table
with `DP_CELL_LIMIT = 500_000` caps space at 2 MB, which is acceptable for the
local daemon and the SSE cloud tier. Revisit if profiling shows memory pressure.

### F. Per-call DP table allocation via an arena allocator

Would reduce allocation pressure for high-throughput cloud deployments. Deferred:
OCI Always Free tier runs a single-tenant daemon; the ≤ 2 MB allocation per call
is released immediately. Revisit if cloud tier (RFC-007) shows memory contention.

---

## Drawbacks

- **`get_nodes` is a new Store trait method.** `KuzuStore` must implement it before
  the Kùzu migration can complete. This is a forcing function, not a blocker.

- **DP table is O(n×W) per call.** At `n=200, budget=2,000`, the table is 1.6 MB.
  At `DP_CELL_LIMIT`, it is exactly 2 MB. Acceptable for the local daemon;
  the SSE cloud tier (RFC-007) should profile under concurrent load.

- **Greedy fallback breaks optimality guarantees.** When `n × budget > 500,000`,
  the result is near-optimal but not provably optimal. The threshold is conservative
  (200 nodes × 2,000 budget = 400k < 500k) so the fallback is not triggered in
  normal operation. The `tracing::warn!` makes it observable.

- **`token_cost` is a proxy, not exact.** The byte-based formula over-counts for
  non-ASCII identifiers. QA-301 validates the total error is within ±20%.

- **PPR does not yet accept an `EdgeFilter`.** Until PERF-012 lands, cross-corpus
  RBAC enforcement in `get_context` relies on post-filtering PPR output rather than
  filtering during traversal. This is correct but wastes PPR cycles on disallowed
  subgraphs in multi-corpus deployments.

---

## Unresolved Questions

1. **`context_candidates` default.** Is 200 PPR candidates the right default? A
   larger pool gives the knapsack more to choose from (better packing), but
   increases `ppr()` latency. Proposed: benchmark at k=50, 100, 200, 500 and pick
   the knee of the latency/quality curve before Sprint 11 ships.

2. **Output format: plain text vs. JSON.** All current tools return plain `String`.
   JSON would let IDE extensions display node metadata in structured UI. Proposed:
   plain text for v1; a `format: "json"` optional parameter in a follow-on RFC.

3. **Multi-seed behaviour.** When `query` matches multiple symbols (e.g. `"charge"`
   matches `fn:charge` in 4 files), PPR runs with all matches as seeds (capped at 5)
   returning a blended context. Is this always preferable to picking the single
   highest-ranking match? Proposed: blended seeds for v1; add `exact: true` flag
   in a follow-on if single-match mode is needed.

4. **`token_budget` required vs. defaulted.** Proposed: require it explicitly — no
   default — so callers must be intentional about their budget and avoid silent
   over-fetching in constrained contexts.

---

## Test Strategy

### Unit tests (`crates/travsr-retrieval/src/knapsack.rs`)

| Test | What it verifies |
|---|---|
| `knapsack_empty_items` | Returns `vec![]` |
| `knapsack_zero_budget` | Returns `vec![]` |
| `knapsack_all_items_fit` | Returns all items when total cost ≤ budget |
| `knapsack_optimal_vs_brute_force` | For n ≤ 15, DP result == brute-force optimal |
| `knapsack_item_at_exact_budget_is_selected` | Single item with `cost == budget` is selected (≤ bound, not <) |
| `knapsack_degenerate_one_big_vs_many_small` | Budget=100; one 90-token node (score 0.9) vs 20×5-token nodes (score 0.05 each). Knapsack must select the 20 small nodes (value 1.0 > 0.9) |
| `knapsack_greedy_fallback_triggered` | Force `n × budget > DP_CELL_LIMIT`; assert result non-empty and `total_cost ≤ budget` |
| `knapsack_oversized_budget_returns_empty` | `token_budget > MAX_CONTEXT_BUDGET` → `vec![]` + `tracing::error!` |
| `token_cost_min_one` | `token_cost` returns ≥ 1 for any node |
| `token_cost_formula` | Known ASCII signature + kind + path → expected byte-counted token cost |
| `token_cost_non_ascii_uses_byte_length` | CJK identifier: asserts byte-count (`.len()`), not char-count, to lock the documented unit |
| `score_normalization_low_ppr_preserved` | Score 0.0000005 rounds to 1u32 (not zero); score 1.0 maps to SCORE_SCALE |

### Property tests (`crates/travsr-retrieval/`)

```rust
// proptest: 1,000 cases. Run against both DP and greedy paths.
proptest! {
    #![proptest_config(ProptestConfig { cases: 1_000, ..Default::default() })]

    #[test]
    fn prop_knapsack_budget_invariant(
        budget in 0usize..=MAX_CONTEXT_BUDGET,
        items in arb_knapsack_items(0..200),
    ) {
        let result = knapsack(items, budget);
        let total: usize = result.iter().map(|n| token_cost(n)).sum();
        prop_assert!(total <= budget);
    }

    #[test]
    fn prop_greedy_fallback_budget_invariant(
        budget in 0usize..=MAX_CONTEXT_BUDGET,
        items in arb_knapsack_items(0..200),
    ) {
        // Force greedy by using a budget and item count that exceeds DP_CELL_LIMIT
        let result = knapsack_greedy(&items, budget);
        let total: usize = result.iter().map(|n| token_cost(n)).sum();
        prop_assert!(total <= budget);
    }
}
```

### Fuzz target (`fuzz/fuzz_targets/fuzz_knapsack.rs`)

Register in `fuzz/Cargo.toml`. Must not panic on any input:

```rust
#![no_main]
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| {
    if data.len() < 9 { return; }
    let budget = usize::from_le_bytes(data[..8].try_into().unwrap())
        % (MAX_CONTEXT_BUDGET + 2);  // includes invalid values above limit
    // parse remaining bytes as (cost: u64, value_bits: u32) item pairs
    let items = parse_fuzz_items(&data[8..]);
    let _ = travsr_retrieval::knapsack(items, budget);
});
```

### Integration tests (`crates/travsr-store/`)

| Test | What it verifies |
|---|---|
| `get_nodes_batch_over_999_does_not_panic` | Insert 1,001 nodes, fetch all by ID in one call; must not crash with "too many SQL variables" |
| `get_nodes_missing_ids_silently_omitted` | Request 5 IDs where 2 are absent; result has 3 nodes |

### Conformance tests (`crates/travsr-mcp/tests/conformance.rs`)

| Test | What it verifies |
|---|---|
| `mcp_get_context_returns_text_content` | Tool is registered; MCP call returns `TextContent` |
| `mcp_get_context_budget_is_respected` | Total `token_cost` of returned nodes ≤ `token_budget` |
| `mcp_get_context_keyed_join_no_misalignment` | PPR returns IDs where some are absent from store; assert correct node–score pairs |
| `mcp_get_context_empty_query_returns_not_found` | Unknown symbol → `"No symbols matching…"` |
| `mcp_get_context_empty_graph_returns_empty_envelope` | Empty repo (0 indexed files) → `<travsr-data></travsr-data>` without panic |
| `mcp_get_context_rejects_oversized_budget` | `token_budget = 100,000` → structured error |
| `mcp_get_context_accepts_budget_at_max` | `token_budget = 32,000` (MAX exactly) → accepted, returns result |
| `mcp_get_context_rejects_budget_at_max_plus_one` | `token_budget = 32,001` → structured error (boundary off-by-one) |
| `mcp_get_context_footer_never_truncated` | Response with many nodes still has `[N nodes, ~T tokens]` as the last line |
| `mcp_get_context_rbac_no_cross_corpus_leak` | Session scoped to corpus A; corpus B has `fn:foo`; `get_context("foo")` returns only corpus A nodes (SEC P0) |

### QA-301 — token cost calibration

Run `tokenize_sample` against 1k random nodes from `.travsr/graph.db`. Assert
`mean_error(token_cost(n), cl100k_base(render(n))) ≤ 0.20` (20% tolerance).
If the assertion fails, adjust `TOKEN_CHARS_PER_TOKEN` and re-run. Gate Sprint 11
ship on green.

---

## Acceptance Checklist

- [ ] `cargo test -p travsr-retrieval` passes including all new knapsack unit tests.
- [ ] `cargo test -p travsr-mcp` passes including all new conformance tests.
- [ ] `cargo test -p travsr-store` passes including `get_nodes_batch_over_999` test.
- [ ] `proptest` budget invariant passes 1,000 cases on both DP and greedy paths.
- [ ] `cargo clippy --workspace -- -D warnings` clean.
- [ ] `cargo fmt --check` clean.
- [ ] `token_budget > MAX_CONTEXT_BUDGET` returns a structured MCP error (not a panic).
- [ ] `knapsack()` internally guards against oversized budget (defense-in-depth).
- [ ] `DP table allocation uses `checked_add` — no integer overflow on `token_budget + 1`.
- [ ] `get_context` appears in `tools/list` response with correct JSON schema.
- [ ] QA-301 calibration complete and `TOKEN_CHARS_PER_TOKEN` validated.
- [ ] `Store::get_nodes` implemented on `SqliteStore` with ≤ 999 ID chunking.
- [ ] `KuzuStore::get_nodes` stub added (returns `Err(TravsrError::NotImplemented)`).
- [ ] RFC-004 schema version bumped to `"1.1.0"` in `travsr-mcp/src/protocol.rs`.
- [ ] `sanitize_for_mcp` bidi-override strip added (`0x202A–0x202E`, `0x2066–0x2069`).
- [ ] Footer appended after sanitization — never truncated.
- [ ] Seeds filtered through `EdgeFilter` before PPR (SEC P0 — step 3).
- [ ] PPR output post-filtered by allowed corpora until PERF-012 lands.
- [ ] `fuzz_knapsack` target added to `fuzz/Cargo.toml` and runs without panic in CI.
- [ ] ADR-003 open question §Token budget integration marked resolved, references RFC-010.
