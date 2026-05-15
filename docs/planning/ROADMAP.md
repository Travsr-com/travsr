# Travsr Roadmap

> Owner: Project Manager. Source of truth for phase scope and dates.
> Anchors: `CLAUDE.md` (algorithmic stack, principles) and `Travsr-Proposal.md` (build order).
> Cadence: two-week sprints. Sprint 1 starts Monday 2026-05-18.

---

## Phase Overview

| Phase | Dates | Sprints | Theme |
|---|---|---|---|
| Phase 1 | 2026-05-18 to 2026-06-26 | S1, S2, S3 | MVP: Tree-sitter + SQLite + BFS + MCP stdio |
| Phase 2 | 2026-06-29 to 2026-08-21 | S4–S7 | LSIF (TypeScript) + Kuzu storage + PPR retrieval |
| Phase 3 | 2026-08-24 to 2026-10-16 | S8–S11 | Glean-style incremental indexing + VS Code extension |
| Phase 4 | 2026-10-19 to 2026-12-25 | S12–S16 | Multi-language LSIF + PCST + k-core + knapsack + Graph RBAC |
| Phase 5 | 2027-01-04 onward | S17+ | Multi-repo cloud (OCI) + sharding + SSE MCP |

---

## Phase 1 — MVP (Weeks 1-6)

- **Goal:** Ship a working `travsr` CLI that indexes a TypeScript repo, stays fresh on git commits, and serves MCP over stdio to Claude Desktop.
- **Headline deliverables:**
  - `travsr-core` Kythe VName + Node/Edge model
  - `travsr-indexer` Tree-sitter TypeScript pipeline
  - `travsr-store` SQLite + WAL schema with migrations
  - `travsr-daemon` post-commit git hook + SHA256 delta
  - `travsr-retrieval` BFS depth-3 with token budget
  - `travsr-mcp` stdio JSON-RPC server, tools: `get_dependencies`, `get_callers`
  - `travsr-cli` `init`, `status`, `ask`, `mcp --stdio`
  - npm wrapper that ships the prebuilt Rust binary
- **Exit criteria:**
  - [ ] `npm install -g travsr` works on macOS and Linux
  - [x] `travsr init` produces `.travsr/graph.db`
  - [ ] Commit triggers graph update in under 1s on a 10-file fixture
  - [ ] `travsr ask` returns correct callers on the fixture
  - [ ] Claude Desktop talks to Travsr via stdio MCP
  - [ ] Public GitHub repo, MIT LICENSE, README quickstart, CI green

### Sprint 1 — Foundation (2026-05-18 → 2026-05-15) ✅ DONE

**Delivered:**
- S1-1 `travsr-core`: VName/Node/Edge/EdgeKind locked, BLAKE3 NodeId, 5 tests ✅
- S1-2 `travsr-indexer`: Tree-sitter TypeScript pipeline (class/fn/method/var/import), 5 integration tests ✅
- S1-3 `travsr-store`: SqliteStore + WAL + migrations + node_count/edge_count, 6 tests ✅
- S1-4 `travsr-cli`: `travsr init` (WalkBuilder, .gitignore-aware) + `travsr status`, 5 CLI tests ✅
- 21/21 tests green · clippy clean · fmt clean
- E2E demo: `fixtures/ts-small` → 7 nodes, 4 edges, idempotent ✅

**Retro:**
- ✅ Tree-sitter version pair (0.22 + 0.21) resolved cleanly — version risk was real but mitigated fast
- ✅ Pre-existing `should_implement_trait` clippy lint caught and fixed — CI would have blocked the PR
- ✅ Edge hierarchy (class→method, not file→method) locked early by Tech Lead — no rework needed
- ⚠️ Rust not installed on dev machine — DevOps should add rustup bootstrap check to onboarding docs
- ⚠️ `VName.path` ended up as absolute path, not repo-relative — spec vs implementation gap; mitigated by debt ticket

**Debt carried to Sprint 2:**
- `DEBT(travsr-010)`: `init_repo` loop lives in `travsr-cli`; migrate to `travsr-daemon::init_repo()`
- `DEBT(travsr-011)`: `nodes_written` counter double-counts on re-index runs
- `DEBT(travsr-012)`: `VName.path` must be repo-relative (currently absolute path from WalkBuilder)

---

## Phase 2 — LSIF + Kuzu + PPR (Weeks 7-14)

- **Goal:** Move from structural-only to semantic-grade graph and from BFS toy retrieval to Personalized PageRank.
- **Headline deliverables:**
  - TypeScript LSIF emitter (`packages/travsr-lsif-ts`) wired into the indexer
  - Kuzu storage backend behind the `travsr-store` trait, feature-flagged
  - Migration path: SQLite to Kuzu for existing `.travsr` directories
  - PPR implementation in `travsr-retrieval` with damping factor + seed distribution
  - MCP tools added: `get_blast_radius`, `search_symbol`, `get_repo_map`
- **Exit criteria:**
  - [ ] Kuzu backend passes the same correctness suite as SQLite
  - [ ] PPR returns the documented top-k on a 1k-file TypeScript fixture
  - [ ] Indexer ingests LSIF dump under 30s on the fixture
  - [ ] Bench: query p95 under 50ms on the fixture

---

## Phase 3 — Glean-style Incremental + VS Code (Weeks 15-22)

- **Goal:** Move past delete-and-reinsert. Make the IDE the primary surface.
- **Headline deliverables:**
  - Stacked-database visibility masks in `travsr-store`
  - `git diff --histogram` based delta computation in `travsr-indexer`
  - Reverse-topological ownership propagation
  - VS Code extension (`packages/travsr-vscode`): tree view, code lens, hover, status bar freshness indicator
  - `get_context(query, token_budget)` MCP tool
- **Exit criteria:**
  - [ ] No physical deletes on commit; reads honor visibility masks
  - [ ] VS Code marketplace listing published (preview)
  - [ ] End-to-end demo: edit, save, commit, hover updates within 1s

---

## Phase 4 — Multi-language + PCST/k-core/Knapsack + RBAC (Weeks 23-32)

- **Goal:** Language breadth and the full retrieval stack. First security-grade release.
- **Headline deliverables:**
  - LSIF or equivalent for Python, Go, Java, Rust, C/C++ (priority order TBD by Phase 3 retro)
  - PCST approximation for `get_execution_path`
  - k-core decomposition surfacing buried-middle nodes
  - 0-1 knapsack token budget optimizer
  - Graph RBAC: session-as-node, structural membership edges, traversal blocked at boundary
- **Exit criteria:**
  - [ ] Two new languages green on the correctness suite
  - [ ] PCST returns connecting subgraph on a multi-concept query fixture
  - [ ] Restricted nodes never appear in retrieved context (audited)
  - [ ] Security review signed off by Principal Security Engineer

---

## Phase 5 — Multi-repo Cloud + Sharding (Weeks 33+)

- **Goal:** Cloud-tier offering on OCI Always Free; multi-repo by Kythe VName corpus; SSE MCP transport.
- **Headline deliverables:**
  - GitLab/GitHub webhook receiver on OCI Instance 2
  - Cross-repo edges via Kythe `%kythe/edge/exports`
  - Consistent-hash sharding by module boundary
  - MCP over SSE with auth
  - Cloud web dashboard (read-only)
- **Exit criteria:**
  - [ ] 10-repo cluster indexed and queryable from a remote MCP client
  - [ ] OCI bill remains 0.00
  - [ ] Tenant isolation reviewed and signed off

---

## Public Launch Plan

Soft-launch coincides with Phase 1 exit (target week of 2026-06-29): the public GitHub repo flips from private to MIT-licensed open source with a working README quickstart, a 90-second demo recording, and `travsr` reserved on npm. Announcement goes out via Hacker News "Show HN", the r/programming and r/LocalLLaMA subreddits, and a thread on X tagging the MCP community. Travsr.com lands with a single-page pitch derived from `Travsr-Proposal.md` and a "graph not chunks" hero. We hold the Kuzu/PPR story for a second wave at Phase 2 exit; the first wave is intentionally MVP-honest to set expectations and recruit early contributors. KPIs for the first 30 days: 1k GitHub stars, 100 npm installs, 5 external contributors, zero critical bugs filed against the indexer.
