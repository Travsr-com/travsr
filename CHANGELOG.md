# Changelog

All notable changes to Travsr are documented here.

---

## v0.6.0 - 2026-05-31

This release ships the complete Phase 2 retrieval stack, the VS Code graph panel,
line-number storage for go-to-definition, and a new `get_graph_stats` introspection
tool. All seven MCP tools are now fully implemented.

### Retrieval

- **0-1 knapsack token-budget enforcer** (`travsr-retrieval`): Implements RFC-010.
  Given a set of PPR-scored nodes and a token budget, the knapsack solver selects
  the highest-value subgraph that fits the budget. Powers the `get_context` MCP tool
  and `travsr ask`.

- **`get_context` MCP tool**: Full PPR traversal ranked by relevance, budget-capped
  by the knapsack solver. Accepts an optional `token_budget` argument; defaults to
  the workspace-wide constant `MAX_CONTEXT_BUDGET`.

- **`travsr ask` upgraded to PPR + knapsack**: The CLI `ask` command now uses the
  full retrieval pipeline instead of raw BFS. Results are scored by PPR and trimmed
  to the token budget.

### VS Code Extension

- **Cytoscape.js graph panel** (`travsr-vscode`): A `WebviewPanel` rendering the
  live dependency graph for the active file. Supports kind filtering, two-hop import
  traversal, unique file node IDs, and the Travsr brand logo in the titlebar. Open
  via the sidebar or `Travsr: Show Graph` in the command palette.

- **`get_graph_stats` integration**: The status bar node count now reads from the
  `get_graph_stats` MCP tool instead of a stale cached value.

- **Bug fixes** (PR #226): Cache key collision, timer leak, loading state
  inconsistency, duplicate welcome panel, and blast-radius called per-symbol instead
  of per-file.

### MCP

- **`get_graph_stats` tool** (`travsr-mcp`): Returns accurate live node and edge
  counts for the indexed graph. Used by the VS Code status bar.

### Core / Indexer

- **Line numbers stored on nodes** (PR #249, #250): Every non-file, non-import node
  now carries a 1-based `line` field populated by all four Tree-sitter parsers
  (TypeScript, Rust, Python, Go). The `travsr-mcp` and `travsr-vscode` layers
  forward the field to callers. Enables go-to-definition in IDE integrations.

### Daemon / CLI

- **Daemon reliability fixes** (PR #223): Registry guard prevents double-registration
  on concurrent `travsr init` calls; `SKIP_DIRS` now respected during walk; `init`
  totals match actual indexed counts.

- **`travsr index` JSON fixes**: Retry daemon status ping 3x before reporting running;
  include all indexed nodes in JSON output; add `corpus` field to each node entry.

- **`delete_nodes_for_path_prefix`** (`travsr-store`): New store primitive used by
  the daemon when a directory is deleted between commits.

### CI

- **Fuzz corpus seeds** (PR #251): Added missing seed files for `fuzz_go_parser`,
  `fuzz_pcst_session`, and `fuzz_pyright_lsif_parser`; the nightly fuzz job no longer
  fails with empty-corpus errors.

- **Release tag glob fix**: `release.yml` tag pattern no longer matches `vscode-v*`
  tags, preventing spurious binary publish runs on VS Code extension releases.

### Architecture

- **RFC-011 - Two-transport plugin architecture**: Defines how future language plugins
  expose both an in-process tree-sitter path (fast, zero-IPC) and an out-of-process
  LSP/LSIF path (accurate, sandboxed). Adopted in ADR-017.

- **ADR-017 - Unified plugin sandbox trust model**: Establishes the trust boundary
  for out-of-process plugins and the capability set they may request.

- **RFC-008, RFC-009, ADR-009, ADR-010**: Multi-language extension architecture docs
  covering TypeScript, Python, Go, and Rust LSIF integration strategies.

**Full changelog:** https://github.com/Travsr-com/travsr/compare/v0.5.1...v0.6.0

---

## v0.3.1 — 2026-05-21

Security patch release on top of v0.3.0. No new features. Upgrade strongly recommended.

### Security

- **Git hook shell-injection fix**: The `post-commit` hook no longer passes
  git-reported filenames through shell expansion. Previously, a filename
  containing `;`, `$(...)`, or other shell metacharacters could execute
  arbitrary code on every `git commit`. The hook now calls
  `travsr hook-run --from-hook`; the binary reads changed files from git
  directly via `std::process::Command` — no shell involved.

- **File permission hardening**: `graph.db` is now created `0600` (owner
  read/write only). `~/.travsr/` is created `0700`. `registry.json` is
  written `0600`. Prevents other local users from enumerating indexed repos
  or reading derived graph data.

### Fixes

- `travsr-retrieval`: remove redundant closure wrapping `ppr_inner`
  (clippy `redundant_closure_call`).

**Full changelog:** https://github.com/raj-rkv/travsr/compare/v0.3.0...v0.3.1

---

## v0.3.0 — 2026-05-21

This release closes Phase 2 of the Travsr roadmap. The production Kùzu storage
backend is now available, the security hardening recommended in the Phase 1 audit
is complete, and the graph identity foundation (Kythe VNames, RFC-002) is locked
in for cross-repo support in Phase 3.

⚠️ **Migration required for v0.2.x users:** RFC-002 changes the VName corpus
format. Re-run `travsr init` in each repo — old signatures are rejected by
v0.3.0. The old `.travsr/graph.db` can be deleted safely; `travsr init` rebuilds
it from source.

### Production Storage

- **Kùzu backend** (`--features kuzu`): The production graph engine is now
  available. Kùzu handles up to 2.5B edges and is 64× faster than Neo4j on
  graph workloads. Build with `cargo build --release --features kuzu` to enable.

- **`travsr migrate --to kuzu`**: Migrate an existing SQLite graph to Kùzu
  with a single command. The migration is idempotent — running it twice is safe.
  SQLite is never deleted; both backends coexist.

- **Migration integrity manifest**: Every migration computes a SHA-256 digest
  of all nodes and edges before and after the copy. A mismatch aborts the
  migration and leaves SQLite intact. The Kùzu store is written to a staging
  path (`graph.kuzu.new`) and atomically renamed only after the manifest matches.

- **Backend-agnostic schema migration framework**: All storage backends now
  share a versioned migration runner. Schema migrations are idempotent and
  version-gated — re-running migrations on an already-migrated store is a no-op.

### Security

- **Prompt-injection hardening (SEC-001)**: All MCP tool outputs are now
  sanitized before being returned to the client. C0/C1 control characters are
  stripped, `<` and `>` are escaped, output is truncated to a safe maximum
  length, and every response is wrapped in a `<travsr-data>` structural envelope.

- **Path-traversal hardening (SEC-002)**: Path and symbol arguments are now
  validated at the MCP dispatch layer. `../`, `..\`, absolute paths, null bytes,
  and `%`-encoded traversal sequences are rejected before reaching the graph store.

- **Migration integrity verification (SEC-008)**: SHA-256 manifests are
  computed over every node and edge during SQLite→Kùzu migration. Mismatch
  aborts; staging directory is removed; SQLite store is left intact.

- **Git hook shell-injection hardening**: The `post-commit` hook no longer
  passes git-reported filenames through shell expansion. The hook now calls
  `travsr hook-run --from-hook`, which reads changed files from git directly
  via `std::process::Command` — filenames containing spaces, semicolons, or
  shell metacharacters are handled safely.

- **File permission hardening**: `graph.db` is now created with `0600`
  permissions (owner read/write only). `~/.travsr/` directory is created with
  `0700`, and `registry.json` is written with `0600`. Prevents other local
  users from enumerating indexed repos or reading derived graph data.

### Architecture

- **RFC-002 — VName signature format versioning**: VName signatures now include
  a version prefix and domain separator. Signatures from different versions do
  not collide. The corpus meta write is now a hard error — a repo with a
  mismatched or missing corpus identity will not index silently.

- **ARCH-102 — Kythe corpus naming convention**: Corpus names are now derived
  deterministically from the git remote URL (or directory name as fallback).
  All repos indexed with v0.3.0+ use the standardized convention.

- **ADR-003 — PPR algorithm constants**: The Personalized PageRank constants
  (`α = 0.85`, `ε = 1e-6`) are now defined in a dedicated module with
  compile-time bounds assertions. These will power the Phase 3 traversal engine.

### Quality

- **QA-010 — SQLite/Kùzu parity harness**: A property-based test harness verifies
  that SQLite and Kùzu backends return identical results for all graph queries.
  Parity is enforced on every CI run when `--features kuzu` is enabled.

- **QA-012 — MCP Phase 2 conformance suite**: The MCP server is now validated
  against the full JSON-RPC envelope spec, sanitization pipeline, and multi-repo
  `repo` argument routing.

### Breaking Changes

- **VName corpus identity (RFC-002)**: Repos indexed with v0.2.x will have a
  mismatched corpus signature under v0.3.0. Run `travsr init` again to re-index
  with the new VName format.

- **`graph.db` and `~/.travsr/` permissions**: These are now created `0600`/`0700`.
  Existing files are updated on the next `travsr init` or `travsr migrate` run.

**Full changelog:** https://github.com/raj-rkv/travsr/compare/v0.2.0...v0.3.0

---

## v0.2.0 — Phase 1 complete

Initial public release. Tree-sitter TypeScript indexer, SQLite graph store,
BFS retrieval, MCP stdio server, `travsr init` / `travsr ask` / `travsr mcp` CLI,
git post-commit hook, SHA-256 delta reindex, global multi-repo registry.

**Full changelog:** https://github.com/raj-rkv/travsr/compare/v0.1.3...v0.2.0

---

## v0.1.3 — Patch

**Full changelog:** https://github.com/raj-rkv/travsr/compare/v0.1.2...v0.1.3

## v0.1.2 — Patch

**Full changelog:** https://github.com/raj-rkv/travsr/compare/v0.1.1...v0.1.2

## v0.1.1 — Initial alpha

**Full changelog:** https://github.com/raj-rkv/travsr/releases/tag/v0.1.1
