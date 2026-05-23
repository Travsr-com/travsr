# Travsr for VS Code

![Status](https://img.shields.io/badge/status-sprint--11-blue)
![Node](https://img.shields.io/badge/node-%3E%3D22-brightgreen)
![License](https://img.shields.io/badge/license-MIT-blue)

Graph-native code intelligence for VS Code, powered by the local Travsr daemon over MCP stdio.

## Features

**Status bar (VSCODE-201)**

A live indicator in the bottom-left status bar shows the health of your local Travsr graph.

| State | Icon | Colour | When |
|---|---|---|---|
| Connecting | `$(sync~spin)` | default | Extension just activated |
| Fresh | `$(graph)` | green `#7cf2c5` | Graph is up-to-date |
| Indexing | `$(sync~spin)` | default | File saved, re-indexing |
| Stale | `$(warning)` | amber `#f59e0b` | Daemon returned empty map |
| Disconnected | `$(error)` | red `#ef4444` | Daemon unavailable |

The status bar polls `get_repo_map` every 30 seconds. Saving a file switches it to "indexing" and re-queries after 2 seconds. Click the item to see connection status.

**Blast radius code lens (VSCODE-202)**

A code lens appears at the top of every `.ts`, `.tsx`, `.rs`, and `.py` file:

```
🩻 blast: 12 files
```

Shows how many files would be affected if this file changed. Click to open the full affected-file list. The lens is omitted when the count is zero or the daemon is unavailable. Counts above 99 display as `99+`.

**Callers and blast radius hover (VSCODE-203)**

Hovering over any symbol in a supported file opens a card showing:

- The top 5 callers of that symbol (with `… and N more` when there are more)
- The blast radius count for the containing file
- Quick-action links to open the full callers list or blast radius panel

## Requirements

- The `travsr` binary must be installed and on your `PATH`, or the path configured via `travsr.binaryPath`.
- Node.js 22 or later (for extension development).
- VS Code 1.85.0 or later.

## Extension Settings

| Setting | Default | Description |
|---|---|---|
| `travsr.binaryPath` | `"travsr"` | Path to the travsr binary. Must be on PATH or set to an absolute path. |

## MCP Protocol

The extension spawns `travsr mcp --stdio` as a child process and communicates over JSON-RPC 2.0. It calls three MCP tools:

| Tool | Used by |
|---|---|
| `get_repo_map` | Status bar — polls every 30 s |
| `get_blast_radius(file)` | Code lens + hover card |
| `get_callers(symbol)` | Hover card |

## Development

```bash
# Install dependencies (Node 22 required)
npm ci

# Type-check
npm run lint

# Compile
npm run compile

# Run tests (requires a display; use xvfb-run on headless Linux)
npm test
```

Tests use `@vscode/test-electron` and run inside a real VS Code instance. The suite covers all three providers: status bar selector, code lens pipeline (provideCodeLenses → resolveCodeLens), and hover card rendering.

## Architecture

```
VS Code Extension Host
  StdioMcpClient  ──stdio──►  travsr mcp --stdio
    │
    ├── createStatusBarItem   (polls get_repo_map)
    ├── BlastRadiusCodeLensProvider  (get_blast_radius per file)
    └── CallersHoverProvider  (get_callers + get_blast_radius per symbol)
```

The MCP client multiplexes JSON-RPC requests by id over a single stdin/stdout pipe. All errors surface as empty strings so providers always degrade gracefully when the daemon is unavailable.

## License

MIT
