# RFC-012 Addendum 02 — Dynamic Synonym Table + L2-B/L2-D + VS Code Auto-Context

**Status:** Draft  
**Author:** Abhishek  
**Date:** 2026-06-02  
**Parent RFC:** RFC-012 + RFC-012-ADDENDUM-01  
**Crate(s) affected:** `travsr-store`, `travsr-cli`, `travsr-mcp`, `travsr-vscode`  
**Sprint target:** S21  
**Revision:** Rev 2

---

## Summary

Four independent but co-landing features that complete the RFC-012 L2 ladder
and make Travsr ambient infrastructure rather than an explicit tool:

1. **Dynamic synonym table** (`fts_synonyms`) — moves the T0 static synonym map
   from a compile-time constant into a per-repo SQLite table, eliminating false
   positives caused by universal synonym assumptions and making corrections
   instantaneous without a recompile.

2. **L2-B: local ONNX embeddings** — opt-in semantic bridge using MRL-256 +
   RaBitQ 1-bit quantisation stored in a `vec0` virtual table (migration v12).
   Only downloaded when the user explicitly enables the feature.

3. **L2-D: MCP sampling borrow** — near-vestigial; exposes a `prompts/get`
   endpoint that lets a capable host LLM contribute a rewritten query to the
   search path. Default-off, gated by security review.

4. **VS Code auto-context provider** — registers a `ChatContextProvider` that
   silently injects graph context into every Copilot Chat turn. The user writes
   a question; Travsr detects what they are asking and looking at, queries the
   graph automatically, and feeds the result to the host AI. Zero explicit
   invocation required.

---

## Motivation

### Why static synonyms are dangerous

The current T0 synonym table (`seed_lexicon.rs`) makes universal assumptions:

```rust
("store",   &["database", "sqlite", "storage"]),
("auth",    &["rbac", "session", "login", "token"]),
```

These are correct for the Travsr codebase. In a retail application `"store"`
means a physical shop, not a database. In a game engine `"auth"` may not
appear at all.

**Observed risk:** A query like `travsr ask "store order item"` expands
`"store"` to `"database"+"sqlite"+"storage"`, returning SqliteStore nodes
instead of order-related nodes. Step 1 (exact LIKE) prevents regressions for
direct symbol queries, but NL queries are affected.

### The fix must be algorithms-first

We must not use an LLM to decide synonyms. The correct approach: move the
synonym table to SQLite (`fts_synonyms`), seed it from the existing static
defaults at `travsr init`, and expose a CLI for per-repo corrections. Changes
take effect on the next query — no recompile, no restart.

---

## Migration Plan

```
v10  fts_vocab          (LANDED — RFC-012 A1)
v11  fts_synonyms       (this RFC — dynamic synonym table)
v12  vec0 / embedding   (this RFC — L2-B opt-in)
```

> **Note:** v11 was previously unassigned (L2-B was spec'd as v11 in Addendum 01
> but not implemented). Dynamic synonyms take v11; vec0 moves to v12.
> The parent RFC-012 §3.5 vec0 reference must be updated to v12 before merge.

---

## Feature 1 — Dynamic Synonym Table (v11)

### Schema (migration v11)

```sql
-- Migration v11: per-repo synonym table for RFC-012 A2 dynamic T0 floor.
--
-- fts_synonyms replaces the compile-time SYNONYMS static in seed_lexicon.rs.
-- Seeded from those defaults at `travsr init`; modified at runtime via
-- `travsr synonym add/remove`.  Reads are at query time (hot path) so the
-- table must be small: soft cap 200 rows, hard cap enforced on insert.
--
-- Direction: term → alias (one-directional, same as static table).
-- "auth" → "session" does NOT imply "session" → "auth".
-- Add the reverse explicitly if needed.

CREATE TABLE IF NOT EXISTS fts_synonyms (
    term     TEXT NOT NULL,
    alias    TEXT NOT NULL,
    PRIMARY KEY (term, alias)
);

-- Optional index for term lookup — already covered by the PK prefix scan
-- but explicit for clarity on the hot path.
CREATE INDEX IF NOT EXISTS fts_synonyms_term_idx ON fts_synonyms(term);
```

### Seeding at `travsr init`

`travsr init` already opens the store and runs migrations. After v11 runs,
seed the defaults:

```rust
// In SqliteStore::open() / open_in_memory(), after backfill_vocab_if_needed:
store.seed_synonyms_if_empty()?;
```

```rust
// In the FTS impl block of lib.rs:
fn seed_synonyms_if_empty(&mut self) -> AnyResult<()> {
    let count: i64 = self.conn
        .query_row("SELECT COUNT(*) FROM fts_synonyms", [], |r| r.get(0))
        .context("counting fts_synonyms")?;
    if count > 0 {
        return Ok(());
    }
    // Seed from the compile-time defaults (still kept as fallback reference).
    let tx = self.conn.transaction()?;
    for (term, aliases) in crate::seed_lexicon::SYNONYMS {
        for alias in *aliases {
            tx.execute(
                "INSERT OR IGNORE INTO fts_synonyms(term, alias) VALUES(?1, ?2)",
                params![term, alias],
            )?;
        }
    }
    tx.commit()?;
    tracing::info!("RFC-012 A2: seeded fts_synonyms from static defaults");
    Ok(())
}
```

### Query-time lookup

Replace the `synonyms_for(term)` call in `expand_tokens` with a DB-backed
version. `expand_tokens` is currently pure (no DB), so we need a new
entrypoint `expand_tokens_db` that takes `&Connection`:

```rust
// seed_lexicon.rs — new companion to expand_tokens
pub(crate) fn expand_tokens_db(
    conn: &Connection,
    raw: &[String],
) -> AnyResult<Vec<String>> {
    let content: Vec<&String> = raw
        .iter()
        .filter(|t| !is_stopword(t.as_str()))
        .collect();

    let mut out: Vec<String> = Vec::new();
    let push = |v: &mut Vec<String>, s: &str| {
        if s.len() >= 3 && !v.iter().any(|e| e == s) {
            v.push(s.to_string());
        }
    };

    // PA C1: raw content tokens always enter first.
    for t in &content {
        push(&mut out, t.as_str());
    }
    // DB-backed aliases.
    for t in &content {
        let mut stmt = conn.prepare_cached(
            "SELECT alias FROM fts_synonyms WHERE term = ?1",
        )?;
        let aliases: Vec<String> = stmt
            .query_map(params![t.as_str()], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        for alias in aliases {
            push(&mut out, &alias);
        }
    }
    // PA C1 safety floor.
    if out.is_empty() {
        for t in raw {
            push(&mut out, t.as_str());
        }
    }
    Ok(out)
}
```

`build_fuzzy_match_expr` in `fts_tokenize.rs` currently calls the pure
`expand_tokens`. We need a new variant `build_fuzzy_match_expr_db` that takes
`&Connection` and calls `expand_tokens_db`. The caller in `search_nodes_fuzzy`
(Step 2) switches to the DB variant:

```rust
// search_nodes_fuzzy Step 2 — replace build_fuzzy_match_expr(query) with:
let step2_expr = match build_fuzzy_match_expr_db(query, &self.conn)? {
    Some(e) => e,
    None => { ... return Ok(Vec::new()); }
};
```

Keep `build_fuzzy_match_expr` (pure, no DB) for unit tests and for
`seed_lexicon` unit tests which must remain dependency-free.

### CLI: `travsr synonym` subcommand

Add to `travsr-cli/src/main.rs` under the existing subcommand dispatch:

```
travsr synonym add   <term> <alias>   # add a synonym pair
travsr synonym remove <term> <alias>  # remove a synonym pair
travsr synonym list                   # list all active synonyms
travsr synonym reset                  # reset to static defaults
```

Each command opens the repo's `graph.db` and modifies `fts_synonyms` directly.
Changes are reflected on the next `travsr ask` or MCP `search_symbol` call.

**Hard cap on insert:** reject if `COUNT(*) >= 200` to prevent runaway tables.

### Acceptance criteria

- [ ] `travsr synonym add foo bar` → subsequent `travsr ask "foo"` expands to include "bar"
- [ ] `travsr synonym remove store sqlite` → `travsr ask "store model"` no longer returns SQLite nodes
- [ ] `travsr synonym reset` → restores the 8 static defaults
- [ ] Fresh `travsr init` → `fts_synonyms` seeded with 8 × N default rows (N = alias count per term)
- [ ] `fts_synonyms` count > 200 → insert rejected with clear error message
- [ ] `expand_tokens` (pure) still compiles and passes all existing unit tests unchanged
- [ ] `expand_tokens_db` passes the same unit tests via a DB-seeded fixture
- [ ] Migration v11 is idempotent (`CREATE TABLE IF NOT EXISTS`)
- [ ] PA C1 invariant preserved: raw content tokens always in union regardless of DB aliases
- [ ] `build_fuzzy_match_expr` (pure) retained; `build_fuzzy_match_expr_db` is the new hot path

---

## Feature 2 — L2-B: Local ONNX Embeddings (migration v12, opt-in)

### What it is

An optional semantic bridge for queries that pass through Steps 1–3 without
results. Uses a local ONNX model (MRL-256 truncated from `nomic-embed-text`,
~100 MB download) + RaBitQ 1-bit quantisation stored in a `vec0` virtual table.
Activated by `--feature embeddings` and an explicit `travsr embed init`.

### Why it is opt-in and not default

- **Binary size**: ONNX runtime adds ~30 MB to the release binary
- **Download cost**: model weights are ~100 MB, separate from the binary
- **Privacy**: even a local model has a different privacy profile than pure
  graph traversal; users must opt in explicitly
- **TL2 gate**: CI already enforces zero embedding deps in the default config

### Schema (migration v12)

```sql
-- Migration v12: vec0 embedding table for RFC-012 A1 L2-B (opt-in).
-- Only created when the `embeddings` feature flag is enabled.
-- Stores MRL-256 + RaBitQ 1-bit compressed vectors per node.
--
-- ADR-004: separate download budget — the model weights are NOT bundled
-- in the binary.  `travsr embed init` downloads them on first use.
--
-- vec0 is provided by the sqlite-vec extension (LGPL-2.1).
-- The extension is loaded lazily; this migration is a no-op if vec0 is
-- not available (CREATE VIRTUAL TABLE IF NOT EXISTS handles this).

CREATE VIRTUAL TABLE IF NOT EXISTS node_embeddings USING vec0(
    node_id INTEGER PRIMARY KEY,
    embedding FLOAT[256]         -- MRL-256 truncated vector
);
```

> **Important:** The RaBitQ rotation seed MUST be pinned and stored in the
> `meta` table (key: `rabitq_rotation_seed`, value: hex-encoded 32-byte seed)
> at `travsr embed init` time. Two stores indexed independently with the same
> model but different rotation seeds will produce incompatible Hamming
> distances. This was the BLOCKER from Addendum 01 Rev 2.

```rust
// travsr-store: after generating the rotation matrix at embed init
store.set_meta("rabitq_rotation_seed", &hex::encode(seed))?;
```

### L2-B query path

Slot in `search_nodes_fuzzy` as Step 4 (after L2-A):

```
Step 4 — L2-B (opt-in, feature-gated):
  embed query → query vector
  SELECT node_id FROM node_embeddings
    WHERE embedding MATCH ?1 AND k = 20    ← ANN via vec0 KNN
  Hamming re-rank top-20 → return
```

The `#[cfg(feature = "embeddings")]` gate ensures this code path does not
compile in the default build.

### Maintenance hooks

`put_node_fts` must also embed the node when the feature is enabled:

```rust
#[cfg(feature = "embeddings")]
Self::put_node_embedding(conn, node)?;
```

`delete_node_fts`, `delete_nodes_for_path`, `delete_nodes_for_path_prefix`
must delete from `node_embeddings` for the same nodes.

### Acceptance criteria

- [ ] Default `cargo build --release` (no `--features`) pulls zero embedding deps (TL2 gate)
- [ ] `cargo build --release --features embeddings` compiles without error
- [ ] `travsr embed init` downloads model, pins rotation seed in meta, indexes existing nodes
- [ ] `travsr embed init` on an already-embedded store is idempotent (seed unchanged)
- [ ] `search_nodes_fuzzy` with `--features embeddings`: Step 4 fires only on Steps 1–3 combined miss
- [ ] Hamming distances are reproducible across two stores with the same pinned seed (AC j from A1)
- [ ] Migration v12 is idempotent; a store without vec0 extension falls back gracefully
- [ ] `node_embeddings` row count == `nodes` row count after full index (parity check)
- [ ] Deleting a node via any path removes its embedding row

---

## Feature 3 — L2-D: MCP Sampling Borrow (default-off)

### What it is

An optional MCP `prompts/get` endpoint that, when called by a host LLM that
supports MCP sampling, contributes a rewritten query string to the search path.

The daemon does NOT call the LLM. The LLM is the MCP client; it calls
`prompts/get` with a prompt template, rewrites the query itself, and then
calls `tools/call` with the rewritten query. The daemon is purely passive.

### Why it is near-vestigial

For any LLM-capable MCP client (Claude Desktop, Claude Code), the host already
translates NL queries before calling `search_symbol` or `get_context`. L2-D
adds a formal `prompts/get` endpoint for clients that want to use the daemon's
own prompt template rather than their own.

Most users will never need this. It is default-off and only relevant if a
custom MCP client wants to reuse the search prompt.

### Implementation

Add to `server.rs`:

```rust
"prompts/list" => ok_response(id, prompts_list()),
"prompts/get"  => {
    let name = req.params
        .as_ref()
        .and_then(|p| p["name"].as_str())
        .unwrap_or("");
    handle_prompts_get(id, name)
}
```

```rust
fn prompts_list() -> serde_json::Value {
    serde_json::json!({
        "prompts": [{
            "name": "search_query_rewrite",
            "description": "Rewrite a natural-language query into a concise \
                            symbol-oriented search term for Travsr's graph index.",
            "arguments": [{
                "name": "query",
                "description": "The original user query",
                "required": true
            }]
        }]
    })
}

fn handle_prompts_get(id: serde_json::Value, name: &str) -> String {
    match name {
        "search_query_rewrite" => ok_response(id, serde_json::json!({
            "description": "Rewrite this query for Travsr symbol search",
            "messages": [{
                "role": "user",
                "content": {
                    "type": "text",
                    "text": "Rewrite the following query into a short (1–4 word) \
                             code-symbol search term that would appear in function \
                             names, class names, or file names:\n\n{{query}}"
                }
            }]
        })),
        _ => error_response(id, INVALID_PARAMS, format!("unknown prompt: {name}")),
    }
}
```

### Security gate

`prompts/get` must be reviewed before enabling in production:
- The prompt template must not accept user-controlled text that reaches the
  daemon's file system or SQL layer
- The `{{query}}` substitution is client-side; the daemon only returns the
  template string, never executes it
- The endpoint is behind a feature flag (`--feature mcp-sampling`) until
  the security review passes

### Acceptance criteria

- [ ] `prompts/list` returns the `search_query_rewrite` prompt entry
- [ ] `prompts/get` with `name=search_query_rewrite` returns the template with `{{query}}` placeholder
- [ ] `prompts/get` with unknown name returns MCP error code `INVALID_PARAMS`
- [ ] Default build (no feature flag) does NOT expose `prompts/list` or `prompts/get`
- [ ] Security review sign-off before enabling in any cloud deployment

---

## Feature 4 — VS Code Auto-Context Provider (ambient, always-on)

### The problem with explicit tool invocation

Every other approach in this RFC still requires the user or AI to **ask**
Travsr for context. The user has to type `#search_symbol`, the AI has to
decide to call a tool, or the developer has to run `travsr ask`. This is a
pull model — Travsr waits to be called.

The actual thesis of Travsr is:

> *"The code graph that lives next to git."*

Git does not wait to be asked. It tracks everything continuously. Travsr
should behave the same way: every AI conversation in the IDE should
automatically have the right graph context, without the developer ever
thinking about it.

### Mechanism: `vscode.chat.registerChatContextProvider`

VS Code 1.99+ exposes a `ChatContextProvider` API that runs **before every
Copilot Chat response**. The provider receives the conversation history and
returns additional context items that are silently prepended to the AI's
context window.

This is the correct primitive: it is push-based (Travsr decides when to
inject), invisible to the user, and zero-friction.

### Query construction

Two signals are combined to build the graph query:

```
query = (symbol under cursor OR active file name) + " " + last user message
```

- **Symbol under cursor** — what the developer is actively looking at. Uses
  `vscode.window.activeTextEditor` + `vscode.commands.executeCommand(
  "vscode.executeDocumentSymbolProvider")` to get the nearest symbol at
  the cursor position.
- **Last user message** — what they are asking about. Extracted from
  `context.messages`.
- **Fallback** — if no editor is open, use the last user message alone.

This combined query is passed to `get_context` via the existing
`StdioMcpClient`. T0 + L2-A normalises it in the daemon — no extra work
needed in the extension.

### Implementation

**`packages/travsr-vscode/package.json`** — three changes:

```json
{
  "engines": { "vscode": "^1.99.0" },
  "contributes": {
    "chatParticipants": [{
      "id": "travsr.context",
      "name": "travsr",
      "description": "Always-on code graph context for Copilot Chat",
      "isSticky": false
    }],
    "configuration": {
      "properties": {
        "travsr.contextTokenBudget": {
          "type": "number",
          "default": 2000,
          "minimum": 500,
          "maximum": 8000,
          "description": "Token budget for automatic context injection per chat turn"
        },
        "travsr.autoContextEnabled": {
          "type": "boolean",
          "default": true,
          "description": "Inject graph context automatically into every Copilot Chat turn"
        }
      }
    }
  }
}
```

**`packages/travsr-vscode/src/contextProvider.ts`** — new file (~70 LOC):

```typescript
import * as vscode from "vscode";
import type { McpClient } from "./mcp";

function getSymbolAtCursor(editor: vscode.TextEditor): string {
  const pos = editor.selection.active;
  const range = editor.document.getWordRangeAtPosition(pos, /[\w:.<>]+/);
  return range ? editor.document.getText(range) : "";
}

function lastUserMessage(
  messages: readonly vscode.LanguageModelChatMessage[]
): string {
  for (let i = messages.length - 1; i >= 0; i--) {
    if (messages[i].role === vscode.LanguageModelChatMessageRole.User) {
      const c = messages[i].content;
      if (typeof c === "string") return c;
      if (Array.isArray(c)) {
        return c
          .filter((p: unknown) => typeof (p as { value?: string }).value === "string")
          .map((p: unknown) => (p as { value: string }).value)
          .join(" ");
      }
    }
  }
  return "";
}

export function registerContextProvider(
  client: McpClient,
  context: vscode.ExtensionContext
): void {
  const provider = vscode.chat.registerChatContextProvider(
    "travsr.context",
    {
      async provideContext(
        ctx: vscode.ChatContextProviderContext,
        _token: vscode.CancellationToken
      ): Promise<vscode.ChatContext[]> {
        const cfg = vscode.workspace.getConfiguration("travsr");
        if (!cfg.get<boolean>("autoContextEnabled", true)) return [];
        if (!client.isConnected()) return [];

        const userMsg = lastUserMessage(ctx.messages);
        if (!userMsg.trim()) return [];

        const editor = vscode.window.activeTextEditor;
        const symbol = editor ? getSymbolAtCursor(editor) : "";
        const filePart = editor
          ? vscode.workspace.asRelativePath(editor.document.uri)
          : "";
        const anchor = symbol || filePart;
        const query = anchor ? `${anchor} ${userMsg}` : userMsg;

        const budget = cfg.get<number>("contextTokenBudget", 2000);
        const result = await client.callTool("get_context", {
          query,
          token_budget: String(budget),
        });

        if (!result || result.startsWith("No symbols")) return [];

        return [{
          value: result,
          description: "Code graph context — Travsr",
        }];
      },
    }
  );
  context.subscriptions.push(provider);
}
```

**`packages/travsr-vscode/src/extension.ts`** — one addition after
`client.connect()`:

```typescript
import { registerContextProvider } from "./contextProvider";

// After: await client.connect();
registerContextProvider(client, context);
```

### What the user experience becomes

No change required from the developer. They open a file, ask Copilot a
question, and Travsr context is silently present:

```
Developer: cursor on fn:charge() in payment.ts
Developer types in Copilot Chat: "why does this fail for large amounts?"

Behind the scenes (invisible to user):
  contextProvider fires automatically
  query = "fn:charge why does this fail for large amounts?"
  → get_context returns: fn:charge, fn:validate_amount, fn:stripe_charge,
    struct:PaymentError — graph-adjacent nodes within 2000 tokens
  → injected silently into Copilot's context window before it responds

Copilot answers: "Looking at fn:validate_amount, amounts over 999999
are rejected by the Stripe ceiling check on line 47..."
```

The developer never typed `#search_symbol`. They never mentioned Travsr.
The AI simply had the right context.

### What happens when Travsr is not running

`client.isConnected()` returns `false` → the provider returns `[]`
immediately → Copilot Chat works exactly as it did before. Zero degradation.

### Token budget guardrail

The `contextTokenBudget` setting (default 2000) caps how much of Copilot's
context window Travsr can consume. At 2000 tokens, a typical graph context
is 8–15 nodes — enough to answer most questions without crowding out the
conversation history.

The user can tune this via VS Code settings. Setting it to 0 or disabling
`autoContextEnabled` turns off injection entirely.

### Acceptance criteria

- [ ] Opening a TS/Rust file with Copilot Chat active → graph context appears
  in Copilot's response without any explicit user invocation
- [ ] `travsr.autoContextEnabled: false` → no context injected, Copilot
  behaves as if Travsr does not exist
- [ ] Travsr daemon not running → `isConnected() === false` → provider
  returns `[]` → Copilot Chat unaffected
- [ ] Symbol under cursor is included in the query when editor is active
- [ ] No symbol under cursor → query falls back to user message only
- [ ] `contextTokenBudget` respected: result never exceeds the configured
  budget (enforced by `get_context` knapsack in the daemon)
- [ ] Provider does not block the chat response: `callTool` timeout is 10 s
  (already enforced in `StdioMcpClient`); timed-out calls resolve to `""`
  → provider returns `[]` → no hang
- [ ] Works with both single-repo mode (`travsr mcp`) and global mode
  (`travsr mcp --global`)
- [ ] Engine requirement bumped to `^1.99.0` in `package.json`

---

## Implementation Order

F4 is the highest user-impact item and has no dependencies on F1–F3. It
should land first so the always-on context layer is available to users even
before dynamic synonyms or embeddings ship.

```
── F4 (VS Code auto-context) ─────────────────────────────── independent ──
1.  contextProvider.ts — new file
2.  extension.ts — registerContextProvider call after client.connect()
3.  package.json — engine bump + contributes (chatParticipants + config)
4.  Tests: context injected on chat turn, isConnected=false → empty,
    autoContextEnabled=false → empty, timeout → empty
    ─────────────────────────────────────────────────────────────────────
── F1 (dynamic synonyms) ─────────────────────────────────── depends on nothing ──
5.  Migration v11 + fts_synonyms table
6.  seed_synonyms_if_empty() + open() / open_in_memory() wiring
7.  expand_tokens_db() + build_fuzzy_match_expr_db()
8.  search_nodes_fuzzy Step 2 switched to DB variant
9.  travsr synonym subcommand (add / remove / list / reset)
10. Tests: synonym CRUD + PA C1 invariant with DB backend
    ─────────────────────────────────────────────────────────────────────
── F2 + F3 (embeddings + sampling) ──────────────── parallel after step 10 ──
11. Migration v12 (feature-gated) + vec0 schema
12. travsr embed init + rotation seed pinning
13. put_node_fts embedding hook (feature-gated)
14. delete hooks for node_embeddings
15. search_nodes_fuzzy Step 4 (feature-gated)
16. Tests: embedding round-trip + Hamming reproducibility + idempotency
    ─────────────────────────────────────────────────────────────────────
17. prompts_list / handle_prompts_get in server.rs (feature-gated)
18. Security review
19. Tests: prompts/list, prompts/get, unknown-prompt error
```

F1, F2, F3, F4 can be developed in parallel; only migration ordering
(v11 before v12) must be preserved at merge time.

---

## Effort Estimate

| Feature | LOC | Complexity | Crate(s) |
|---|---|---|---|
| VS Code auto-context (F4) | ~70 | XS | `travsr-vscode` |
| Dynamic synonyms (F1) | ~300 | M | `travsr-store`, `travsr-cli` |
| L2-B ONNX embeddings (F2) | ~450 | L | `travsr-store`, `travsr-cli` |
| L2-D MCP sampling (F3) | ~80 | XS | `travsr-mcp` |
| **Total** | **~900** | **L** | |

F4 is XS but highest impact — ships first, independent of all other work.

---

## Open Questions

1. **F4 context budget**: default 2000 tokens — is this right? At 2000 tokens
   the context provider consumes ~15% of a 4096-token chat turn. Too high risks
   crowding out conversation history; too low misses multi-hop graph neighbours.
   Measure empirically against 5 canonical queries before hardcoding the default.

2. **F4 query construction — weighting**: should the symbol-under-cursor
   contribute more weight than the user message, or equal? Current proposal
   concatenates them with a space (equal weight in FTS BM25). An alternative:
   run `get_context` twice (once for cursor symbol, once for user message) and
   take the union within budget.

3. **F4 context visibility**: should the injected context be visible to the
   user (shown as a "used context" chip in Copilot Chat) or hidden? The
   `ChatContext` API supports a `description` field that Copilot may display.
   Visible is more transparent; hidden is less distracting.

4. **F1 synonym direction**: should `fts_synonyms` support bidirectional pairs
   (insert both directions automatically on `travsr synonym add`)? Current
   proposal is one-directional, matching the static table contract.

5. **F2 model choice**: `nomic-embed-text` (v1.5, MRL-256) is the current
   candidate. Should we also support `all-minilm-l6-v2` (22 MB, lower quality)?
   Decision before `travsr embed init` is implemented.

6. **F2 quantisation depth**: RaBitQ 1-bit (~40 B/node) vs 8-bit scalar
   (~256 B/node). 1-bit is memory-optimal at 75M nodes (~3 GB); 8-bit would be
   ~19 GB. 1-bit is the default; 8-bit is a compile-time option.

7. **F3 feature flag name**: `mcp-sampling` vs `prompts` vs `l2d`. Pick one
   before implementation starts to avoid churn in Cargo.toml.

8. **Migration v11 parent RFC update**: RFC-012 §3.5 lists vec0 as v9 (stale
   after v9 became FTS, v10 became fts_vocab). Must be corrected to v12 as
   part of this sprint's first PR.

---

## References

- RFC-012: Fuzzy seed selection (L1 + T0)
- RFC-012-ADDENDUM-01: Key-free L2 ladder (L2-A/B/C/D), Rev 4
- `crates/travsr-store/src/seed_lexicon.rs` — static SYNONYMS + expand_tokens
- `crates/travsr-store/src/lib.rs:search_nodes_fuzzy` — three-layer query path
- `crates/travsr-store/src/migrations/v10_fts_vocab.sql` — vocabulary table
- `crates/travsr-mcp/src/server.rs:tools_list` / `tools_list_global`
- `packages/travsr-vscode/src/mcp.ts` — `StdioMcpClient.callTool` (F4 uses this)
- `packages/travsr-vscode/src/extension.ts` — extension entry point
- `docs/benchmarks/dogfooding.md` — T0 + L2-A end-to-end benchmark
- Issue #258 — S20 acceptance criteria (all complete as of S20)
- VS Code Chat Context Provider API: https://code.visualstudio.com/api/references/vscode-api#chat.registerChatContextProvider
