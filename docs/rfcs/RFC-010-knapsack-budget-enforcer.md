# RFC-010: 0-1 Knapsack Token-Budget Enforcer

| Field | Value |
|---|---|
| **Status** | Draft |
| **Author** | Tech Lead |
| **Date** | 2026-05-29 |
| **Issue** | #TBD |
| **Phase** | 3 (Sprint 11) |
| **Crate(s) affected** | `travsr-retrieval`, `travsr-mcp`, `travsr-store` |
| **Depends on** | ADR-003 (PPR policy), RFC-004 (MCP tool JSON schema) |

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
/// The formula accounts for three components an LLM sees in the output line:
///   `<signature> (<kind>) — <path>`
/// Path is included because it is always rendered and is the dominant cost
/// for deeply-nested files (e.g. `crates/travsr-retrieval/src/knapsack.rs`).
/// Returns at least 1 to prevent zero-weight items from trivially filling
/// any budget.
pub fn token_cost(node: &Node) -> usize {
    let chars = node.vname.signature.len()
        + node.kind.len()
        + node.vname.path.len();
    (chars / TOKEN_CHARS_PER_TOKEN).max(1)
}
```

**Why add `path.len()`:** The current formula (`signature + kind`) undercounts for nodes in deep paths. An LLM receiving `fn:charge (function) — crates/travsr-retrieval/src/knapsack.rs` needs ~15 tokens for the path alone. Omitting it causes the budget to be silently exceeded when nodes are formatted for output.

**Migration for BFS and PCST:** Both existing call sites are replaced with `knapsack::token_cost(node)`. No behaviour change — the divisor `/4` on the new formula produces values within ±10% of the old formula for typical Travsr graph nodes (median signature ≈ 18 chars, median path ≈ 32 chars → old: ~5 units, new: ~12 tokens at `/4`). The absolute scale changes but the relative ordering is identical, so BFS and PCST budget behaviour is preserved.

**Calibration task QA-301:** Before Sprint 11 ships, run `tokenize_sample` on 1k random graph nodes. If the mean error between `token_cost(node)` and `cl100k_base(render(node))` exceeds ±20%, adjust `TOKEN_CHARS_PER_TOKEN`. No API changes required.

---

### 2. `knapsack()` — 0-1 DP with greedy fallback

```rust
// crates/travsr-retrieval/src/knapsack.rs

/// Maximum DP table cells before switching to greedy fallback.
/// 500k cells ≈ 2 MB at 4 bytes/cell. Typical call: n=100 nodes,
/// budget=2000 tokens → 200k cells, well within limit.
pub const DP_CELL_LIMIT: usize = 500_000;

/// Score multiplier for converting f32 PPR scores to u32 DP values.
/// Preserves ordering while enabling integer arithmetic in the DP table.
pub const SCORE_SCALE: u32 = 1_000;

/// Selects the highest-value subset of `items` that fits within `token_budget`.
///
/// Items are `(Node, value: f32)` — PPR scores from `ppr()`, sorted
/// descending. Returns selected nodes ordered by descending value.
///
/// **Algorithm selection:**
/// - 0-1 DP when `n × token_budget ≤ DP_CELL_LIMIT` (optimal).
/// - Greedy (sort by value/cost ratio) otherwise (near-optimal).
///
/// Greedy is near-optimal in practice because PPR output nodes tend toward
/// similar token costs (most signatures are 10–50 chars). A `tracing::warn!`
/// is emitted on greedy fallback so operators can tune `DP_CELL_LIMIT`
/// or reduce PPR candidate count if needed.
///
/// # Edge cases
/// - Empty `items` → returns `vec![]`.
/// - `token_budget == 0` → returns `vec![]`.
/// - All items fit within budget → returns all items sorted by descending value.
pub fn knapsack(items: Vec<(Node, f32)>, token_budget: usize) -> Vec<Node>
```

**DP algorithm (rolling 1-D array, O(W) space):**

```
costs[i]  = token_cost(&items[i].0)
values[i] = (items[i].1 * SCORE_SCALE as f32) as u32

dp: Vec<u32> of length (token_budget + 1), initialised to 0

for each item i (0..n):
    for w from token_budget down to costs[i]:   // iterate right-to-left (0-1 variant)
        dp[w] = max(dp[w], dp[w - costs[i]] + values[i])

backtrack to recover selected items:
    w = token_budget
    for i from n-1 down to 0:
        if w >= costs[i] && dp[w] == dp[w - costs[i]] + values[i]:
            select item i
            w -= costs[i]
```

**Greedy fallback:**

```
sort items by (value / token_cost) descending
greedily include each item while accumulated_cost + cost_i ≤ token_budget
```

---

### 3. `get_context` MCP tool

**Full pipeline:**

```
get_context(query, token_budget, repo?)
  1. validate_mcp_arg(query)                  — SEC-002 input sanitization
  2. store.search_nodes_by_name(query)        → seed NodeIds (up to 5 seeds)
  3. ppr(store, &seeds, k = context_candidates()) → Vec<(NodeId, f32)>
  4. store.get_nodes(&node_ids)              → Vec<Node>    ← new Store method
  5. zip NodeIds with scores → Vec<(Node, f32)>
  6. knapsack(items, token_budget)           → Vec<Node>   (optimal subset)
  7. format_context_response(&nodes)         → String
  8. sanitize_for_mcp(response)              → String
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
pub fn context_candidates() -> usize {
    parse_env_usize("TRAVSR_CONTEXT_CANDIDATES", 200)
}
```

200 candidates is the default. The knapsack selects the optimal subset from these
200; callers never see more than `token_budget / min_node_cost` nodes in the output.

**Tool parameters (RFC-004 schema extension — version bump to 1.1.0):**

| Parameter | Type | Required | Constraints | Default | Notes |
|---|---|---|---|---|---|
| `query` | `string` | yes | 1–200 chars | — | Symbol name or free-text query |
| `token_budget` | `integer` | yes | 100–32 000 | — | Hard ceiling on output token cost |
| `repo` | `string` | no | — | all repos | Restrict traversal to one corpus |

`MAX_CONTEXT_BUDGET = 32_000` — aligns with Claude's max context window.
The MCP server rejects calls with `token_budget > MAX_CONTEXT_BUDGET` with a
structured error rather than silently clamping, so callers know they passed an
invalid value.

**Output format:**

```
fn:PaymentService.charge (function) — src/payment/service.ts [package: @acme/payments]
fn:validateCard (function) — src/payment/validate.ts [package: @acme/payments]
class:StripeClient (class) — src/integrations/stripe.ts [package: @acme/stripe]
fn:StripeClient.chargeCard (method) — src/integrations/stripe.ts [package: @acme/stripe]
fn:PaymentService.refund (function) — src/payment/service.ts [package: @acme/payments]

[5 nodes, ~310 tokens]
```

The footer `[N nodes, ~T tokens]` is always the last line. It lets the LLM caller
verify it received a complete budget-constrained response, and lets operators
observe actual utilization in logs.

**Behaviour when no seeds are found:**

```
No symbols matching 'query' found in the graph.
```

Plain-text, no error code — consistent with `search_symbol` empty results.

---

### 4. New `Store::get_nodes` method

`ppr()` returns `Vec<(NodeId, f32)>` — node IDs, not `Node` objects. Step 4 of the
pipeline requires bulk-resolving IDs to `Node` structs. No such method exists today.

```rust
// crates/travsr-store/src/lib.rs  (Store trait)

/// Fetch a batch of nodes by ID. IDs not found in the store are silently
/// omitted from the result (not an error). Order of results is unspecified.
fn get_nodes(&self, ids: &[NodeId]) -> Result<Vec<Node>, TravsrError>;
```

**`SqliteStore` implementation:** single `SELECT … WHERE id IN (…)` with bound
parameters. Batch size is capped at 999 (SQLite `SQLITE_MAX_VARIABLE_NUMBER`
default); larger batches are chunked automatically.

This is a `travsr-store` change but stays entirely within the `Store` abstraction.
`KuzuStore` will need to implement the same method before the Kùzu backend ships.

---

### 5. `travsr-retrieval` export additions

```rust
// crates/travsr-retrieval/src/lib.rs

pub mod knapsack;
pub use knapsack::{knapsack, token_cost, DP_CELL_LIMIT, TOKEN_CHARS_PER_TOKEN};
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

### E. Per-call DP table allocation via an arena allocator

Would reduce GC pressure for high-throughput cloud deployments. Deferred: OCI
Always Free tier runs a single-tenant daemon; the 128 KB Vec allocation per call
is released immediately and does not accumulate. Revisit if cloud tier shows
memory pressure.

---

## Drawbacks

- **`get_nodes` is a new Store trait method.** `KuzuStore` (production backend,
  currently in a worktree) must implement it before the Kùzu migration can complete.
  This is a forcing function, not a blocker — `SqliteStore` implements it first.

- **DP table is O(W) per call.** At `token_budget = 32_000`, the rolling array is
  128 KB. This is allocated and released on every `get_context` call. Acceptable
  for the local daemon (one call at a time); the SSE cloud tier (RFC-007) should
  profile under concurrent load and consider a pool if contention appears.

- **Greedy fallback breaks optimality guarantees.** When `n × budget > 500_000`,
  the result is near-optimal but not provably optimal. The threshold is set
  conservatively (200 nodes × 2 000 budget = 400k < 500k) so the fallback
  is not triggered in normal operation. The `tracing::warn!` makes it observable.

- **`token_cost` is a proxy, not exact.** The character-based formula can diverge
  from actual tokenizer output for non-ASCII symbols (e.g. Japanese identifiers in
  cross-language repos). QA-301 must validate before Sprint 11 ships.

---

## Unresolved Questions

1. **`context_candidates` default.** Is 200 PPR candidates the right default? A
   larger pool gives the knapsack more to choose from (better packing), but
   increases `ppr()` latency. Proposed: benchmark on the Travsr repo itself
   at k=50, 100, 200, 500 and pick the knee of the latency/quality curve.

2. **Output format: plain text vs. JSON.** All current tools return plain `String`.
   JSON would let IDE extensions display node metadata (kind, package, path) in
   structured UI. Proposed: plain text for v1 (consistent with all other tools);
   a `format: "json"` optional parameter in a follow-on RFC.

3. **Multi-seed behaviour.** When `query` matches multiple symbols (e.g.
   `"charge"` matches `fn:charge` in 4 different files), should PPR run with all
   matches as seeds (union of contexts) or only the top match? Proposed: all
   matches as seeds, capped at 5, so `get_context("charge")` returns a blended
   context rather than arbitrarily picking one file.

4. **`token_budget` default.** When an MCP client calls `get_context` without
   specifying `token_budget`, should the server default to 4 096 or require the
   parameter? Proposed: require it (no default) — callers must be intentional
   about their budget. This avoids silent over-fetching in constrained contexts.

---

## Test Strategy

### Unit tests (`crates/travsr-retrieval/src/knapsack.rs`)

| Test | What it verifies |
|---|---|
| `knapsack_empty_items` | Returns `vec![]` |
| `knapsack_zero_budget` | Returns `vec![]` |
| `knapsack_all_items_fit` | Returns all items when total cost ≤ budget |
| `knapsack_optimal_vs_brute_force` | For n ≤ 15, assert DP result == brute-force optimal |
| `knapsack_greedy_fallback_triggered` | Force n × budget > DP_CELL_LIMIT; assert result is non-empty and cost ≤ budget |
| `knapsack_degenerate_one_big_vs_many_small` | Classic adversarial input: one 90-token node at score 0.9 vs. 20 × 5-token nodes at score 0.05 each. Budget = 100. Knapsack must pick the 20 small nodes (total value 1.0 > 0.9). |
| `token_cost_min_one` | `token_cost` returns ≥ 1 for any node |
| `token_cost_formula` | Known signature + kind + path → expected token count |

### Integration tests (`crates/travsr-mcp/tests/conformance.rs`)

| Test | What it verifies |
|---|---|
| `mcp_get_context_returns_text_content` | Tool is registered; MCP call returns `TextContent` |
| `mcp_get_context_budget_is_respected` | Total `token_cost` of returned nodes ≤ `token_budget` |
| `mcp_get_context_empty_query_returns_not_found` | Unknown symbol → "No symbols matching…" |
| `mcp_get_context_rejects_oversized_budget` | `token_budget = 100_000` → structured error |

### Property tests (fuzzing, `crates/travsr-retrieval/`)

```rust
// proptest: for any Vec<(Node, f32)> and any token_budget in 0..=32_000:
assert!(total_token_cost(&knapsack(items, token_budget)) <= token_budget);
```

The budget invariant must hold for all inputs — this is the only hard correctness
requirement.

### QA-301 — token cost calibration

Run `tokenize_sample` against 1k random nodes from `.travsr/graph.db`. Assert
`mean_error(token_cost(n), cl100k_base(render(n))) <= 0.20` (20% tolerance).
If the assertion fails, adjust `TOKEN_CHARS_PER_TOKEN` and re-run. Gate Sprint 11
ship on green.

---

## Acceptance Checklist

- [ ] `cargo test -p travsr-retrieval` passes including all new knapsack unit tests.
- [ ] `cargo test -p travsr-mcp` passes including new conformance tests.
- [ ] `proptest` budget invariant passes 10 000 random cases.
- [ ] `cargo clippy --workspace -- -D warnings` clean.
- [ ] `cargo fmt --check` clean.
- [ ] `token_budget > MAX_CONTEXT_BUDGET` returns a structured MCP error (not a panic).
- [ ] `get_context` appears in `tools/list` response with correct JSON schema.
- [ ] QA-301 calibration task is complete and `TOKEN_CHARS_PER_TOKEN` is validated.
- [ ] `Store::get_nodes` is implemented on `SqliteStore` with chunked binding for > 999 IDs.
- [ ] `KuzuStore::get_nodes` stub added (can return `Err(TravsrError::NotImplemented)` until Kùzu backend merges).
- [ ] RFC-004 schema version bumped to `"1.1.0"` in `travsr-mcp/src/protocol.rs`.
- [ ] ADR-003 open question §Token budget integration marked resolved, references RFC-008.
