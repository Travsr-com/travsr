# travsr

**The code graph that lives next to git.**

> Source code is a deterministic graph, not unstructured text. Travsr builds
> that graph on every commit and exposes it via MCP so AI agents traverse edges
> instead of guessing from vector chunks — 80% fewer tokens, zero structural
> hallucinations.

[![CI](https://github.com/raj-rkv/travsr/actions/workflows/ci.yml/badge.svg)](https://github.com/raj-rkv/travsr/actions/workflows/ci.yml)
[![npm](https://img.shields.io/npm/v/%40travsr.com%2Ftravsr)](https://www.npmjs.com/package/@travsr.com/travsr)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

---

## Quickstart

```bash
# 1. Install
npm install -g @travsr.com/travsr

# 2. Initialize your repo (requires git)
cd your-project
git init          # skip if already a git repo
travsr init       # indexes TypeScript files → .travsr/graph.db
                  # auto-registers in ~/.travsr/registry.json

# 3. Connect to Claude Desktop — set once, works for all repos
```

Add to `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS)
or `%APPDATA%\Claude\claude_desktop_config.json` (Windows):

```json
{
  "mcpServers": {
    "travsr": {
      "command": "travsr",
      "args": ["mcp", "--stdio", "--global"]
    }
  }
}
```

Restart Claude Desktop. Ask: *"Who calls PaymentService.charge?"*

> **No `cwd` needed.** `--global` reads `~/.travsr/registry.json` which
> `travsr init` populates automatically. Every repo you init becomes
> immediately available — no config changes required.

---

## Multi-repo support

Travsr maintains a global registry at `~/.travsr/registry.json`. Every
`travsr init` call registers that repo automatically.

```bash
# Init each repo once — that's it
cd ~/projects/repo-a && travsr init
cd ~/projects/repo-b && travsr init
cd ~/projects/task-manager && travsr init

# See all registered repos
travsr repos
```

```
| Name         | DB Path                                           | Exists |
| repo-a       | /Users/you/projects/repo-a/.travsr/graph.db      | yes    |
| repo-b       | /Users/you/projects/repo-b/.travsr/graph.db      | yes    |
| task-manager | /Users/you/projects/task-manager/.travsr/graph.db | yes   |
```

The single `--global` MCP server serves all of them. When you ask about a
symbol, it searches all registered repos and prefixes results with
`[repo-name]` so you always know which codebase the answer came from.

---

## Works with Every MCP-Compatible AI Tool

Travsr speaks [MCP](https://modelcontextprotocol.io) — the open standard for
connecting AI agents to tools.

### Claude Desktop / global mode (recommended)

```json
{
  "mcpServers": {
    "travsr": {
      "command": "travsr",
      "args": ["mcp", "--stdio", "--global"]
    }
  }
}
```

### Cursor

Add to `~/.cursor/mcp.json` (global) or `.cursor/mcp.json` (per-project):

```json
{
  "mcpServers": {
    "travsr": {
      "command": "travsr",
      "args": ["mcp", "--stdio", "--global"]
    }
  }
}
```

### GitHub Copilot (VS Code)

Requires VS Code 1.99+. Add to `.vscode/mcp.json` in your project:

```json
{
  "servers": {
    "travsr": {
      "type": "stdio",
      "command": "travsr",
      "args": ["mcp", "--stdio", "--global"]
    }
  }
}
```

### Cline (VS Code extension)

In the Cline extension settings → **MCP Servers** → Add server:

```json
{
  "travsr": {
    "command": "travsr",
    "args": ["mcp", "--stdio", "--global"],
    "disabled": false
  }
}
```

### Continue.dev

Add to `~/.continue/config.json` under `mcpServers`:

```json
{
  "mcpServers": [
    {
      "name": "travsr",
      "command": "travsr",
      "args": ["mcp", "--stdio", "--global"]
    }
  ]
}
```

### Single-repo mode

If you prefer to point at one specific repo, use `--db`:

```bash
travsr mcp --stdio --db /path/to/repo/.travsr/graph.db
```

Or omit both flags and run from inside the repo — travsr discovers the db
from the current git root.

---

## MCP Tools

All tool responses are wrapped in a `<travsr-data>` envelope and sanitized
before being returned, so returned content is safe to pass directly into LLM
context (control characters stripped, prompt-injection vectors neutralised).

In global mode, tools that accept a `file` or `symbol` argument also accept
an optional `repo` parameter to target a specific registered repo. Omitting
`repo` searches all registered repos.

| Tool | Required | Optional | Description | Status |
|---|---|---|---|---|
| `get_dependencies(file)` | `file` — file path | `repo` | Return all imports/dependencies of a file | Available |
| `get_callers(symbol)` | `symbol` — symbol name | `repo` | Return all nodes with an incoming edge to a symbol | Available |
| `get_blast_radius(file)` | `file` — file path | `repo` | Return the set of files transitively affected if the given file changes | Phase 3 |
| `search_symbol(name)` | `name` — symbol name | `repo` | Find symbol definitions matching a name across the indexed graph | Phase 3 |
| `get_repo_map` | — | `repo` | Return a structural overview of the indexed repository | Phase 3 |

> Phase 3 tools are registered and respond with well-formed JSON-RPC. Full
> graph traversal implementations (PPR + PCST + k-core) ship in Phase 3.

---

## CLI Commands

```
travsr init                      Index the repo, install git hook, register globally
travsr repos                     List all globally registered repos
travsr status                    Show node/edge counts, schema version, last-indexed SHA
travsr ask <query>               BFS symbol lookup from the terminal (partial match)
travsr migrate --to kuzu         Migrate the graph store from SQLite to Kùzu backend
travsr mcp --stdio               Start the MCP stdio server (single-repo, cwd-based)
travsr mcp --stdio --global      Start the MCP stdio server (all registered repos)
travsr mcp --stdio --db <path>   Start the MCP stdio server (explicit db path)
travsr graph <query>             Show dependency graph for a symbol or file
travsr graph --all               Show graph for the entire indexed repository
```

### travsr migrate

Migrate an existing SQLite graph to the Kùzu production backend. The SQLite
database is never deleted — both backends coexist and `travsr status` continues
to read from SQLite after migration.

```bash
# Requires a kuzu-enabled build (see Build from Source below)
travsr migrate --to kuzu
```

```
sqlite source : .travsr/graph.db
  nodes       : 18432
  edges       : 94107
  sha256      : a3f2...

migrating to kuzu at .travsr/graph.kuzu …

migration complete.
  kuzu path   : .travsr/graph.kuzu
  nodes       : 18432
  edges       : 94107
  sha256      : a3f2...

tip: SQLite graph is unchanged — `travsr status` reads graph.db
     and shows the same counts as before.
```

The migration computes a SHA-256 integrity manifest of every node and edge
before and after the copy. If the digests don't match, the staging directory is
removed and the SQLite store is left intact.

### travsr graph

Visualise the dependency graph from any symbol or file as an ASCII tree,
Graphviz DOT, or structured JSON.

```bash
# ASCII tree (default) — what does extension.ts import and define?
travsr graph extension.ts

# Who calls PaymentService.charge?
travsr graph PaymentService.charge --direction callers

# Both directions, depth 2
travsr graph service.ts --direction both --depth 2

# Render as SVG (requires graphviz: brew install graphviz)
travsr graph extension.ts --format dot | dot -Tsvg -o graph.svg && open graph.svg

# Machine-readable JSON for AI tools
travsr graph extension.ts --format json

# Whole-repository graph
travsr graph --all --format dot | dot -Tsvg -o repo.svg && open repo.svg
travsr graph --all --format json
```

**Flags:**

| Flag | Default | Description |
|---|---|---|
| `--direction` | `deps` | `deps` · `callers` · `both` |
| `--depth` | `3` | Maximum traversal depth |
| `--format` | `tree` | `tree` · `dot` · `json` |
| `--all` | — | Dump the entire indexed graph (mutually exclusive with `<query>`) |

**JSON output schema** (`--format json`):

```json
{
  "schema_version": 1,
  "summary": {
    "mode": "query",
    "root": "file",
    "root_path": "src/index.ts",
    "total_nodes": 6,
    "total_edges": 5,
    "kinds": { "file": 1, "function": 2, "import": 2, "variable": 1 }
  },
  "nodes": [
    { "id": "...", "signature": "fn:activate", "kind": "function",
      "path": "src/index.ts", "language": "typescript", "depth_from_seed": 1 }
  ],
  "edges": [
    { "from": "file", "to": "fn:activate", "kind": "defines/binding" }
  ]
}
```

---

## Storage Backends

| Backend | Flag | Best for | Status |
|---|---|---|---|
| SQLite + WAL | _(default)_ | < 75M nodes, zero setup | Available |
| Kùzu | `--features kuzu` | < 2.5B edges, production workloads | Available (Phase 2) |
| RocksDB | `--features rocksdb` | Hyperscale / unlimited | Planned (Phase 3) |

SQLite is the default and requires no additional dependencies. Kùzu is a native
property-graph engine that is 64× faster than Neo4j on graph workloads and is
now available behind a feature flag.

To migrate an existing SQLite graph to Kùzu, build with `--features kuzu` and
run `travsr migrate --to kuzu`.

---

## How It Works

```
git init && travsr init
  └─▶ walks .ts / .tsx files (respects .gitignore)
        └─▶ Tree-sitter parses each file
              └─▶ Nodes + edges → .travsr/graph.db (SQLite WAL)
                    └─▶ post-commit hook installed
                          └─▶ repo registered in ~/.travsr/registry.json

git commit
  └─▶ post-commit hook fires
        └─▶ travsr hook-run <changed files>
              └─▶ SHA-256 delta — only re-indexes changed files
                    └─▶ graph.db updated, last_commit SHA recorded

travsr migrate --to kuzu   (optional, kuzu build only)
  └─▶ SHA-256 manifest of all edges computed from SQLite
        └─▶ nodes + edges bulk-copied to .travsr/graph.kuzu.new (staging)
              └─▶ post-copy manifest compared — mismatch aborts, SQLite intact
                    └─▶ atomic rename: graph.kuzu.new → graph.kuzu
```

**Graph stays current via the post-commit hook** — every committed change is
re-indexed automatically. The graph is also fully queryable immediately after
`travsr init`, before any commit.

Language support: **TypeScript / TSX**. Python, Go, Rust planned.

### Retrieval algorithms

| Algorithm | When used | Status |
|---|---|---|
| BFS depth-3 | All `get_dependencies` / `get_callers` queries | Available |
| Personalized PageRank (PPR) | `get_context` and deep traversal (Phase 3) | Constants ready (α=0.85, ε=1e-6) |
| Prize-Collecting Steiner Tree | `get_execution_path` (Phase 3) | Planned |
| k-core decomposition | Buried-middle recovery (Phase 3) | Planned |
| 0-1 Knapsack | Token budget optimisation (Phase 3) | Planned |

### Edge kinds

| Kind | Meaning |
|---|---|
| `depends` | File imports another module |
| `defines/binding` | File or class defines a symbol (function, method, variable) |
| `ref/call` | Call-site reference |
| `exports` | Symbol exported from a module |

### Security

All MCP tool outputs are passed through a sanitization pipeline before being
returned to the client:

1. Truncated to a safe maximum length
2. C0/C1 control characters stripped
3. `<` and `>` escaped to prevent XML/HTML injection in tool descriptions
4. Wrapped in a `<travsr-data>` structural envelope

Path traversal and argument injection are rejected at the tool dispatch layer
(`../`, `..\\`, absolute paths, null bytes, `%`-encoded traversal sequences).

---

## Build from Source

```bash
git clone https://github.com/raj-rkv/travsr
cd travsr

# Default build (SQLite backend only)
cargo build --release   # requires Rust 1.75+

# With Kùzu production backend (requires CMake + C++ toolchain)
cargo build --release --features kuzu

# Override the npm-installed binary with a local build
cp target/release/travsr $(which travsr)
# or
export TRAVSR_BINARY=/path/to/travsr/target/release/travsr
```

---

## Troubleshooting

- **`not inside a git repository`**
  Run `git init` before `travsr init`.

- **`not initialized — run travsr init`**
  Run `travsr init` in the repo root before using `graph`, `ask`, `status`, or `mcp`.

- **MCP server returns empty results in `--global` mode`**
  Run `travsr repos` to verify the repo is registered and `Exists` shows `yes`.
  If missing, re-run `travsr init` in that repo.

- **Stale entries in `travsr repos` (Exists = no)**
  Safe to ignore — they are skipped automatically. They appear when a repo
  was deleted or moved after being indexed.

- **`travsr migrate` — kuzu feature not enabled**
  Rebuild with `cargo build --features kuzu` (requires CMake and a C++ toolchain).

- **`travsr migrate` — Kùzu store already exists**
  Migration was already completed. Run `travsr status` to verify counts.
  Remove `.travsr/graph.kuzu` manually only if you need to re-migrate.

- **Binary not found after npm install?**
  Set `TRAVSR_BINARY=/path/to/travsr` to use a local build instead.

- **Corporate proxy blocks the postinstall download?**
  Same — set `TRAVSR_BINARY` to skip the remote fetch.

---

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Issues and PRs welcome. Licensed MIT.
