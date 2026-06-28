# Plan — `get_context` MCP Tool Enhancements

> Status: Draft for sign-off
> Author: Solution Architect + Software Engineer (travsr personas)
> Date: 2026-06-17
> Related: PR #354 (`get_snippets`, merged path), RFC-012-ADDENDUM-02 (L2-B), RFC-010 (knapsack budget)

---

## 1. Why

`get_context` is the flagship retrieval tool — the full `search_nodes_fuzzy → PPR → knapsack` pipeline. Today it returns **structural metadata only** (signature, kind, path, package) and its semantic re-ranking step is a no-op stub. Three gaps remain between today's behaviour and a "complete" `get_context`:

| # | Gap | Memory ref |
|---|---|---|
| 1 | No way to get code + context in one call — agents must follow up with `get_snippets` | `project-get-context-roadmap` Item 1 |
| 2 | Step 4 (ONNX re-rank) is a stub → PPR seeds are pure FTS5 matches, no semantic recall | `project-get-context-roadmap` Item 2, `f2-embeddings-deps` |
| 3 | Multi-node snippet output is flat — no graph-relationship annotations between blocks | `project-get-context-roadmap` Item 3 |

The non-negotiable that frames all three: **Algorithms first, LLM last.** Item 2 (embeddings) is the only one that touches model inference, and even there the model only *re-ranks* candidates the graph already produced — it never invents nodes or edges (PA C4 bright line).

---

## 2. Current pipeline (verified against code 2026-06-17)

```
get_context_body()                         crates/travsr-mcp/src/tools.rs:1365
  └─ store.search_nodes_fuzzy(query)        crates/travsr-store/src/lib.rs:2099
       Step 1  exact substring             ← works
       Step 2  FTS5 trigram (synonyms DB)  ← works  (RFC-012 A2 F1)
       Step 3  L2-A vocab expansion        ← works  (RFC-012 A1)
       Step 4  ONNX re-rank                ← MISSING (not even called here)
  └─ travsr_retrieval::ppr(seeds)          ← works
  └─ knapsack(items, token_budget)         ← works  tools.rs:1441
  └─ format metadata lines                 ← works  tools.rs:1446
```

Dispatch + schema live in **two** places each — both must be touched for any contract change:
- Local store path: dispatch `server.rs:159`, schema `server.rs:354`
- Global repos path: dispatch `server.rs:662`, schema `server.rs:845`

Snippet machinery already exists and is reusable:
- `snippet_for_node(node, repo_root)` — `tools.rs:3241` (kind-aware caps, SEC path guard, docblock strip)
- `get_snippets_body` — `tools.rs:3317`
- `SNIPPET_SEP`, `snippet_line_cap`, `skip_leading_comments`

---

## 3. Work items

### Item 1 — `include_snippets` flag on `get_context`  (ship first; XS, ~40 LOC)

**Goal:** one round-trip from query to code. `get_context(query, token_budget, include_snippets=true)` returns the knapsack-selected nodes *with* their source bodies inline.

**Contract (additive, backward compatible):**
```jsonc
// get_context input schema — new optional fields, both default to legacy behaviour
"include_snippets": {
  "type": "boolean",
  "description": "When true, append the actual source body of each selected symbol (kind-aware, docblock-stripped)."
},
"snippet_budget": {
  "type": "integer",
  "description": "Optional separate token budget for the appended snippets. When omitted, snippets share the main token_budget (best-effort enrichment)."
}
```
Omitting both reproduces today's exact output → zero breaking change for existing clients.

**Budget model — support BOTH shared and separate; pick the default empirically.**
The implementation must accommodate two scenarios and we decide the recommended default by testing which yields better results in practice:

- **Shared budget** (`snippet_budget` omitted): knapsack selects nodes under `token_budget`; snippets are then appended in PPR-score order, accumulating `header+snippet` cost, stopping once the *total* (metadata + snippets) exceeds `token_budget`. Nodes past the cutoff stay metadata-only (graceful degradation, mirrors `get_snippets_body:3375`). Keeps the knapsack contract intact (it still optimises *which* nodes matter) and treats code as best-effort enrichment within one ceiling.
- **Separate budget** (`snippet_budget` provided): `token_budget` governs metadata/node selection exactly as today; snippets get their own independent ceiling (`snippet_budget`), reported separately in the footer. This mirrors how the standalone `get_snippets` tool already works (own budget, own cap) and avoids snippets crowding out node selection.

Both paths share the same snippet renderer and the same SEC/RBAC guards; the only difference is which budget the snippet accumulation loop charges against. We benchmark both on real queries before recommending a default.

**Implementation:**
1. `get_context_body` signature gains `include_snippets: bool` and `snippet_budget: Option<usize>`. Thread through `get_context_with_filter`, `get_context_authed`, `get_context_raw`, and the public `get_context`.
2. Snippet accumulation loop charges against `snippet_budget.unwrap_or(remaining_main_budget)` per the model above; footer reports the budget actually used (and which mode).
3. **RBAC correctness (P0):** snippets must only be rendered for nodes that already passed `filter.allow(...)` at `tools.rs:1430`. Reuse the already-filtered `selected` list — never re-query. `snippet_for_node` reads from disk via `repo_root`, so an access-restricted corpus that was filtered out is never read.
4. **repo_root absence:** if `store.get_meta("repo_root")` is `None`/empty (pre-snippet indexes), silently fall back to metadata-only output + a one-line footer hint (do **not** error — same degradation as `get_snippets_body:3320`).
5. Format: reuse `SNIPPET_SEP` block layout from `get_snippets_body` for output consistency between the two tools.
6. Wire dispatch in `server.rs:159` (local) and `server.rs:662` (global) to read `args["include_snippets"].as_bool().unwrap_or(false)`; add the schema field at `server.rs:354` and `server.rs:845`.

**Tests (travsr-mcp):**
- `get_context_include_snippets_appends_code` — known symbol returns header + body.
- `get_context_include_snippets_false_is_byte_identical_to_legacy` — regression guard on backward compat.
- `get_context_include_snippets_shared_budget_truncates` — no `snippet_budget` → snippets share `token_budget`, overflow nodes stay metadata-only, footer counts correct.
- `get_context_include_snippets_separate_budget` — `snippet_budget` provided → node selection unaffected by snippet size; snippet ceiling enforced independently.
- `get_context_include_snippets_no_repo_root_degrades` — meta absent → metadata-only, no panic.
- `get_context_include_snippets_rbac_excluded_node_not_read` — filtered node's file is never opened.

**Risk:** Low. Pure additive, reuses audited snippet code. No migration, no deps.

---

### Item 2 — L2-B ONNX embedding re-rank (Step 4)  (largest; L, feature-gated, blocked on deps)

**Goal:** make `search_nodes_fuzzy` surface semantically-related seeds even when the symbol name doesn't lexically match the query. This is the biggest retrieval-quality lever.

**This is gated behind two unblocks (see `f2-embeddings-deps`):**

**Phase 2a — unblock deps (prerequisite, separate small PR):**
1. Pin `ort = { version = "2.0.0-rc.12", optional = true }` in `crates/travsr-store/Cargo.toml`.
2. Verify `sqlite-vec` crate name/version on crates.io; pin it `optional = true`.
3. `embeddings = ["dep:ort", "dep:sqlite-vec"]` in the feature table.
4. Register the `vec0` extension loader on the connection in `lib.rs` *before* the V12 migration runs (the V12 stub at `lib.rs:200` currently has no loader → enabling the feature fails today). This is the gating fix that makes the existing migration usable.

**Phase 2b — implement inference:**
5. `put_node_embedding` / `delete_node_embedding` in `lib.rs`, `#[cfg(feature = "embeddings")]`; wire into `put_node_fts` + all delete paths (mirror the existing FTS hook locations).
6. `travsr embed init` CLI command: download `nomic-embed-text` MRL-256, pin `rabitq_rotation_seed` to the `meta` table (BLOCKER documented in RFC-012 A2 Rev2).
7. **Step 4 in `search_nodes_fuzzy`** (`lib.rs:2099`): only fires on the combined Step 1+2+3 path (re-rank, not gate). Embed the query, vec0 KNN ANN, RaBitQ Hamming re-rank, merge into the candidate set *without dropping* any FTS/L2-A hit (additive — PA C1 invariant: raw FTS hits always retained).
8. `#[cfg(feature = "embeddings")]` on the whole Step 4 block so the default build is a byte-for-byte no-op (TL2 CI gate already enforces default-build parity).

**Tests:** feature-gated module; cosine/Hamming unit tests; "embeddings off = identical results" parity test; `embed init` integration test with a tiny fixture model.

**Risk:** High — pre-1.0 `ort` RC, model download, new migration in the live path. Keep strictly opt-in; security review required before any cloud build enables it (model provenance + download integrity).

**Dependency:** Item 2 is independent of Items 1 and 3 and can land last. Items 1/3 must NOT wait on it.

---

### Item 3 — Edge-relationship context  (RESOLVED: option B — not building now)

**Decision (2026-06-17):** keep `get_snippets` a pure, single-responsibility code-retrieval primitive. Relationship/structure context is **already** provided by the existing graph-navigation tools — `get_callers` (who calls a symbol) and `get_graph_json` (full local graph with edge kinds and depth). Adding an edges-join to `get_snippets` would partially duplicate tools that already do it better, and would couple the standalone primitive to graph state.

**Why option B:**
- Cleanest single-responsibility design — matches the goal of an independent `get_snippets`.
- Zero new work, zero risk — no edges-join, no schema touch, no new test surface.
- No redundancy — `get_graph_json` already returns edge kinds; `get_callers` already returns relationships. Workflow stays: navigate with graph tools, then read with `get_snippets`.

**Workflow it relies on:** `get_callers` / `get_graph_json` for structure → `get_snippets` for code (the CLAUDE.md "graph first, then read" sequence).

**Future optimization (deferred, not scheduled):** if a single-call "code + local relationships" or a "trace this execution path" need emerges, revisit as either (a) an opt-in `include_relationships` flag on `get_snippets`, or (c) a dedicated path-aware trace tool pairing with `get_execution_path`. Not in scope now.

---

## 4. Sequencing

```
Item 1 (include_snippets)   [no deps, ship now]
Item 2a (unblock ort/vec0)  ──▶  Item 2b (Step 4 inference)  [independent, parallel/last]
Item 3                       — resolved as option B (no build)
```

- **Sprint slice 1:** Item 1 → PR. Small, high-value, no deps.
- **Sprint slice 2 (parallel/whenever deps unblock):** Item 2a + 2b → PR, feature-gated, security review gate.

Each item is its own branch + PR (`feature/travsr-mcp-...`).

---

## 5. Cross-cutting requirements

- **Backward compatibility:** every contract change is additive and optional-default-false. Legacy clients see identical output. Item 1 has an explicit byte-identical regression test.
- **RBAC (SEC P0):** snippets and edge annotations are only ever produced for nodes already passed through `filter.allow(...)`. No new file read or edge query touches a node the filter rejected.
- **SEC path guard:** all snippet reads go through the existing 3-layer `snippet_for_node` guard (Unix abs / Windows drive / `..` + canonicalize + `starts_with(repo_root)`). No new disk-read path is introduced.
- **Token budget:** snippets + annotations share the single `token_budget`; never a hidden second budget. Footer counts reflect actuals.
- **Default-build parity:** Item 2 is fully `#[cfg(feature = "embeddings")]`; default `cargo build`/test must be unchanged (TL2 gate).
- **CI gates (per `feedback_ci_local_before_push`):** before each commit/push run fmt, clippy, `cargo test -p travsr-mcp` (and `-p travsr-store` for Item 2), cargo-deny, MSRV. Scope tests to changed crate (`feedback_test_scope`); escalate to workspace only if `travsr-core` interfaces change.
- **Local end-to-end (per `feedback_test_before_commit`):** build the release binary and exercise via real MCP call before committing each item; let user test before commit.

---

## 6. Open questions for sign-off

1. **Item 1 budget default** — RESOLVED in approach: implement *both* shared and separate (`snippet_budget`) modes; benchmark on real queries to pick the recommended default. Open sub-question: which becomes the documented default once benchmarked.
2. **Item 3 mechanism** — RESOLVED (2026-06-17): option B. Keep `get_snippets` pure; relationships come from `get_callers`/`get_graph_json`. No build now; (a)/(c) deferred as possible future optimization.
3. **Item 2 timing** — pursue the `ort`/`sqlite-vec` unblock now, or hold until Items 1+3 ship? (Recommendation: hold; ship 1+3 first, they need no deps.)
```
