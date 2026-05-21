# Changelog

All notable changes to Travsr are documented here.

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
