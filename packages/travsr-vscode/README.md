# Travsr for VS Code

**Graph-native code intelligence. Live call graph, blast radius, and callers — without vector RAG.**

Travsr builds a deterministic graph of your codebase on every git commit and exposes it to VS Code via MCP. Instead of guessing from text chunks, it traverses real structure: call edges, import edges, type references.

---

## Features

### Live status bar

A real-time indicator in the bottom status bar shows the health of your local Travsr graph.

| State | Icon | When |
|---|---|---|
| Connecting | spinning sync | Extension just activated |
| Fresh | graph icon (green) | Graph is up-to-date |
| Indexing | spinning sync | File saved, re-indexing in progress |
| Stale | warning (amber) | Daemon returned an empty map |
| Disconnected | error (red) | Daemon process not running |

The status bar polls every 30 seconds. Saving a file triggers an immediate re-query after 2 seconds. Click the item to see the current connection state.

### Blast radius code lens

A code lens appears at the top of every `.ts`, `.tsx`, `.rs`, and `.py` file showing how many files would be affected if this file changed:

```
🩻 blast: 12 files
```

Click to open the full affected-file list in a panel. Counts above 99 display as `99+`. The lens is omitted when the count is zero or the daemon is unavailable.

### Callers and blast radius hover

Hovering over any symbol in a supported file shows an inline card with:

- The top 5 callers of that symbol, with `… and N more` when there are more
- The blast radius count for the containing file
- Quick links to open the full callers list or blast radius panel

All three features degrade gracefully — if the Travsr daemon is not running, they silently do nothing rather than showing errors.

---

## Requirements

- The `travsr` binary installed and available on your `PATH`, or path set via the `travsr.binaryPath` setting.
- VS Code 1.85.0 or later.

Install the CLI:

```sh
npm install -g travsr
```

Then initialise your repo:

```sh
cd your-repo
travsr init
```

---

## Extension Settings

| Setting | Default | Description |
|---|---|---|
| `travsr.binaryPath` | `"travsr"` | Path to the travsr binary. Set to an absolute path if travsr is not on your PATH. |

---

## How It Works

The extension spawns `travsr mcp --stdio` as a child process and communicates over JSON-RPC 2.0 (MCP stdio transport). It calls three MCP tools:

| MCP Tool | Used By |
|---|---|
| `get_repo_map` | Status bar — polled every 30 s |
| `get_blast_radius(file)` | Code lens + hover card |
| `get_callers(symbol)` | Hover card callers list |

The MCP client multiplexes requests by id over a single stdin/stdout pipe. All network errors surface as empty results so the extension never shows error popups in normal usage.

```
VS Code Extension Host
  StdioMcpClient  ──stdio──►  travsr mcp --stdio
    │
    ├── StatusBarProvider      (polls get_repo_map every 30 s)
    ├── BlastRadiusCodeLens    (get_blast_radius on file open / save)
    └── CallersHoverProvider   (get_callers + get_blast_radius on hover)
```

---

## Supported Languages

| Language | Status |
|---|---|
| TypeScript / TSX | Supported |
| Rust | Supported |
| Python | Supported |
| Go | Supported |

---

## Links

- [travsr.com](https://travsr.com)
- [Documentation](https://docs.travsr.com)
- [GitHub](https://github.com/raj-rkv/travsr)
- [Report an issue](https://github.com/raj-rkv/travsr/issues)

---

## License

MIT
