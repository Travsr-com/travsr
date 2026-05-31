# RFC-012: Fuzzy Seed Selection

| Field | Value |
|---|---|
| **Status** | Draft |
| **Author** | Tech Lead |
| **Date** | 2026-05-31 |
| **Issue** | #258 |
| **Phase** | 4 (post-v0.6.0) -> Phase 5 |
| **Crate(s) affected** | `travsr-store`, `travsr-mcp`, `travsr-retrieval`, `travsr-cli`, `travsr-embed` (new) |
| **Depends on** | RFC-004 (MCP tool schemas, Accepted), RFC-007 (MCP SSE transport, Accepted), RFC-010 (knapsack, Accepted), ADR-003 (PPR policy, Accepted), ADR-004 (error taxonomy / tarball budget, Accepted) |
| **Decision owner** | Principal Architect (ratification gate before Draft → Accepted) |

---

## Summary

Replace the substring-only seed selection used by `search_symbol`, `get_context`, and `travsr ask` with a single, layered fuzzy-input pipeline specified in full here:

1. **L1 - Lexical FTS5** with snake/camel tokenization (sprint S19, default-on)
2. **L2 - LLM query translator** in the MCP client (sprint S20, default-on, transport-layer change only)
3. **L3 - Embedding sidecar** via `sqlite-vec` (Phase 5, behind `--features embedding`, opt-in)

All three layers share one query API (`search_nodes_fuzzy`) and one invariant: **fuzzy mechanisms select PPR seed nodes only. Edges remain deterministic. The PPR / knapsack / PCST pipeline is unchanged.**

The reason all three layers are in one RFC: each layer is meaningful only as part of the ordered stack. Specifying them separately would let a future contributor implement L3 first and short-circuit the deterministic-by-default property the project is built on. The RFC documents the *ordering* as much as the implementations.

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

The first three queries hit `SqliteStore::search_nodes_by_name` (`signature LIKE '%x%' OR path LIKE '%x%'`) and succeed because the user already knows the exact substring. The fourth fails because no single graph node has the four-word literal string in either field. This is the boundary at which Travsr currently looks worse than vector RAG.

### Why this matters now

The retrieval algorithm stack (BFS, PPR, knapsack, PCST) is complete as of v0.6.0. Every algorithm assumes a seed node. The seed-selection step is now the lowest-quality link in the chain - improving the algorithms further has diminishing returns until seed selection catches up.

### Why one RFC, not three

Adding fuzzy matching is a precedent-setting change. Without an explicit ordering, a future contributor will eventually ship vector NN search as the default seed-selection step - inverting the "algorithms first, LLM last" manifesto. Bundling L1, L2, and L3 into a single ratified RFC fixes the ordering as a project decision, not an implementation accident: lexical first, LLM-translated second, embeddings last and quarantined behind a feature flag.

---

## Non-Goals

- Re-ranking the graph traversal output. PPR remains the final scorer for all three layers.
- Replacing `search_symbol`'s deterministic substring behavior. Its existing semantics ship as the first step of the layered fallback so exact-name queries (`knapsack`, `VName`) never regress.
- Multilingual stemming. ASCII identifier matching only.
- Synonym dictionaries (`db` -> `database`). Deferred - tokenization plus L2 translation covers the common code-style case.
- Server-side LLM calls. L2 lives in the MCP client; the daemon stays LLM-free.

---

## Shared Architecture

```
                       +-------------------------+
client query string -> |  L2: LLM translator     |  (in MCP client only)
                       |  (default on, optional) |
                       +-----------+-------------+
                                   | structured query
                                   v
                       +-------------------------+
                       |  MCP server dispatch    |
                       +-----------+-------------+
                                   | calls search_nodes_fuzzy(s)
                                   v
                  +----------------+-----------------+
                  |  L1: search_nodes_fuzzy          |
                  |  1. exact substring (today)      |
                  |  2. FTS5 trigram MATCH (new)     |
                  |  3. L3 if compiled + miss        |
                  +----------------+-----------------+
                                   | seed node set
                                   v
                       +-------------------------+
                       |  PPR + knapsack + PCST  |  (unchanged)
                       +-------------------------+
                                   |
                                   v
                              tool result
```

All three layers funnel into `search_nodes_fuzzy`. From the rest of the codebase's perspective there is one fuzzy primitive; the layers are an implementation detail of that one function.

---

## Detailed Design - L1: Lexical FTS5

### 1.1 Schema migration `v8_nodes_fts.sql`

```sql
-- crates/travsr-store/src/migrations/v8_nodes_fts.sql

-- FTS5 contentless virtual table over node identifiers.
-- Contentless mode (content='') keeps the FTS index from duplicating
-- node data; we look up the row in `nodes` by id after a match.
-- The `tokens` column in nodes_fts_map (below) is REQUIRED to support
-- FTS5 delete commands: contentless tables cannot recover the original
-- tokenized values, so we must supply them explicitly for retraction.
CREATE VIRTUAL TABLE IF NOT EXISTS nodes_fts USING fts5(
    tokens,            -- pre-tokenized signature + path + kind
    content='',
    tokenize='trigram'  -- handles typos + sub-token matches
);

-- Map FTS rowid back to node id (NodeId is 32 BLAKE3 bytes - not an integer).
-- `tokens` is stored here (not in the contentless FTS table) so that
-- incremental reindex can issue the FTS5 'delete' command with the exact
-- original tokenized values — the only way to retract a contentless row.
-- Omitting `tokens` would make stale FTS rows unretractable on rename/delete.
CREATE TABLE IF NOT EXISTS nodes_fts_map (
    rowid    INTEGER PRIMARY KEY AUTOINCREMENT,
    node_id  BLOB    NOT NULL UNIQUE,
    tokens   TEXT    NOT NULL  -- retained for FTS5 delete; must match the inserted value exactly
);

CREATE INDEX IF NOT EXISTS nodes_fts_map_node_id_idx
    ON nodes_fts_map(node_id);
```

Tokenizer choice: **trigram**, not `unicode61` or `porter`. Trigram catches `dispatch` in `dispatch_tool_call` regardless of word boundaries and tolerates a single character typo. The FTS index is ~3x the size of the raw token data; per-node payload is small (~80 bytes), so total overhead on the Travsr-self-index is ~200 KB — well under the CI acceptance criterion of < 5% of `graph.db` (Acceptance Criteria L1).

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
- Split at `CamelCase` boundaries (lower -> upper transition, ASCII only)
- Split runs of digits as separate tokens
- Emit each piece lowercased
- Emit the original string lowercased as one extra token to preserve substring search

**Known limitation — tokens shorter than 3 characters:** FTS5 `trigram` only indexes and matches character sequences ≥ 3 characters. `tokenize_identifier` legitimately emits 2-char tokens (`ts`, `id`, `db`, `fs`, `by`). These 2-char tokens are not searchable via the FTS5 path. Mitigation: step 1 of `search_nodes_fuzzy` is an exact-substring match that runs *before* the FTS path; any query that contains or is a short identifier will still resolve correctly via substring. The FTS path is only reached on substring miss, so the practical impact is limited to natural-language queries that are *only* 2-char tokens — an unlikely case. Tracked in Open Question #2.

### 1.3 Indexing path

`SqliteStore::put_node` gains three sibling operations covering the full write/update/delete lifecycle. This is non-optional: omitting delete/update would silently drift the FTS index away from the live graph on every incremental reindex, violating the "always fresh" non-negotiable.

**Insert (`put_node_fts`):**
```rust
fn put_node_fts(&self, node: &Node) -> Result<(), StoreError> {
    let tokens = format!(
        "{} {} {} {}",
        tokenize_identifier(&node.vname.signature),
        tokenize_identifier(&node.vname.path),
        node.kind,
        node.vname.language
    );
    // 1. Upsert into nodes_fts_map (node_id, tokens) → get rowid.
    //    Store `tokens` — required to retract the FTS row later.
    // 2. INSERT INTO nodes_fts(rowid, tokens) VALUES(?, ?)
}
```

**Delete (`delete_node_fts`):** Called from every node-removal path (`reindex_files` when a symbol is renamed or removed):
```rust
fn delete_node_fts(&self, node_id: &NodeId) -> Result<(), StoreError> {
    // 1. SELECT rowid, tokens FROM nodes_fts_map WHERE node_id = ?
    //    (tokens was stored at insert time — contentless FTS cannot recover it)
    // 2. Issue the FTS5 special delete command:
    //    INSERT INTO nodes_fts(nodes_fts, rowid, tokens) VALUES('delete', ?, ?)
    // 3. DELETE FROM nodes_fts_map WHERE node_id = ?
}
```

This is the only correct retraction path for contentless FTS5 tables: the original tokenized string must be supplied explicitly. Storing `tokens` in `nodes_fts_map` makes this possible without reading back from the FTS virtual table (which contentless tables do not support).

**Update:** A symbol rename is a `delete_node_fts(old_id)` followed by `put_node_fts(new_node)`. There is no in-place FTS5 row update for contentless tables.

Called from every node-write path (initial index, `reindex_files`, migration v8 backfill).

### 1.4 Query API: `search_nodes_fuzzy`

```rust
impl SqliteStore {
    /// Layered seed selection.
    ///
    /// 1. Try `search_nodes_by_name` (existing exact-substring) - if it
    ///    returns >=1 result, use those.
    /// 2. Else MATCH the FTS5 index with the tokenized query.
    /// 3. Else (if compiled with `embedding` feature) call into L3.
    /// 4. Else return empty.
    ///
    /// Always capped at 50 results to bound PPR seed-set size.
    ///
    /// **Seed K passed to PPR:** `search_nodes_fuzzy` returns at most 50 nodes.
    /// All of them are handed to PPR as seeds (K = |results|, bounded at 50).
    /// L3 internally fetches its top-10 by cosine distance; these are merged into
    /// the same 50-node output cap, not added on top. This reconciles §3.7 (LIMIT 10)
    /// with the 50-node cap here and the client fan-out re-cap in §2.4: 10 is the
    /// per-layer L3 fetch limit; 50 is the unified seed-set limit across all layers.
    /// See ADR-003 for the rationale behind the 50-seed bound on PPR diffusion.
    pub fn search_nodes_fuzzy(&self, query: &str) -> Result<Vec<Node>, StoreError>;
}
```

The layered fallback means existing benchmarks do not regress: any test previously hitting substring continues to hit substring. The fuzzy path is opt-in via cache miss.

### 1.5 MCP tool wiring

| Tool | Today | After L1 |
|---|---|---|
| `search_symbol(name)` | `search_nodes_by_name` | `search_nodes_fuzzy` |
| `get_context(query, budget)` | `search_nodes_by_name` -> PPR seeds | `search_nodes_fuzzy` -> PPR seeds |
| `travsr ask <query>` | same | same |

No new MCP tools. No JSON schema changes (the `name` and `query` arguments stay strings; the contract just loosens internally).

### 1.6 Migration backfill

`v8` runs after the existing schema upgrade. Backfill is a single `INSERT … SELECT` from `nodes` into the FTS table; on the Travsr-self-index (2.6k nodes) this is sub-second. For users with larger graphs, backfill runs once at upgrade time inside the migration transaction - no separate `travsr reindex` step required. **Large-repo caveat:** for graphs with >500k nodes the single transaction holds a WAL write lock for the duration of tokenize+insert. If this becomes a measured issue in Phase 5 telemetry, the backfill can be chunked into batches of 10k rows with intermediate commits; this is deferred rather than pre-optimized.

---

## Detailed Design - L2: LLM Query Translator

### 2.0 Explicit decision: L2 default-on and "algorithms first, LLM last"

L2 ships default-on. This is a deliberate, owned exception to the "algorithms first, LLM last" manifesto — not an oversight — and must be ratified explicitly here rather than implied.

**Why L2 default-on does not violate the manifesto:**
- "Algorithms first, LLM last" means the *daemon* does not use LLMs to determine edges, relationships, or traversal. That invariant holds: the daemon receives only structured symbol-name inputs; all graph traversal (PPR, knapsack, PCST) remains fully deterministic and LLM-free.
- L2 is a *query-translation UX layer* in the MCP client. It converts a user's natural-language phrasing into symbol-name fragments that L1 can look up. It never touches the graph itself.
- If L2 is absent or fails, the experience degrades to L1 (deterministic substring + FTS), never below it. Default-on is therefore a UX improvement with a deterministic floor.

**What this RFC explicitly decides:** L2 may be client-side default-on. Any future proposal to move LLM involvement into the *daemon* (server-side) or to use LLMs to determine graph edges or traversal order requires a new RFC and Principal Architect sign-off. This RFC draws that line, not just permits L2.

### 2.1 Where it lives

L2 runs **in the MCP client, never in the server.** Concretely:

- `packages/travsr-vscode/src/queryTranslator.ts` (new file) - VS Code extension
- `packages/travsr-npm/scripts/translator.js` (new file) - reference impl shipped for any other MCP client
- The daemon (`travsr-mcp`) gains zero LLM dependencies. It still only sees structured arguments.

This placement is the key design choice. The MCP server is purely an algorithmic graph engine; the LLM lives on the consumer side of the transport. Same daemon, identical determinism guarantees, whether the client is `curl` issuing raw JSON-RPC or a VS Code extension with full natural-language input.

### 2.2 The translator contract

```typescript
// packages/travsr-vscode/src/queryTranslator.ts

export interface StructuredQuery {
  // OR-joined symbol-name fragments. Each becomes a search_nodes_fuzzy call.
  symbols: string[];
  // Optional path globs to scope the result set (intersected with symbols).
  paths?: string[];
  // Optional kind filter ("function", "class", "method", "struct", ...).
  kinds?: string[];
  // Plain English re-statement, attached for telemetry only. Never sent to daemon.
  echo: string;
}

export interface Translator {
  translate(naturalQuery: string): Promise<StructuredQuery>;
}
```

The translator returns at most 5 symbol fragments and at most 3 path globs. These caps bound the number of `search_nodes_fuzzy` calls the client makes per user query and prevent prompt-injection from causing tool-call storms.

### 2.3 Translation prompt

A single user-message-with-system-prompt round-trip. No tool use, no chain-of-thought, no agentic loop. The prompt is fixed at build time and lives in the client repo:

```
You are a query translator for the Travsr code graph. Given a natural-language
question about a codebase, return a JSON object with these fields:

- symbols:  array of 1-5 identifier fragments likely to appear in matching code.
            Use snake_case or camelCase as the codebase does. Prefer specific
            terms ("dispatch", "knapsack") over generic ones ("function", "code").
- paths:    array of 0-3 path globs ("crates/travsr-mcp/**", "*.ts").
- kinds:    array of 0-3 of: function, method, class, struct, enum, file, module.
- echo:     short paraphrase of the question for logs.

Respond with ONLY the JSON object, no prose, no markdown.

Question: {{userQuery}}
```

The translator uses `temperature=0` for best-effort stability. In practice, `temperature=0` does not guarantee bit-identical output across providers, model versions, or inference hardware (batching schedules, MoE routing, and quantization all introduce variance). Two identical queries *usually* produce the same `StructuredQuery` against a fixed model deployment, but this is not a hard guarantee. The **actual** end-to-end determinism guarantee is on the daemon side: identical structured inputs to `search_nodes_fuzzy` always produce identical outputs. L1 provides the true determinism floor; L2 is best-effort stable on top of it.

### 2.4 Client-side fan-out

```typescript
async function fuzzyQuery(nl: string): Promise<Node[]> {
  const sq = await translator.translate(nl);
  // The up-to-5 search_symbol calls are issued concurrently — this is the
  // user-facing latency path and sequential fan-out would multiply round-trip time.
  const results = await Promise.all(
    sq.symbols.map(sym => mcp.callTool("search_symbol", { name: sym }))
  );
  const seenIds = new Set<string>();
  const merged: Node[] = [];
  for (const nodes of results) {
    for (const n of nodes) {
      if (seenIds.has(n.id)) continue;
      if (sq.paths && !matchAny(n.path, sq.paths)) continue;
      if (sq.kinds && !sq.kinds.includes(n.kind)) continue;
      seenIds.add(n.id);
      merged.push(n);
    }
  }
  return merged.slice(0, 50);  // mirrors server-side cap
}
```

The client then feeds `merged` to `get_context` as the seed set. The server receives a list of node IDs the same way it would for any deterministic query.

### 2.5 Bypass / opt-out

Power users can bypass the translator by prefixing their query with `:` (colon). VS Code extension sees `:dispatch_tool_call` and skips translation, sending the literal string to `search_symbol`. This preserves the deterministic path for users who already know the exact symbol they want. Same convention applies in `travsr ask`.

### 2.6 Failure modes

- **LLM unavailable:** translator returns `{symbols: [nl], echo: nl}` - falls back to L1.
- **LLM returns malformed JSON:** same fallback, plus a `tracing::warn` log line.
- **LLM returns excessive fields:** silently truncated to the caps in 2.2.
- **LLM hallucinates non-existent terms:** harmless - the search returns zero nodes for those fragments and the union still picks up the real ones.

L2 is therefore a strict improvement on L1: at worst it degrades to L1, never below it.

### 2.7 Security

- Translator output passes through the existing `sanitize.rs` MCP arg validator (SEC-001/002): no `../`, no null bytes, no control chars, 256-byte arg cap.
- Prompt injection in `userQuery` cannot reach the daemon (translator output is the only thing forwarded, and it is a structured object). Worst case: attacker tricks the LLM into searching for noise symbols. The daemon side is unchanged.
- Echo field is logged but never forwarded over MCP.

---

## Detailed Design - L3: Embedding Sidecar

### 3.1 Build gating

L3 is **opt-in** behind `--features embedding`. Default builds do not include the dependency, do not include the model, do not change the binary size, and do not run any embedding code.

This is non-negotiable: the 25 MB tarball budget (ADR-004) does not accommodate a 120 MB model. L3 ships the same way the `kuzu` feature does today - separate build, separate distribution channel, downloadable on demand.

### 3.2 New crate: `travsr-embed`

A new workspace crate with a single responsibility:

```
crates/travsr-embed/
  Cargo.toml      // sqlite-vec, ort (ONNX runtime), once_cell
  src/lib.rs      // load model, embed(&str) -> [f32; 384], cosine_nn
  src/model.rs    // tokenizer + ONNX session
  src/sidecar.rs  // sqlite-vec virtual-table read/write
```

Allowed dependencies, by ADR-001 rule:
- `sqlite-vec` 0.x (single-crate, ~150 KB compiled)
- `ort` (ONNX runtime, ~5 MB compiled, statically linked)
- `tokenizers` (HuggingFace tokenizers, ~2 MB)

`travsr-embed` is referenced only by `travsr-store` and only behind the `embedding` feature flag.

### 3.3 Model

| Attribute | Value |
|---|---|
| Model | `bge-small-en-v1.5` (BAAI, MIT license) |
| Parameters | 33 M |
| Embedding dim | 384 |
| Quantization | int8 (binary-safe via ONNX) |
| Quantized size | ~33 MB |
| FP16 fallback | ~66 MB |
| Tokenizer | Sentencepiece, bundled |
| Architecture | BERT (encoder-only, no decoder, no generation) |

Choice rationale: smallest model that benchmarks within 5 points of `bge-base` on the BEIR code-search subset. `bge-small` runs on CPU at ~12 ms / query on M1, ~30 ms on x86_64. Stays inside the p95 < 50 ms latency budget.

### 3.4 Model distribution

Modeled on the `kuzu` feature install:

```bash
cargo build --release --features embedding
# Binary built. Model not yet present.

travsr embed install
# Downloads bge-small-en-v1.5.onnx.int8 (~33 MB) from
# https://github.com/Travsr-com/travsr-models/releases/download/bge-small-v1.5-int8
# Verified by SHA-256 against the version compiled into the binary.
# Cached at ~/.travsr/models/bge-small-en-v1.5.int8.onnx.
```

Model URLs and SHA-256 are compile-time constants in `travsr-embed/src/model.rs`. Different binary versions cannot accidentally load incompatible models.

### 3.5 Schema (migration v9, embedding feature only)

```sql
-- Only created if the daemon was built with `--features embedding`.
-- Default builds never see this migration.
CREATE VIRTUAL TABLE IF NOT EXISTS nodes_vec USING vec0(
    node_rowid INTEGER PRIMARY KEY,
    embedding FLOAT[384]
);
```

Same `nodes_fts_map.rowid` is reused as `node_rowid` so L1 and L3 share the row-identity mapping. If a node has an FTS entry it can have a vector entry; if it has a vector entry it always has an FTS entry. This invariant is enforced by the `put_node_embed` write being conditional on a successful `put_node_fts`.

### 3.6 Indexing payload

```rust
// What we embed for each node:
fn embed_payload(node: &Node, docstring_first_line: Option<&str>) -> String {
    format!(
        "{} {} {} {}",
        node.kind,
        node.vname.signature,
        node.vname.language,
        docstring_first_line.unwrap_or("")
    )
}
```

Never embeds function bodies. The graph already captures structural relationships; bodies would balloon the index, leak code into the vector space, and tempt future contributors to use embeddings for semantic retrieval (which is L3's explicit non-goal - it picks seeds, nothing else).

### 3.7 Query path

```rust
fn search_nodes_fuzzy_l3(query: &str) -> Result<Vec<Node>, StoreError> {
    let embedding = travsr_embed::embed(query)?;  // ~12-30 ms
    let rows = conn.query_row(
        // LIMIT 10: L3 fetches the top-10 nearest neighbours by cosine distance.
        // These are merged into the shared 50-node cap of search_nodes_fuzzy, not
        // added on top of it. When L3 fires, L1 has already returned 0 results, so
        // the effective seed K passed to PPR is at most 10 from L3 alone — well
        // within the 50-seed bound in ADR-003. The 50-node cap in search_nodes_fuzzy
        // is the unified ceiling across all layers.
        "SELECT node_rowid, distance FROM nodes_vec
         WHERE embedding MATCH ? ORDER BY distance LIMIT 10",
        params![embedding.as_bytes()],
        ...
    )?;
    // Map rowids back to nodes via nodes_fts_map.
}
```

Result: top-10 nodes by cosine distance, capped under the existing 50-seed limit. These become PPR seeds exactly the same way L1 results do.

### 3.8 Cost model

| Resource | Default build | `--features embedding` |
|---|---|---|
| Binary size | 8 MB | 16 MB (+8 MB for ort + tokenizers + sqlite-vec) |
| Disk (graph.db) | ~5% | ~5% + ~1.5 KB/node embedding (~3.9 MB on Travsr-self-index) |
| Cold model load | n/a | ~300 ms first query |
| Per-query latency | ~0.5 ms p50 | +12-30 ms for query embedding |
| User download | one `travsr install` | + `travsr embed install` (33 MB one-time) |

Three explicit guards in CI to keep these costs from regressing:
1. Default `travsr-cli` build produces a tarball <= 25 MB (ADR-004 enforced as today).
2. `--features embedding` build produces a tarball <= 50 MB.
3. Model download URL is reachable; SHA-256 matches the compile-time constant.

### 3.9 When L3 fires

L3 is **only consulted on L1 miss.** A query that already has a good substring or FTS match never invokes embedding. This keeps the average-case latency at L1's ~0.5 ms while still providing a semantic escape valve for the queries L1 cannot answer.

### 3.10 Determinism semantics

The vector index is deterministic given identical query strings and identical model bytes. Two daemons built from the same commit with the same feature flags will return identical L3 results for identical queries. We treat the model binary as part of the daemon version; a model upgrade requires a graph reindex.

---

## Why Not Just Embed Everything

Three reasons, in order of importance:

1. **Manifesto.** "Algorithms first, LLM last" is the project's identity. Bolting on embeddings as the primary search step inverts the order and makes Travsr indistinguishable from vector RAG (the thing it was built to replace) at the boundary the user actually touches.
2. **Cost.** L1 ships in ~400 LOC and adds ~200 KB to the database. L3 adds 8 MB to the binary, 33 MB to user disk via a download, and 12-30 ms per query. Most queries don't need it.
3. **Determinism erosion.** Once a vector layer is on the default path, every subsequent retrieval RFC will be tempted to lean on it. The layered approach forces every future contributor to first prove L1 + L2 are insufficient before reaching for embeddings.

---

## Alternatives Considered

### A1. Skip L1, jump straight to embeddings
Rejected. Solves the symptom (`dispatch_tool_call` query fails) but commits the project to the wrong default. Also adds an 8 MB binary cost and 33 MB user download for a query class that L1 handles correctly.

### A2. `SQL LIKE` with tokenized wildcards
Tried in prototype. Splitting `"mcp dispatch tool"` into three `LIKE` clauses ANDed together works for the demo case but scales O(n x terms) and has no relevance ranking. FTS5 trigram is a strict superset for ~150 lines more code and gives BM25 scoring for free.

### A3. External search service (Tantivy, Meilisearch)
Rejected. New process, new ops burden, breaks the single-binary install promise. SQLite FTS5 is already linked; using it costs zero ops.

### A4. LLM translator on the server
Rejected. Pulling an LLM dependency into `travsr-mcp` would violate the "daemon stays LLM-free" architectural rule (CLAUDE.md "MCP is the only external interface"). Translator lives in the client. Daemon receives only structured input.

### A5. Synonym expansion in the indexer (`db` -> `database`, `req` -> `request`)
Rejected for v1. Maintenance hazard, locale-specific, and L1's trigram + camel-split plus L2 translation already handle the common case. Revisit if telemetry shows real synonym misses after L2 ships.

### A6. Larger embedding model (`bge-base`, ~440 MB)
Rejected. Marginal recall improvement (~3 points BEIR) not worth the 4x storage cost. `bge-small` is the right Pareto point for an opt-in feature.

---

## Migration / Compatibility

- **Schema:** migration v8 (L1, default) and v9 (L3, embedding feature only) are forward-only. Old daemons reading newer databases are protected by the existing schema-version gate.
- **API:** `search_symbol` and `get_context` JSON schemas are unchanged across all three layers. The `name` and `query` argument semantics broaden (any query that worked before still works); no client code must change.
- **Determinism:** unchanged on identical inputs at each layer. L1 ranking is deterministic (FTS5 BM25). L2 is best-effort stable given a fixed model deployment (`temperature=0` reduces variance but does not guarantee bit-identical output across providers or hardware — see §2.3); the true determinism floor is the daemon receiving identical structured inputs. L3 is deterministic given a fixed model binary.
- **Performance:** L1 adds ~0.5 ms p50, ~2 ms p99 - within the existing p95 < 50 ms budget by 25x. L2 adds one LLM round-trip on the client side (not counted against server budget). L3 adds ~12-30 ms only on L1 miss.

---

## Open Questions

1. **Should `search_symbol` expose the fuzzy fallback explicitly?** Option A: tools.rs hides the layering and clients see one tool that "just works." Option B: add a `fuzzy: bool` argument so deterministic clients can opt out. Lean: A - the tool's job is "find symbols by name"; whether it uses substring, FTS, or vectors underneath is an implementation detail.
2. **Non-ASCII identifier tokenization.** `tokenize_identifier` splits ASCII case boundaries only. CJK / Cyrillic identifiers emit as one trigram blob. Acceptable for v1; flag in QA-014.
3. **FTS / vec index freshness on schema migrations v10+.** If a future migration alters `nodes.signature`, the FTS and vec tables can drift. Solve via a checksum row in `nodes_fts_map` that triggers a rebuild on mismatch. Defer to L1 implementation. **Note:** per-commit incremental reindex (the common case) is addressed in §1.3 via `delete_node_fts` / `put_node_fts`; the delete/retract path stores `tokens` in `nodes_fts_map` precisely to make contentless-FTS retraction correct. This open question covers only the forward-schema-migration drift case, not incremental reindex.
4. **Telemetry for layer hit rate.** Need a metric for "L1 hit", "L1 miss -> L2 used", "L1+L2 miss -> L3 used", "all-miss -> empty." Reuse the existing `tracing` span counters; expose via the metrics endpoint added in RFC-007. Drives the decision of whether L3 ever leaves opt-in.
5. **Model attestation for L3.** Should the model SHA-256 be signed alongside the binary via cosign (mirroring release artifact signing)? Lean: yes, because a swapped model silently changes retrieval behavior in a way users cannot detect. Defer to Phase 5 security review.
6. **L2 model choice.** The translator prompt is fixed but the model is not. Default = whatever the MCP client already has access to (Claude, GPT, local). Should the prompt be tuned per-model? Lean: no - keep it portable, accept ~5% recall variance across models.

---

## Sprint Shape

All three layers are in this RFC; they ship across three sprints with separate PRs.

| Sprint | Scope | Crate impact | Default-on? |
|---|---|---|---|
| **S19 (L1)** | Migration v8, tokenizer, `search_nodes_fuzzy`, MCP wiring, >=10 fuzzy tests, dogfooding-doc benchmark update | `travsr-store` (+400 LOC), `travsr-mcp` (+30 LOC), `travsr-cli` (+5 LOC) | Yes |
| **S20 (L2)** | Translator in `travsr-vscode` + reference impl in `travsr-npm`, `:` bypass syntax in VS Code + `travsr ask`, prompt-injection test suite | `travsr-vscode` (+200 LOC), `travsr-npm` (+150 LOC), docs only on server | Yes (client-side) |
| **S21 (L3)** | `travsr-embed` crate, migration v9, `travsr embed install`, `--features embedding` gating, CI size guards | new `travsr-embed` crate (~800 LOC), `travsr-store` (+100 LOC behind feature flag) | No (opt-in) |

S19 and S20 ship in Phase 4. S21 ships in Phase 5 alongside cloud preview - by then telemetry from L1+L2 will tell us whether L3 is worth building at all.

---

## Acceptance Criteria

### L1 (S19)
- [ ] Migration v8 is idempotent (re-running on a v8 db is a no-op).
- [ ] `tokenize_identifier` handles snake_case, CamelCase, kebab-case, `:`, `/`, `.`, digits.
- [ ] `search_nodes_fuzzy("mcp dispatch tool call")` returns `fn:dispatch_tool_call` as the top result on the Travsr-self-index.
- [ ] Existing `search_nodes_by_name` exact-substring queries return identical results - no regression in `QA-012` MCP conformance suite.
- [ ] p95 latency for `get_context` on the 2.6k-node Travsr-self-index remains < 50 ms.
- [ ] FTS index adds < 5% to `graph.db` size on the Travsr-self-index.
- [ ] `dogfooding.md` benchmark table updated with the natural-language query class.

### L2 (S20)
- [ ] `queryTranslator.ts` ships in `travsr-vscode`; `translator.js` reference ships in `travsr-npm`.
- [ ] Translator falls back to L1 cleanly on LLM unavailability, malformed output, or transient errors (covered by unit tests).
- [ ] `:` bypass syntax skips translation and sends literal query to `search_symbol`.
- [ ] Translator output passes through `sanitize.rs` validation.
- [ ] At least 5 natural-language test queries documented in `dogfooding.md` end-to-end (VS Code -> daemon -> graph result).
- [ ] No new dependencies added to `travsr-mcp` crate.

### L3 (S21)
- [ ] Default `cargo build --release` does not pull `travsr-embed` or any L3 dependency.
- [ ] `cargo build --release --features embedding` produces a tarball <= 50 MB.
- [ ] `travsr embed install` downloads the model, verifies SHA-256, caches at `~/.travsr/models/`.
- [ ] L3 query is invoked only on L1 miss (verified by tracing-span counter assertions in test).
- [ ] Embedding migration v9 is idempotent and only created in `--features embedding` builds.
- [ ] L3 added latency does not exceed 50 ms p95 on the Travsr-self-index.
- [ ] CI gate: default-build tarball size cannot exceed 25 MB (existing ADR-004 enforcement extended to cover L3 leakage).

---

## References

- ADR-001 - coding standards (allowed deps)
- ADR-003 - PPR policy (seed-set semantics)
- ADR-004 - error taxonomy + tarball budget
- RFC-004 - MCP tool JSON schemas
- RFC-007 - MCP SSE transport (metrics endpoint)
- RFC-010 - knapsack token-budget enforcer (downstream consumer)
- SQLite FTS5 docs - https://www.sqlite.org/fts5.html (trigram tokenizer §4.3.5)
- `sqlite-vec` - https://github.com/asg017/sqlite-vec
- `bge-small-en-v1.5` - https://huggingface.co/BAAI/bge-small-en-v1.5
- BLAKE3 `NodeId` rationale - `crates/travsr-core/src/lib.rs` (rowid mapping context)
