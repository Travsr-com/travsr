# Travsr — CLAUDE.md

> This file is loaded into every agent's context automatically.
> Read this before doing anything. It is the ground truth for the project.

---

## What Travsr Is

Travsr is a **graph-native, always-fresh code intelligence daemon** that lives next to Git.

**Core thesis:** Source code is a deterministic mathematical graph — not unstructured text. Every AI coding tool that uses vector RAG (Copilot, Cursor, Codeium) is wrong at the foundation. Travsr fixes this by building a live graph of your codebase and exposing it via MCP so AI agents traverse the graph instead of guessing from chunks.

**Result:** 80% fewer tokens, zero structural hallucinations, always-fresh context.

**Website:** travsr.com  
**npm:** `npm install -g travsr`  
**Tagline:** *The code graph that lives next to git.*

---

## Non-Negotiable Principles

1. **Algorithms first, LLM last** — graph algorithms answer structural questions. LLMs only translate queries and format results. LLMs must NEVER determine graph edges or node relationships.
2. **Always fresh** — the graph updates on every git commit via hooks. Staleness is a bug.
3. **Local first** — developer data stays on the developer's machine unless they explicitly opt into the cloud tier.
4. **MCP is the only external interface** — no REST API, no GraphQL. MCP over stdio (local) or SSE (cloud).
5. **OCI free tier only** — all cloud infrastructure runs on Oracle Cloud Always Free. Never provision anything that costs money.
6. **ARM64 for OCI** — OCI A1 instances are aarch64. All Docker images must be built for `linux/arm64`.
7. **No unsafe Rust** — `unsafe` is forbidden without an RFC and Tech Lead sign-off.

---

## The Algorithmic Stack

```
Graph Construction (Zero-LLM):
  Tree-sitter     → instant structural AST (all languages)
  LSIF compiler   → deep semantic edges (CFG, DFG, call graphs)
  Kythe VNames    → globally unique node identity across all repos

Storage:
  SQLite + WAL    → MVP (< 75M nodes)
  Kùzu            → Production (< 2.5B edges)
  RocksDB         → Hyperscale (unlimited)

Retrieval (No Vector Search):
  BFS depth 3     → MVP
  Personalized PageRank (PPR)  → Production
  Prize-Collecting Steiner Tree (PCST) → Full
  k-core decomposition → buried middle recovery
  0-1 Knapsack    → token budget optimization

Interface:
  MCP stdio       → local daemon ↔ IDE/agent
  MCP SSE         → cloud server ↔ remote clients
```

---

## Repo Structure

```
travsr/
├── CLAUDE.md                     ← you are here
├── .claude/
│   ├── skills/                   ← role skill files
│   └── agents/                   ← subagent prompt files
├── Cargo.toml                    ← workspace root
├── crates/
│   ├── travsr-core/              ← graph engine: nodes, edges, Kythe VNames
│   ├── travsr-indexer/           ← Tree-sitter + LSIF parsing pipeline
│   ├── travsr-store/             ← storage abstraction (SQLite/Kùzu/RocksDB)
│   ├── travsr-retrieval/         ← BFS, PPR, PCST, k-core, knapsack
│   ├── travsr-mcp/               ← MCP server (stdio + SSE)
│   ├── travsr-daemon/            ← Git hook, file watcher, scheduler
│   └── travsr-cli/               ← `travsr` binary entrypoint
└── packages/
    ├── travsr-vscode/            ← VS Code extension (TypeScript)
    └── travsr-lsif-ts/           ← TypeScript LSIF emitter (TypeScript)
```

---

## Crate Dependency Rules (Enforced — Never Violate)

```
travsr-core        → zero dependencies on other travsr crates
travsr-indexer     → depends on travsr-core only
travsr-store       → depends on travsr-core only
travsr-retrieval   → depends on travsr-core + travsr-store
travsr-mcp         → depends on travsr-retrieval
travsr-daemon      → depends on travsr-mcp + travsr-indexer
travsr-cli         → depends on travsr-daemon
```

No circular dependencies. Ever.

---

## MCP Tools (Current + Planned)

```
get_dependencies(file, transitive?)     → direct + transitive imports
get_callers(symbol, repo?)              → all call sites across repos
get_blast_radius(file)                  → what breaks if this changes
get_execution_path(source, sink)        → PCST path between symbols
get_context(query, token_budget)        → full PPR + knapsack pipeline
search_symbol(name, repo?)              → find symbol definitions
get_repo_map(repo)                      → structural overview
```

---

## Infrastructure (OCI Always Free)

```
Region: ap-mumbai-1 (or your chosen home region)

Instance 1: travsr-mcp-server
  Shape: VM.Standard.A1.Flex
  OCPU: 2 | RAM: 12 GB | Block: 50 GB
  Runs: travsr-mcp Docker container + Nginx SSL

Instance 2: travsr-indexer
  Shape: VM.Standard.A1.Flex
  OCPU: 2 | RAM: 12 GB | Block: 100 GB
  Runs: travsr-indexer + GitLab webhook receiver

Object Storage: travsr-releases (20 GB)
OCIR: ap-mumbai-1.ocir.io/travsr/ (500 MB)
DNS: travsr.com → Instance 1 public IP
SSL: Let's Encrypt via Certbot
```

---

## Current Phase

**MVP — Phase 1 (Weeks 1–6)**

Goal: Working CLI that indexes a TypeScript repo and serves MCP.

```
Sprint 1 (Weeks 1–2): Tree-sitter indexer + SQLite graph schema
Sprint 2 (Weeks 3–4): Git hook + SHA256 delta + BFS retrieval
Sprint 3 (Weeks 5–6): MCP server + CLI binary + npm package
```

Success criteria:
- `npm install -g travsr && travsr init` works on macOS + Linux
- `travsr ask "what calls PaymentService?"` returns correct answer
- Claude Desktop can use travsr as an MCP source

---

## Key Decisions Already Made (Do Not Re-Debate)

| Decision | Choice | Reason |
|---|---|---|
| Core language | Rust | Performance, safety, systems-level |
| MVP storage | SQLite + WAL | Zero setup, sufficient for < 75M nodes |
| Production storage | Kùzu | 64× faster than Neo4j for graph workloads |
| MVP retrieval | BFS depth 3 | Simple, correct, shippable |
| Node identity | Kythe VNames | Cross-repo, cross-language, stable |
| External interface | MCP only | Standard, agent-agnostic |
| Infrastructure | OCI Always Free | Zero cost |
| OCI compute | ARM64 A1 Flex | Only free compute with enough RAM |
| Open source license | MIT | Maximum adoption |

---

## Escalation Path

```
Code question          → Tech Lead
Architecture question  → Solution Architect → Principal Architect
Strategic question     → CTO
Infrastructure         → DevOps (OCI)
Testing                → QA
Planning / timeline    → Project Manager
```

---

## Git Conventions

```
Branch naming:   feature/<crate>-<description>
                 fix/<crate>-<description>
                 rfc/<number>-<title>

Commit format:   [crate-name] short description
                 e.g. [travsr-retrieval] Add BFS depth-limited traversal

PR requirements: CI green, 1 reviewer, no unwrap() in lib code
```