# RFC-012: Fuzzy Seed Selection

| Field | Value |
|---|---|
| **Status** | Draft |
| **Author** | Tech Lead |
| **Date** | 2026-05-31 |
| **Issue** | #258 |
| **Phase** | 4 (post-v0.6.0) → Phase 5 staging |
| **Crate(s) affected** | `travsr-store`, `travsr-mcp`, `travsr-retrieval`, `travsr-cli` |
| **Depends on** | RFC-004 (MCP tool schemas), RFC-010 (knapsack), ADR-003 (PPR policy) |

---

## Summary

Replace the substring-only seed selection used by `search_symbol`, `get_context`, and `travsr ask` with a layered fuzzy-input pipeline:

1. **L1 - Lexical FTS5 with snake/camel tokenization** (this RFC, S19)
2. **L2 - LLM query translator** (next RFC, S20)
3. **L3 - Embedding sidecar via `sqlite-vec`** (deferred to Phase 5)

L1 alone unblocks the majority of failed queries observed in the v0.6.0 benchmarks (e.g. `"mcp dispatch tool call"` currently returns zero results despite `dispatch_tool_call` existing). L2 fulfills the manifesto's "LLMs translate queries" clause. L3 is the escape valve for genuinely semantic queries and is kept out of the core retrieval path so the graph's determinism guarantee is never compromised.

The non-negotiable invariant: **fuzzy mechanisms only pick seed nodes. Edges remain deterministic. The PPR / knapsack / PCST pipeline is unchanged.**

---

## Motivation

### Observed gap (v0.6.0 benchmark, 2026-05-31)

Across four benchmark queries against the live Travsr graph:

| Query | Tool | Result |
|---|---|---|
| `knapsack` | `get_callers` | exact symbol hit, 24 tokens |
| `crates/travsr-core/src/lib.rs` | `get_blast_radius` | exact file hit, 14 tokens |
| `crates/travsr-mcp/src/tools.rs` | `get_dependencies` | exact path hit, 193 tokens |
| `mcp dispatch tool call` | `get_context` | **0 results** |

The first three queries hit `SqliteStore::search_nodes_by_name` (`signature LIKE '%x%' OR path LIKE '%x%'`) and succeed because the user already knows the exact substring. The fourth fails because no single graph node has the four-word literal string in either field. This is precisely the kind of query an LLM client emits when asked an English question, and it is the boundary at which Travsr currently looks worse than vector RAG.

### Why this matters now

The retrieval algorithm stack (BFS, PPR, knapsack, PCST) is complete as of v0.6.0. Every algorithm assumes a seed node. The seed-selection step is now the lowest-quality link in the chain - improving the algorithms further has diminishing returns until seed selection catches up.

### Why this RFC, not a quiet patch

Adding fuzzy matching is also a precedent. Without an explicit RFC, a future contributor will eventually answer the "fuzzy search" customer request by embedding every node and shipping vector NN search - which would violate the "LLM last" manifesto by making the seed step itself a neural inference. The RFC fixes the ordering: lexical first, LLM-translated second, embeddings last and quarantined.

---

## Non-Goals

- Re-ranking the graph traversal output. PPR remains the final scorer.
- Embedding nodes (L3 is sketched here but not specified in detail).
- Replacing `search_symbol`'s deterministic substring behavior. Its existing semantics ship as the FTS5 fallback path so that exact-name queries (`knapsack`, `VName`) never regress.
- Multilingual stemming. ASCII identifier matching only.
- Synonym dictionaries (`db` → `database`). Deferred - `tokenize_camel` covers the common code-style case.

---

## Detailed Design - L1: Lexical FTS5

### 1.1 Schema migration `v8_nodes_fts.sql`

```sql
-- crates/travsr-store/src/migrations/v8_nodes_fts.sql

-- FTS5 contentless virtual table over node identifiers.
-- Contentless mode (content='') keeps the FTS index from duplicating
-- node data; we look up the row in `nodes` by id after a match.
CREATE VIRTUAL TABLE IF NOT EXISTS nodes_fts USING fts5(
    tokens,            -- pre-tokenized signature + path + kind
    content='',
    tokenize='trigram'  -- handles typos + sub-token matches
);

-- Map FTS rowid back to node id (NodeId is 32 BLAKE3 bytes - not an integer).
CREATE TABLE IF NOT EXISTS nodes_fts_map (
    rowid INTEGER PRIMARY KEY AUTOINCREMENT,
    node_id BLOB NOT NULL UNIQUE
);

CREATE INDEX IF NOT EXISTS nodes_fts_map_node_id_idx
    ON nodes_fts_map(node_id);
```

Tokenizer choice: **trigram**, not `unicode61` or `porter`. Trigram catches `dispatch` in `dispatch_tool_call` regardless of the surrounding word boundaries, and tolerates a single character typo. The cost is a larger FTS index (~3× the contentless table) but per-node payload is small (~80 bytes of tokens text), so total overhead on the Travsr-self-index is ~200 KB.

### 1.2 Tokenizer: `tokenize_identifier`

```rust
// crates/travsr-store/src/fts_tokenize.rs

/// Split an identifier into searchable tokens.
///
/// Examples:
///   "dispatch_tool_call"     -> ["dispatch", "tool", "call"]
///   "getCallersGlobal"       -> ["get", "callers", "global"]
///   "MAX_CONTEXT_BUDGET"     -> ["max", "context", "budget"]
///   "travsr-mcp::sse"        -> ["travsr", "mcp", "sse"]
///   "src/payment.ts"         -> ["src", "payment", "ts"]
///
/// Output is lowercased, space-separated, suitable for direct insert into
/// the FTS5 `tokens` column. The original identifier is also emitted as
/// a single token so exact-substring queries continue to hit.
pub fn tokenize_identifier(s: &str) -> String { /* ... */ }
```

Rules:
- Split on `_`, `-`, `:`, `/`, `.`, ` `
- Split at `CamelCase` boundaries (lower → upper transition, ASCII only)
- Split runs of digits as separate tokens
- Emit each piece lowercased
- Emit the original string lowercased as one extra token to preserve substring search

### 1.3 Indexing path

`SqliteStore::put_node` gains a sibling write:

```rust
fn put_node_fts(&self, node: &Node) -> Result<(), StoreError> {
    let tokens = format!(
        "{} {} {} {}",
        tokenize_identifier(&node.vname.signature),
        tokenize_identifier(&node.vname.path),
        node.kind,
        node.vname.language
    );
    // upsert into nodes_fts_map to get rowid, then insert into nodes_fts.
}
```

Called from every node-write path (initial index, reindex_files, migration v8 backfill).

### 1.4 Query API: `search_nodes_fuzzy`

```rust
impl SqliteStore {
    /// Layered seed selection.
    ///
    /// 1. Try `search_nodes_by_name` (existing exact-substring) - if it
    ///    returns ≥1 result, use those.
    /// 2. Else MATCH the FTS5 index with the tokenized query.
    /// 3. Else return empty.
    ///
    /// Always capped at 50 results to bound PPR seed-set size.
    pub fn search_nodes_fuzzy(&self, query: &str) -> Result<Vec<Node>, StoreError>;
}
```

The layered fallback means existing benchmarks do not regress: any test that was previously hitting substring continues to hit substring. The fuzzy path is opt-in via cache miss.

### 1.5 MCP tool wiring

| Tool | Today | After L1 |
|---|---|---|
| `search_symbol(name)` | `search_nodes_by_name` | `search_nodes_fuzzy` |
| `get_context(query, budget)` | `search_nodes_by_name` → PPR seeds | `search_nodes_fuzzy` → PPR seeds |
| `travsr ask <query>` | same | same |

No new MCP tools. No JSON schema changes (the `name` and `query` arguments stay strings; the contract just loosens internally).

### 1.6 Migration backfill

`v8` runs after the existing schema is upgraded. Backfill is a single `INSERT … SELECT` from `nodes` into the FTS table; on the Travsr-self-index (2.6k nodes) this is sub-second. For users with larger graphs, backfill runs once at upgrade time inside the migration transaction - no separate `travsr reindex` step required.

---

## Detailed Design - L2: LLM Query Translator (sketch)

Specified in a follow-up RFC (RFC-013, post-S19). High-level shape only here so that L1 leaves the right seams.

The translator runs **in the MCP client, not the server**, so the daemon stays LLM-free. Contract:

```
input:  natural-language string ("the thing that talks to the AI")
output: structured query  { symbols: ["mcp", "server"], paths: ["crates/travsr-mcp/**"], kinds: ["function"] }
```

The structured query lowers to multiple `search_nodes_fuzzy` calls, unioned to form the PPR seed set. The MCP server never knows the original natural-language string. This preserves the property that the server's behavior is deterministic given identical structured input.

The "LLM translates, algorithms decide" line is therefore literal: the LLM lives on the other side of the MCP transport and only produces structured filter terms. Everything past `dispatch_tool_call` is exactly the same code path that today's exact-name queries take.

---

## Detailed Design - L3: Embedding Sidecar (deferred)

Sketch only. Concrete spec deferred to Phase 5, contingent on observed failure rate of L1+L2.

- Dependency: `sqlite-vec` (single-crate, ~150 KB compiled).
- Embedding model: `bge-small-en-v1.5` (33 M parameters, ~120 MB).
- Bundled with the binary on macOS/Linux. Windows opt-in.
- Index lifetime parallels `graph.db`, located at `.travsr/graph.vec`.
- Embeds: `signature + first-line-doc + kind`. Never embeds bodies.
- Inserted at the same point as `nodes_fts_map`, after `put_node`.
- Query path: cosine NN top-K → those become PPR seeds. **The vector layer never returns answers; it only proposes starting nodes.**

The 25 MB tarball budget (ADR-004) does not accommodate a 120 MB model. L3 therefore ships behind a `--features embedding` flag with a post-install model download step modeled on the kuzu feature. L3 must not regress default-install size.

---

## Why Not Just Embed Everything

Three reasons, in order of importance:

1. **Manifesto.** "Algorithms first, LLM last" is the project's identity. Bolting on embeddings as the primary search step inverts the order and makes Travsr indistinguishable from vector RAG (the thing it was built to replace) at the boundary the user actually touches.
2. **Cost.** L1 ships in ~400 LOC and adds ~200 KB to the database. L3 adds 120 MB to the binary and ~10 ms per query on cold cache. Most queries don't need it.
3. **Determinism erosion.** Once a vector layer exists, every subsequent retrieval RFC will be tempted to lean on it. The layered approach forces every future contributor to first prove that L1 + L2 are insufficient for their use case.

---

## Alternatives Considered

### A1. Skip L1, jump straight to embeddings
Rejected. Solves the symptom (`dispatch_tool_call` query fails) but commits the project to the wrong default. Also adds a 120 MB binary cost to every user for a query class that L1 handles correctly.

### A2. `SQL LIKE` with tokenized wildcards
Tried in prototype. Splitting `"mcp dispatch tool"` into three `LIKE` clauses ANDed together works for the demo case but scales O(n × terms) and has no relevance ranking. FTS5 trigram is a strict superset for ~150 lines more code and gives BM25 scoring for free.

### A3. External search service (Tantivy, Meilisearch)
Rejected. New process, new ops burden, breaks the single-binary install promise. SQLite FTS5 is already linked; using it costs zero ops.

### A4. Synonym expansion in the indexer (`db` → `database`, `req` → `request`)
Rejected for v1. Maintenance hazard, locale-specific, and L1's trigram + camel-split already handles `dispatch_tool_call` from `"tool dispatch"`. Revisit if telemetry shows real synonym misses.

---

## Migration / Compatibility

- **Schema:** migration v8 is forward-only. Old daemons reading a v8 database are protected by the existing schema-version gate.
- **API:** `search_symbol` and `get_context` JSON schemas are unchanged. The `name` and `query` argument semantics broaden (any query that worked before still works); no client code must change.
- **Determinism:** unchanged on identical inputs. The fuzzy step is deterministic - same tokenization, same FTS5 result ranking.
- **Performance:** FTS5 lookup adds ~0.5 ms p50, ~2 ms p99 on the Travsr-self-index. Within the existing p95 < 50 ms budget by 25×.

---

## Open Questions

1. **Should `search_symbol` expose the fuzzy fallback explicitly?** Option A: tools.rs hides the layering and clients see one tool that "just works." Option B: add a `fuzzy: bool` argument so deterministic clients can opt out. Lean: A - the tool's job is "find symbols by name"; whether it uses substring or FTS underneath is an implementation detail.
2. **Token tokenization for non-ASCII identifiers.** `tokenize_identifier` currently splits ASCII case boundaries only. Identifiers in CJK / Cyrillic source will be emitted as one trigram blob. Acceptable for v1; flag in QA-014.
3. **FTS index rebuild on schema migrations v9+.** If a future migration alters `nodes.signature`, the FTS table can drift. Solve via a checksum row in `nodes_fts_map` that triggers a rebuild on mismatch. Defer to L1 implementation.
4. **Telemetry for fallthrough rate.** We need a metric for "fuzzy MISS → returned empty" to know when L2 / L3 are worth the cost. Reuse the existing `tracing` span counters; expose via the metrics endpoint added in RFC-007.

---

## Sprint Shape

| Sprint | Scope | Crate impact |
|---|---|---|
| **S19 (L1)** | This RFC, end-to-end: migration v8, tokenizer, `search_nodes_fuzzy`, MCP wiring, ≥10 fuzzy-query tests in `tests/fuzzy.rs`, dogfooding-doc benchmark update | `travsr-store` (+400 LOC), `travsr-mcp` (+30 LOC), `travsr-cli` (+5 LOC) |
| **S20 (L2)** | RFC-013 + implementation of the in-client query translator. No server changes beyond a JSON-schema doc update describing the structured-query convention. | docs only on the server side; `travsr-vscode` and any other client gains a translator module |
| **Phase 5 (L3)** | RFC-014 + `--features embedding`. Out of scope for this RFC. | new `travsr-embed` crate |

---

## Acceptance Criteria

- [ ] Migration v8 ships and is idempotent (re-running on a v8 db is a no-op).
- [ ] `tokenize_identifier` handles snake_case, CamelCase, kebab-case, `:`, `/`, `.`, digits.
- [ ] `search_nodes_fuzzy("mcp dispatch tool call")` returns `fn:dispatch_tool_call` as the top result on the Travsr-self-index.
- [ ] Existing `search_nodes_by_name` exact-substring queries return identical results - no regression in `QA-012` MCP conformance suite.
- [ ] p95 latency for `get_context` on the 2.6k-node Travsr-self-index remains < 50 ms.
- [ ] FTS index adds < 5 % to `graph.db` size on the Travsr-self-index.
- [ ] `dogfooding.md` benchmark table updated with the natural-language query class.

---

## References

- ADR-003 - PPR policy (seed-set semantics)
- RFC-004 - MCP tool JSON schemas (contract for `search_symbol` / `get_context`)
- RFC-010 - knapsack token-budget enforcer (downstream consumer of seed selection)
- SQLite FTS5 docs - https://www.sqlite.org/fts5.html (trigram tokenizer §4.3.5)
- BLAKE3 `NodeId` rationale - `crates/travsr-core/src/lib.rs` (rowid mapping context)
