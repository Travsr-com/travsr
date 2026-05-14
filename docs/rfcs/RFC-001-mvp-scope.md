# RFC-001: MVP Scope (Phase 1)

| Field    | Value           |
|----------|-----------------|
| Status   | Draft           |
| Author   | Tech Lead       |
| Date     | 2026-05-15      |
| Phase    | MVP — Weeks 1–6 |

## Summary

Define the exact surface area of Travsr's MVP: a single-language (TypeScript), single-machine, Tree-sitter-backed, SQLite-stored, BFS-retrieved code graph daemon that ships as an npm-installable CLI and exposes two MCP tools (`get_dependencies`, `get_callers`) over stdio. Everything outside this scope is deferred.

## Motivation

The Travsr thesis — graph over RAG — needs a credible, shippable demonstration before any production algorithm work begins. The MVP must prove three things to the market and to ourselves: (1) Tree-sitter alone produces a useful graph for one language; (2) git-hook-driven incremental indexing is fast enough to be invisible; (3) an MCP client (Claude Desktop) can consume Travsr and answer structural questions that vector RAG cannot. We optimize for time-to-first-correct-answer, not breadth.

## Non-Goals

- Cross-language indexing (only TypeScript in MVP).
- LSIF, CFG, DFG, or any compiler-grade semantic edges.
- Personalized PageRank, PCST, k-core, knapsack budgeting.
- Kùzu or RocksDB storage backends.
- Cloud tier, SSE transport, multi-tenant RBAC.
- IDE extensions beyond a stub that proves MCP works in Claude Desktop.
- Performance tuning beyond "feels instant on a 50k-LOC repo".

## MVP Success Criteria

Lifted verbatim from `CLAUDE.md`:

- `npm install -g travsr && travsr init` works on macOS + Linux.
- `travsr ask "what calls PaymentService?"` returns the correct answer.
- Claude Desktop can use travsr as an MCP source.

## Design Sketch

**`travsr-core`** — Defines the canonical graph types: `Node`, `Edge`, `VName` (Kythe 5-field), edge enums (`Imports`, `Calls`, `Defines`, `References`). Pure data, zero I/O, zero dependencies on other Travsr crates. Every higher crate speaks this vocabulary.

**`travsr-indexer`** — Wraps Tree-sitter with a TypeScript grammar. Walks a repo, parses every `.ts`/`.tsx` file, emits `Node` and `Edge` records into a channel. Computes SHA256 per file so the daemon can diff old vs new on subsequent runs. No LSIF. No semantic resolution beyond what Tree-sitter queries can express.

**`travsr-store`** — Storage abstraction trait `GraphStore`. MVP implementation is `SqliteStore` using SQLite in WAL mode with three tables (`nodes`, `edges`, `file_hashes`) plus indexes on `vname` and `(src, edge_kind)`. Schema is migration-versioned from day one so we can swap to Kùzu later without breaking the abstraction.

**`travsr-retrieval`** — Implements depth-bounded BFS (default depth 3) over a `GraphStore`. Two public entry points map to the two MVP MCP tools: `dependencies_of(vname, transitive)` and `callers_of(vname)`. Returns ranked `Vec<Node>` plus the path edges so clients can render evidence.

**`travsr-mcp`** — MCP server over stdio implementing the official protocol. Registers two tools, validates JSON-Schema inputs, dispatches to `travsr-retrieval`, serializes results. SSE transport is stubbed but disabled.

**`travsr-daemon`** — Installs a `post-commit` git hook on `travsr init`. The hook invokes `travsr index --incremental`, which reads `file_hashes`, re-indexes only changed files, and updates the store atomically inside a single SQLite transaction. Also exposes a long-lived `travsr serve` mode that hosts the MCP server.

**`travsr-cli`** — `clap`-derive entrypoint for the `travsr` binary. Subcommands: `init`, `index`, `ask`, `serve`, `version`. `ask` is a thin local MCP client for shell debugging; the real consumer is Claude Desktop.

## MCP Tools Shipped in MVP

| Tool              | Input                                 | Output                              |
|-------------------|---------------------------------------|-------------------------------------|
| `get_dependencies`| `{ file: string, transitive?: bool }` | List of imported modules + VNames   |
| `get_callers`     | `{ symbol: string, repo?: string }`   | List of call-site VNames + spans    |

Both back onto BFS depth 3 over the SQLite store. Any other tool listed in `CLAUDE.md` ships in a later phase.

## Algorithm Choices for MVP

- **Parsing:** Tree-sitter only. No LSIF, no `tsc` API.
- **Storage:** SQLite with `journal_mode=WAL`, `synchronous=NORMAL`. Single-file DB at `.travsr/graph.db`.
- **Retrieval:** BFS, depth-limited to 3, with a visited set and a hard cap of 10k expanded nodes per query.
- **Identity:** Kythe VNames computed from `(repo_root, relative_path, language="typescript", signature)`.

These are locked decisions from `CLAUDE.md` and are not open for re-debate in this RFC.

## Out of Scope for MVP

- LSIF emitter and compiler-grade semantic edges.
- Personalized PageRank, Prize-Collecting Steiner Tree, k-core decomposition, 0-1 knapsack budgeting.
- Kùzu and RocksDB backends.
- Any language other than TypeScript.
- Cloud tier, SSE transport, Graph RBAC, multi-tenant isolation.
- VS Code and JetBrains extensions beyond a smoke test.
- Windows support (Linux + macOS only).

## Risks

- **Tree-sitter alone is too shallow** to answer "what calls X?" precisely for TypeScript's dynamic patterns (re-exports, default exports, namespace merging). Mitigation: scope demos to direct named imports; document the limitation.
- **SQLite write contention** during incremental re-index if the user commits rapidly. Mitigation: serialize indexer behind a file lock; WAL mode keeps reads non-blocking.
- **MCP protocol churn** — the spec is still evolving. Mitigation: pin a specific protocol version, gate breaking client changes behind a version handshake.
- **npm packaging of a Rust binary** across macOS x86_64, macOS arm64, and Linux x86_64/arm64. Mitigation: use `napi-rs`-style platform-specific optional deps, validated in CI on day one of Sprint 3.
- **Git hook injection** is a security boundary. Mitigation: hook must be idempotent, opt-in via `travsr init`, removable via `travsr uninit`; sign-off from the Principal Security Engineer before release.

## Open Questions

- Do we ship `travsr ask` as a permanent UX surface, or strictly as a debugging aid that we remove before 1.0?
- Should incremental indexing run on `post-commit` only, or also on `post-merge` and `post-checkout`?
- What is the canonical location of `.travsr/` — repo root, `$XDG_DATA_HOME`, or both with a symlink?
- How do we represent TypeScript path aliases (`tsconfig.json` `paths`) in VNames without an `tsc` resolver?
- Do we require Node 20+ for the npm wrapper, or support Node 18 LTS?

## Acceptance Checklist

- [ ] `cargo test --workspace` is green on Linux x86_64, Linux arm64, macOS arm64.
- [ ] `cargo clippy --workspace -- -D warnings` is green.
- [ ] `cargo fmt --check` is green.
- [ ] No `unsafe` blocks anywhere in the workspace (`#![forbid(unsafe_code)]` enforced).
- [ ] `npm install -g travsr` succeeds on macOS arm64 and Linux x86_64.
- [ ] `travsr init` inside a fresh git repo installs the `post-commit` hook and creates `.travsr/graph.db`.
- [ ] `travsr index` produces a non-empty graph for a 1k-file TypeScript repo in under 10 seconds.
- [ ] Editing one file and committing re-indexes only that file (verified via log + timing).
- [ ] `travsr serve` responds to MCP `initialize`, `tools/list`, and `tools/call` over stdio.
- [ ] `get_dependencies` returns the correct import set for a known fixture file.
- [ ] `get_callers` returns the correct call sites for a known fixture symbol.
- [ ] Claude Desktop, configured with travsr as an MCP source, answers "what calls PaymentService?" correctly on the demo repo.
- [ ] `travsr uninit` removes the git hook and is idempotent.
