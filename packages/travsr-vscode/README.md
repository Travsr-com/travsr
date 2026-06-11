# Travsr for VS Code

**Graph-native code intelligence. Live call graph, blast radius, and callers — powered by a local daemon over MCP.**

Travsr replaces vector RAG with a deterministic graph. Every function call, import, and type reference becomes a graph edge. Ask "what calls this?", "what breaks if I change this?", or "show me the execution path" — and get exact, always-fresh answers.

## Features

- **Live call graph** — interactive Cytoscape.js graph panel showing dependencies and callers for the active file
- **Blast radius** — instantly see every file and function that depends on the current symbol; toggle between Tree-sitter (structural) and Semantic (SCIP-derived) edges
- **Callers view** — find all call sites for any function across the entire repo
- **Codelens** — inline blast radius and callers counts on every function definition across all 13 indexed languages
- **Hover cards** — callers and blast radius on symbol hover, available in all 13 indexed languages
- **Symbol search** — `Travsr: Ask Symbol` command searches the graph for any symbol by name
- **Synonym management** — `Travsr: Manage Synonyms` to define aliases for domain terms used in graph queries
- **Language manager** — `Travsr: Languages` to install, enable, and remove Phase B semantic indexers per language
- **Go-to-definition** — graph nodes carry line numbers; jump directly to any definition from the graph panel
- **MCP integration** — exposes `get_dependencies`, `get_callers`, `get_blast_radius`, `get_context`, `search_symbol`, and `get_repo_map` to Claude and other MCP-capable AI agents
- **Always fresh** — the graph updates on every git commit via hooks; no stale context
- **Zero cloud dependency** — the graph daemon runs locally; your code never leaves your machine
- **Auto-install** — the extension downloads and installs the correct travsr binary for your platform on first use

## Supported Languages

Phase A (Tree-sitter structural indexing — always available):

TypeScript, JavaScript, Rust, Python, Go, Java, Kotlin, Ruby, C#, PHP, Scala, C++, C, Swift, Dart

Phase B (SCIP semantic indexing — install via `Travsr: Languages`):

TypeScript, JavaScript, Python, Rust, Go, Java, Kotlin, C#, Scala, Swift, Dart

## Requirements

- VS Code 1.85 or later
- [Travsr daemon](https://travsr.com) — the extension auto-installs it on first use

## Getting Started

1. Install the extension
2. Open a git repository in VS Code
3. The Travsr daemon starts automatically and indexes your repo
4. Open the **Travsr** panel in the Activity Bar to see your live call graph
5. Use `Travsr: Show Callers` or `Travsr: Show Blast Radius` from the command palette

## Configuration

| Setting | Default | Description |
|---|---|---|
| `travsr.binaryPath` | (auto) | Path to the travsr binary. Leave empty for auto-detect. |
| `travsr.statusBarPosition` | `left` | Which side of the status bar the Travsr indicator appears on. |
| `travsr.mcpPath` | (auto) | Override the MCP socket path used to connect to the daemon. |
| `travsr.logLevel` | `info` | Log verbosity for the extension output channel. |
| `travsr.cloudEndpoint` | (empty) | Remote SSE endpoint URL. Leave empty to use the local daemon. |
| `travsr.telemetry.enabled` | `false` | Send anonymous usage events (opt-in). |

## Using with Claude / AI Agents

When the Travsr daemon is running, it exposes an MCP server. Claude Desktop and Claude Code can connect to it automatically. See [travsr.com](https://travsr.com) for setup instructions.

## Privacy

Telemetry is **off by default**. When enabled, only anonymous usage events are sent (tool names, connection failures). No file paths, symbol names, or code content is ever sent.

## License

MIT — [github.com/Travsr-com/travsr](https://github.com/Travsr-com/travsr)
