# RFC-012 L2 — LLM Query Translator (client-side) — Implementation Plan

Issue #258 · Sprint S20 · Branch `feature/travsr-vscode-l2-query-translator-258`

## Context

L1 (merged) gave the daemon a deterministic fuzzy seed primitive (`search_nodes_fuzzy`:
exact LIKE → FTS5 trigram → empty). L2 adds a **client-side** natural-language → symbol-fragment
translator so a user can type *"where do we dispatch MCP tool calls"* and have it resolved to the
right symbols before any MCP call is made.

**Hard invariant (RFC §2.0):** the LLM lives **only in the MCP client**. The daemon (`travsr-mcp`)
gains **zero** LLM dependencies and still receives only structured symbol strings. All graph traversal
(PPR/knapsack/PCST) stays deterministic and LLM-free. If L2 is disabled, unavailable, or fails, behaviour
degrades to L1 — never below it. This is the "deterministic floor".

## Key reality-vs-RFC reconciliation (the one design decision that matters)

The RFC §2.4 fan-out assumes the client issues N concurrent `search_symbol` calls and merges them.
**The actual VS Code extension does not do this.** Its only natural-language entry point is the
**graph panel search form** (`graph.ts::query()` → `callTool("get_graph_json", {query, direction, depth, kind_filter})`):
a single tool call with a single query string. (Confirmed by exploration — tree/hover/codelens all
operate on an already-known symbol from cursor position, not free-form NL.)

**Resolution — split the translator from its consumption:**

| Layer | Responsibility | Lives in |
|---|---|---|
| **Translator core** | pure `translate(nl) → StructuredQuery` (one LLM round-trip, caps, fallback) | `queryTranslator.ts` (vscode) + `translator.js` (npm reference) |
| **VS Code consumption** | take `StructuredQuery.symbols[0]` (+ `kinds[0]` as `kind_filter`) → single `get_graph_json` call | `graph.ts` |
| **Reference fan-out** | full §2.4: N concurrent `search_symbol` → merge/dedupe/filter → 50-cap, for headless MCP clients | `translator.js` (npm) |

So the graph panel stays a single-call visualization (it needs one root node), and the faithful §2.4
multi-symbol fan-out ships in the portable npm reference impl where clients drive `search_symbol`/`get_context`
directly. Both share the identical translator contract and prompt.

## The contract (RFC §2.2, unchanged)

```typescript
export interface StructuredQuery {
  symbols: string[];   // 1–5 identifier fragments (OR-semantics downstream)
  paths?: string[];    // 0–3 path globs (intersect)
  kinds?: string[];    // 0–3 of: function|method|class|struct|enum|file|module
  echo: string;        // paraphrase, telemetry only — NEVER forwarded to the daemon
}
export interface Translator { translate(naturalQuery: string): Promise<StructuredQuery>; }
```

Caps: ≤5 symbols, ≤3 paths, ≤3 kinds (bounds tool-call fan-out / prompt-injection storms).

## Changes

### 1. Translator core — `packages/travsr-vscode/src/queryTranslator.ts` (new, ~150 LOC)
- `class LlmTranslator implements Translator` with a provider abstraction.
- **Provider abstraction (greenfield — no HTTP infra exists today):** use Node 18 global `fetch`
  (VS Code ships modern Node) — **zero new runtime dependencies**. Three built-ins:
  - `anthropic` → `POST https://api.anthropic.com/v1/messages` (default model `claude-haiku-4-5-20251001`)
  - `openai` → `POST {base}/chat/completions` (default `gpt-4o-mini`)
  - `custom` → user-supplied OpenAI-compatible endpoint
- Fixed build-time system prompt verbatim from RFC §2.3. `temperature=0`, `max_tokens≈256`, single
  round-trip, no tool use, no agentic loop.
- **Caps + sanitisation applied to LLM output** (see §4) before returning the `StructuredQuery`.
- **Fallback floor (RFC §2.6):** any failure (network, non-2xx, malformed JSON, empty) returns
  `{ symbols: [naturalQuery], echo: naturalQuery }` with a `console.warn`/telemetry note — never throws.

### 2. `:` bypass + wiring in the graph panel — `packages/travsr-vscode/src/graph.ts` (~30 LOC)
- In `query(rawQuery, ...)`: 
  - If `travsr.l2.enabled` is false **or** `rawQuery` starts with `:` → strip a single leading `:`,
    **skip translation**, pass the literal straight to `get_graph_json` (deterministic path preserved).
  - Else `const sq = await translator.translate(rawQuery)` → use `sq.symbols[0]` as the `query` arg and,
    if present, `sq.kinds[0]` as `kind_filter`. Fall back to the literal if `symbols` is empty.
- Translation failure already degrades to literal via the core's fallback floor — the panel never errors.

### 3. Config + secret storage — `packages/travsr-vscode/package.json` + `extension.ts` (~40 LOC)
New `contributes.configuration` keys (mirroring the existing `travsr.*` pattern):
- `travsr.l2.enabled` (boolean, default `true`)
- `travsr.l2.provider` (enum `anthropic|openai|custom`, default `anthropic`)
- `travsr.l2.model` (string, default empty → provider default)
- `travsr.l2.endpoint` (string, custom provider only)
API key is **never** a setting — stored via `context.secrets` (VS Code SecretStorage). New command
`travsr.l2.setApiKey` (quick-input, `password: true`). If no key present and provider needs one →
translator silently falls back to L1 (deterministic floor; no nag-loop).

### 4. Client-side arg validation — mirror of `sanitize.rs` in `queryTranslator.ts`
RFC §2.7 requires translator output to pass the SEC-002 validator. Since `sanitize.rs` is Rust in the
daemon (re-validates server-side anyway), the client gets a **faithful TS mirror** `validateMcpArg(arg)`
enforcing the exact rules: ≤512 bytes, no `\0`, no `%`, no `../ ..\ .. /.. \..`, no leading `/` or `\`,
no `X:` drive letter. Plus control-char strip. Each `symbols[]` fragment that fails is **dropped**
(not the whole query). This is defence-in-depth; the daemon still re-validates.

### 5. npm reference translator + fan-out — `packages/travsr-npm/scripts/translator.js` (new, CommonJS, ~150 LOC)
- Standalone CommonJS reference (the package is CJS; only dep-free `https`/`fetch` used).
- Exports `translate(nl, opts)` (same contract + same prompt as the vscode core) **and** `fuzzyQuery(nl, mcp, opts)`
  implementing the faithful RFC §2.4 fan-out: concurrent `search_symbol` per fragment → merge → dedupe by
  `id` → path/kind filter → `.slice(0, 50)`. This is the portable impl "for any other MCP client".
- Mirror `validateMcpArg` here too.

### 6. `:` bypass in the CLI — `crates/travsr-cli/src/ask.rs` (~6 LOC, in scope per S20 table)
- Strip a single leading `:` from `query` before `search_nodes_fuzzy`, so the bypass syntax learned in
  VS Code is uniform and the colon doesn't pollute the exact-substring LIKE step. (The CLI has no LLM;
  the `:` is purely the "force-literal" marker here.) One unit test.

### 7. Tests
**TypeScript (Mocha + node:assert, matching existing suites):** `packages/travsr-vscode/src/test/suite/queryTranslator.test.ts`
- valid LLM JSON → correct `StructuredQuery`; caps truncate >5 symbols / >3 paths / >3 kinds.
- LLM unavailable (fetch throws) → fallback `{symbols:[nl], echo:nl}`, no throw.
- malformed JSON / non-2xx / empty body → fallback + warn.
- `:` bypass → `translate` not called, literal forwarded (graph.ts-level test with a stub translator).
- **Prompt-injection suite (named S20 deliverable):**
  - NL trying to coerce traversal: LLM emits `../../etc/passwd` symbol → dropped by `validateMcpArg`.
  - LLM emits a 10 KB symbol → dropped/over-512 rejected.
  - LLM emits 50 symbols → truncated to 5 (no tool-call storm).
  - `echo` is never passed to any `callTool` arg (assert daemon args contain only `symbols`).
- `validateMcpArg` parity tests mirroring the Rust `sanitize.rs` cases (the cross-language sync guard).

**Rust:** `ask.rs` `:`-strip unit test.

### 8. Docs — `docs/dogfooding.md`
Add an L2 section: ≥5 natural-language queries end-to-end (VS Code graph panel → translated symbols →
`get_graph_json` result), the `:` bypass, the provider/secret config, and the deterministic-floor guarantee.

## Out of scope (deferred)
- L3 embedding sidecar (`travsr-embed`, `--features embedding`) — Sprint S21 / Phase 5, separate PR.
- Synonym dictionaries, multilingual stemming (RFC Non-Goals).
- Any server-side LLM (explicitly forbidden by RFC §2.0 / A4).

## Security review (Principal Security Engineer lens)
- **Prompt injection cannot reach the daemon:** only the structured `symbols[]` (each `validateMcpArg`-clean)
  is forwarded; `echo` and raw NL never leave the client. Worst case = searches for noise symbols → empty.
- **Tool-call storm:** bounded by the ≤5 symbol cap and single round-trip (no agentic loop).
- **Secret hygiene:** API key in SecretStorage only, never in settings.json/telemetry/logs; `echo` logged
  but never forwarded.
- **No new daemon deps** (acceptance criterion) — `travsr-mcp` untouched; verified by `cargo tree`.
- **Determinism:** L2 best-effort (`temperature=0`); the true floor is the daemon receiving identical
  structured inputs → identical outputs (L1).

## Verification
```
# TS
cd packages/travsr-vscode && npm ci && npm run compile && npm test
cd packages/travsr-npm && node --test scripts/   # reference translator unit tests
# Rust (ask.rs change)
cargo fmt --all && cargo clippy --all-targets -- -D warnings
cargo test -p travsr-cli
cargo tree -p travsr-mcp | grep -iE 'anthropic|openai|reqwest-llm' || echo "daemon LLM-free: OK"
# e2e
TRAVSR_DISABLE_REGISTRY=1 ./target/release/travsr ask ":dispatch_tool_call"   # bypass → literal
```

## Acceptance criteria (RFC §L2, S20)
- [ ] `queryTranslator.ts` in travsr-vscode; `translator.js` reference in travsr-npm.
- [ ] Clean fallback to L1 on LLM unavailable / malformed / transient (unit tests).
- [ ] `:` bypass skips translation, sends literal (VS Code + `travsr ask`).
- [ ] Translator output passes the `sanitize.rs` rule set (TS mirror + parity tests).
- [ ] ≥5 NL queries documented end-to-end in `dogfooding.md`.
- [ ] No new dependencies in `travsr-mcp`.

## Files
- `packages/travsr-vscode/src/queryTranslator.ts` (new)
- `packages/travsr-vscode/src/graph.ts` (translation hook + `:` bypass)
- `packages/travsr-vscode/src/extension.ts` (config read + `setApiKey` command)
- `packages/travsr-vscode/package.json` (config keys + command)
- `packages/travsr-vscode/src/test/suite/queryTranslator.test.ts` (new, incl. prompt-injection suite)
- `packages/travsr-npm/scripts/translator.js` (new, reference + §2.4 fan-out)
- `crates/travsr-cli/src/ask.rs` (`:` bypass strip + test)
- `docs/dogfooding.md` (L2 benchmark/usage section)
