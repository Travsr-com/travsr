# travsr

**The code graph that lives next to git.**

> Source code is a deterministic graph, not unstructured text. Travsr builds
> that graph on every commit and exposes it via MCP so AI agents traverse real
> edges instead of guessing from vector chunks.

[![CI](https://github.com/Travsr-com/travsr/actions/workflows/ci.yml/badge.svg)](https://github.com/Travsr-com/travsr/actions/workflows/ci.yml)
[![npm](https://img.shields.io/npm/v/%40travsr.com%2Ftravsr)](https://www.npmjs.com/package/@travsr.com/travsr)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)

---

## Quickstart

```bash
# 1. Install
npm install -g @travsr.com/travsr

# 2. Index your repo
cd your-project
travsr init

# 3. Connect to Claude Desktop
```

Add to `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS)
or `%APPDATA%\Claude\claude_desktop_config.json` (Windows):

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

Restart Claude Desktop. Ask: *"Who calls PaymentService.charge?"*

---

## Works with Every MCP-Compatible AI Tool

Travsr speaks [MCP](https://modelcontextprotocol.io) - the open standard for
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

Then ask Cursor: *"What does PaymentService depend on?"*

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

The pattern is always the same - point your client at `travsr mcp --stdio`.
Travsr handles the rest.

```
command: travsr
args:    ["mcp", "--stdio"]
type:    stdio
```

---

## MCP Tools

| Tool | Description |
|---|---|
| `get_dependencies(file)` | Return all imports/dependencies of a file |
| `get_callers(symbol)` | Return all nodes with an incoming edge to a symbol |

---

## CLI Commands

```
travsr init          Index the current repo and install the git hook
travsr status        Show graph stats and last-indexed commit SHA
travsr ask <query>   BFS lookup from the terminal (no MCP client needed)
travsr mcp --stdio   Start the MCP stdio server (used by Claude Desktop)
```

---

## How It Works

```
git commit
  └─▶ post-commit hook
        └─▶ travsr hook-run <changed files>
              └─▶ SHA256 delta - only reindex changed files
                    └─▶ SQLite graph DB  (.travsr/graph.db)
                          └─▶ MCP stdio server reads on demand
```

Language support: **TypeScript / TSX**. Python, Go, Rust arriving in Phase 2.

---

## Build from Source

```bash
git clone https://github.com/Travsr-com/travsr
cd travsr
cargo build --release   # requires Rust 1.75+
```

---

## Troubleshooting

- **Binary not found after install?**  
  Set `TRAVSR_BINARY=/path/to/travsr` to point to a local build.
- **Corporate proxy blocks the postinstall download?**  
  Same - set `TRAVSR_BINARY` to skip the remote fetch.

---

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Issues and PRs welcome. Licensed under Apache 2.0.
