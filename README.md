# travsr

**The code graph that lives next to git.**

> Source code is a deterministic graph, not unstructured text. Travsr builds
> that graph on every commit and exposes it via MCP so AI agents traverse real
> edges instead of guessing from vector chunks.

[![CI](https://github.com/Travsr-com/travsr/actions/workflows/ci.yml/badge.svg)](https://github.com/Travsr-com/travsr/actions/workflows/ci.yml)
[![Bench](https://github.com/Travsr-com/travsr/actions/workflows/bench.yml/badge.svg)](https://github.com/Travsr-com/travsr/actions/workflows/bench.yml)
[![Phase 2 Exit](https://github.com/Travsr-com/travsr/actions/workflows/phase2-exit.yml/badge.svg)](https://github.com/Travsr-com/travsr/actions/workflows/phase2-exit.yml)
[![npm](https://img.shields.io/npm/v/%40travsr.com%2Ftravsr)](https://www.npmjs.com/package/@travsr.com/travsr)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)

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

# 3. Connect to Claude Desktop (set once, works for all repos)
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
> immediately available, no config changes required.

---

## Multi-repo support

Travsr maintains a global registry at `~/.travsr/registry.json`. Every
`travsr init` call registers that repo automatically.

```bash
# Init each repo once
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

Travsr speaks [MCP](https://modelcontextprotocol.io), the open standard for
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

Or omit both flags and run from inside the repo; travsr discovers the db
from the current git root.

---

## MCP Tools

All tool responses are wrapped in a `<travsr-data>` envelope and sanitized
before being returned, so returned content is safe to pass directly into LLM
context (control characters stripped, prompt-injection vectors neutralised).

In global mode, tools that accept a `file` or `symbol` argument also accept
an optional `repo` parameter to target a specific registered repo. Omitting
`repo` searches all registered repos.

**Query tools** (available in both single-repo and global mode):

| Tool | Description |
|---|---|
| `get_dependencies(file)` | All imports/dependencies of a file |
| `get_callers(symbol)` | All nodes with an incoming edge to a symbol |
| `get_blast_radius(file)` | Files transitively affected if the given file changes |
| `search_symbol(name)` | Symbol definitions matching a name across the graph |
| `get_repo_map` | Structural overview of the indexed repository |
| `get_execution_path(source, sink)` | PCST-optimal execution path between two symbols |
| `get_context(query, token_budget)` | PPR traversal ranked by relevance, budget-capped by knapsack |
| `get_graph_stats` | Node/edge counts, schema version, last-indexed SHA |
| `get_graph_json(query, direction, depth)` | Subgraph as structured JSON for graph renderers |
| `get_snippets(symbol)` | Source snippets for a symbol with file and line context |
| `get_lang_status` | Phase B indexer status for each language in the repo |
| `repo_languages` | Languages detected in the indexed repository |

**Repo management tools** (global mode):

| Tool | Description |
|---|---|
| `repos_list` | List all globally registered repos |
| `repos_remove(repo)` | Remove a repo from the global registry |
| `repos_prune` | Remove registry entries whose db path no longer exists |

**Synonym tools** (query expansion):

| Tool | Description |
|---|---|
| `synonym_add(term, alias)` | Add a single term/alias pair |
| `synonym_set(term, aliases)` | Replace all aliases for a term atomically |
| `synonym_remove(term, alias)` | Remove one term/alias pair |
| `synonym_remove_term(term)` | Remove all aliases for a term |
| `synonym_list` | List all configured synonyms |
| `synonym_reset` | Clear all synonyms |

In global mode, tools that accept a `file` or `symbol` argument also accept an optional `repo` parameter to target a specific registered repo. Omitting `repo` searches all registered repos.

---

## CLI Commands

```
travsr init                          Index the repo, install git hook, register globally
travsr daemon start/stop/status      Start, stop, or check the background daemon
travsr repos                         List all globally registered repos
travsr status                        Show node/edge counts, schema version, last-indexed SHA
travsr ask <query>                   PPR + knapsack symbol lookup from the terminal
travsr graph <query>                 Show dependency graph for a symbol or file
travsr graph --all                   Show graph for the entire indexed repository
travsr mcp --stdio                   Start the MCP stdio server (single-repo, cwd-based)
travsr mcp --stdio --global          Start the MCP stdio server (all registered repos)
travsr mcp --stdio --db <path>       Start the MCP stdio server (explicit db path)
travsr lang list                     List all known Phase B language indexers and their status
travsr lang install <language>       Download and register a Phase B language indexer
travsr lang detect                   Scan the repo, detect supported languages, auto-install
travsr lang remove <language>        Unregister a Phase B language indexer
travsr lang approve <language>       Pre-approve a language that needs network access
travsr synonym add <term> <alias>    Add a query synonym
travsr synonym list                  List all configured synonyms
travsr synonym remove <term>         Remove a synonym term
travsr embed list                    List available embedding models
travsr embed init                    Initialize the embedding index for this repo
travsr embed status                  Show embedding index status
travsr embed reindex                 Rebuild the embedding index
travsr embed switch <model>          Switch to a different embedding model
```

### travsr graph

Visualise the dependency graph from any symbol or file as an ASCII tree,
Graphviz DOT, or structured JSON.

```bash
# ASCII tree (default): what does extension.ts import and define?
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
| `--all` | (none) | Dump the entire indexed graph (mutually exclusive with `<query>`) |

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

## VS Code Extension

Install the **Travsr** extension from the VS Code Marketplace (`travsr.travsr-vscode`).

The extension connects to your local Travsr daemon over MCP and adds:

- **Status bar**: daemon connection state and indexed node count
- **Code lens**: inline "N callers" annotations on function definitions
- **Hover**: dependency list on import statements
- **Graph panel**: interactive dependency graph rendered with Cytoscape.js; supports kind filtering and two-hop import traversal; open via the Travsr sidebar or the command palette (`Travsr: Show Graph`)

The extension uses your installed `travsr` binary. Set `travsr.binaryPath` in VS Code settings to override the binary location.

---

## Storage Backend

| Backend | Flag | Notes | Status |
|---|---|---|---|
| SQLite + WAL | _(default)_ | Zero setup, works everywhere | Available |

SQLite + WAL is the storage backend and requires no additional dependencies.
Kùzu was previously offered as an optional backend but was dropped
(see `docs/adrs/ADR-018-drop-kuzu-backend.md`). RocksDB remains a possible future
hyperscale backend.

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
              └─▶ SHA-256 delta: only re-indexes changed files
                    └─▶ graph.db updated, last_commit SHA recorded
```

**Graph stays current via the post-commit hook.** Every committed change is
re-indexed automatically. The graph is also fully queryable immediately after
`travsr init`, before any commit.

Language support: **TypeScript / TSX, Rust, Python, Go** (builtin, zero configuration). Additional languages (Java, Kotlin, C#, Scala, PHP, Ruby, Swift) are available as Phase B indexers via `travsr lang install`.

### Retrieval algorithms

| Algorithm | When used | Status |
|---|---|---|
| BFS depth-3 | `get_dependencies` / `get_callers` queries | Available |
| Personalized PageRank (PPR) | `get_context` and deep traversal | Available |
| PPR weighted | Score-aware PPR variant | Available |
| 0-1 Knapsack | Token budget cap on `get_context` results | Available |
| Prize-Collecting Steiner Tree (PCST) | `get_execution_path`: optimal path between two symbols | Available |
| k-core decomposition | Buried-middle recovery | Available |
| BM25 | Full-text ranked retrieval | Available |
| RBAC | Role-based access filtering on graph queries | Available |

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

**Release artifact signing:** Every release tarball is signed with
[cosign keyless signing](https://docs.sigstore.dev) using the GitHub Actions
OIDC token. SLSA v1.0 provenance is attached to every release via GitHub
attestations. See [SECURITY.md](SECURITY.md) for verification instructions.

**Supply chain auditing:** All Rust dependencies are audited on every CI run
with [cargo-deny](https://github.com/EmbarkStudios/cargo-deny) (CVE advisories,
license policy, banned crates). A nightly OSV scan checks for new CVEs against
`Cargo.lock` and `package-lock.json`.

---

## Build from Source

```bash
git clone https://github.com/Travsr-com/travsr
cd travsr

# Build (SQLite backend)
cargo build --release   # requires Rust 1.75+

# Override the npm-installed binary with a local build
cp target/release/travsr $(which travsr)
# or
export TRAVSR_BINARY=/path/to/travsr/target/release/travsr
```

**Platform support:** macOS (x86\_64 + arm64), Linux (x86\_64 + aarch64), Windows (x86\_64).
Pre-built binaries are available on the [Releases](https://github.com/Travsr-com/travsr/releases) page.

**MSRV:** Rust 1.75 (verified in CI on every commit).

---

## Troubleshooting

- **`not inside a git repository`**
  Run `git init` before `travsr init`.

- **`not initialized: run travsr init`**
  Run `travsr init` in the repo root before using `graph`, `ask`, `status`, or `mcp`.

- **MCP server returns empty results in `--global` mode`**
  Run `travsr repos` to verify the repo is registered and `Exists` shows `yes`.
  If missing, re-run `travsr init` in that repo.

- **Stale entries in `travsr repos` (Exists = no)**
  Safe to ignore; they are skipped automatically. They appear when a repo
  was deleted or moved after being indexed.

- **Binary not found after npm install?**
  Set `TRAVSR_BINARY=/path/to/travsr` to use a local build instead.

- **Corporate proxy blocks the postinstall download?**
  Same: set `TRAVSR_BINARY` to skip the remote fetch.

---

## Changelog

See [CHANGELOG.md](CHANGELOG.md) for the full release history.

---

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Issues and PRs welcome. Licensed under Apache 2.0.
