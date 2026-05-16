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

# 3. Connect to Claude Desktop
```

Add to `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS)
or `%APPDATA%\Claude\claude_desktop_config.json` (Windows):

```json
{
  "mcpServers": {
    "travsr": {
      "command": "travsr",
      "args": ["mcp", "--stdio"],
      "cwd": "/path/to/your-project"
    }
  }
}
```

Restart Claude Desktop. Ask: *"Who calls PaymentService.charge?"*

---

## Works with Every MCP-Compatible AI Tool

Travsr speaks [MCP](https://modelcontextprotocol.io) — the open standard for
connecting AI agents to tools. The same `travsr mcp --stdio` command works
with any client that supports it.

### Cursor

Add to `~/.cursor/mcp.json` (global) or `.cursor/mcp.json` (per-project):

```json
{
  "mcpServers": {
    "travsr": {
      "command": "travsr",
      "args": ["mcp", "--stdio"]
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
      "args": ["mcp", "--stdio"]
    }
  }
}
```

Copilot Chat will automatically discover the `get_dependencies` and
`get_callers` tools and invoke them when answering questions about your code.

### Cline (VS Code extension)

In the Cline extension settings → **MCP Servers** → Add server:

```json
{
  "travsr": {
    "command": "travsr",
    "args": ["mcp", "--stdio"],
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
      "args": ["mcp", "--stdio"]
    }
  ]
}
```

### Any other MCP client

The pattern is always the same — point your client at `travsr mcp --stdio`.
Travsr handles the rest.

```
command: travsr
args:    ["mcp", "--stdio"]
type:    stdio
```

---

## MCP Tools

| Tool | Input | Description |
|---|---|---|
| `get_dependencies(file)` | File path (partial match) | Return all imports/dependencies of a file |
| `get_callers(symbol)` | Symbol name (partial match) | Return all nodes with an incoming edge to a symbol |

---

## CLI Commands

```
travsr init                    Index the repo and install the git post-commit hook
travsr status                  Show node/edge counts, schema version, and last-indexed commit SHA
travsr ask <query>             BFS symbol lookup from the terminal (partial match supported)
travsr mcp --stdio             Start the MCP stdio server
travsr graph <query>           Show dependency graph for a symbol or file
travsr graph --all             Show graph for the entire indexed repository
```

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
travsr graph --all --format json   # full repo as structured JSON
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

## How It Works

```
git init && travsr init
  └─▶ walks .ts / .tsx files (respects .gitignore)
        └─▶ Tree-sitter parses each file
              └─▶ Nodes + edges → .travsr/graph.db (SQLite WAL)
                    └─▶ post-commit hook installed

git commit
  └─▶ post-commit hook fires
        └─▶ travsr hook-run <changed files>
              └─▶ SHA-256 delta — only re-indexes changed files
                    └─▶ graph.db updated, last_commit SHA recorded
```

**Graph stays current via the post-commit hook** — every committed change is
re-indexed automatically. The graph is also fully queryable immediately after
`travsr init`, before any commit.

Language support: **TypeScript / TSX**. Python, Go, Rust arriving in Phase 2.

### Edge kinds

| Kind | Meaning |
|---|---|
| `depends` | File imports another module |
| `defines/binding` | File or class defines a symbol (function, method, variable) |
| `ref/call` | Call-site reference |
| `exports` | Symbol exported from a module |

---

## Build from Source

```bash
git clone https://github.com/raj-rkv/travsr
cd travsr
cargo build --release   # requires Rust 1.75+

# Use local build instead of the npm-installed binary
export TRAVSR_BINARY=/path/to/travsr/target/release/travsr
```

---

## Troubleshooting

- **`not inside a git repository`**  
  Run `git init` before `travsr init`.

- **`not initialized — run travsr init`**  
  Run `travsr init` in the repo root before using `graph`, `ask`, `status`, or `mcp`.

- **MCP server returns empty results**  
  Make sure `cwd` in your MCP client config points to the root of the indexed repo.

- **Binary not found after npm install?**  
  Set `TRAVSR_BINARY=/path/to/travsr` to use a local build instead.

- **Corporate proxy blocks the postinstall download?**  
  Same — set `TRAVSR_BINARY` to skip the remote fetch.

---

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Issues and PRs welcome. Licensed MIT.
