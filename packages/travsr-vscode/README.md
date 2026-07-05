# Travsr for VS Code

**Graph-native code intelligence. Live call graph, blast radius, and callers - powered by a local daemon over MCP.**

Travsr replaces vector RAG with a deterministic graph. Every function call, import, and type reference becomes a graph edge. Ask "what calls this?", "what breaks if I change this?", or "show me the execution path" - and get exact, always-fresh answers.

## Features

- **Live call graph** - sidebar panel showing the call graph for the active file
- **Blast radius** - instantly see every file and function that depends on the current symbol
- **Callers view** - find all call sites for any function across the entire repo
- **MCP integration** - exposes `get_dependencies`, `get_callers`, `get_blast_radius`, `get_context` to Claude and other MCP-capable AI agents
- **Always fresh** - the graph updates on every git commit via hooks; no stale context
- **Zero cloud dependency** - the graph daemon runs locally; your code never leaves your machine

## Requirements

- VS Code 1.85 or later
- [Travsr daemon](https://travsr.com) - the extension auto-installs it on first use

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
| `travsr.cloudEndpoint` | (empty) | Remote SSE endpoint URL. Leave empty to use the local daemon. |
| `travsr.telemetry.enabled` | `false` | Send anonymous usage events (opt-in). |

## Using with Claude / AI Agents

When the Travsr daemon is running, it exposes an MCP server. Claude Desktop and Claude Code can connect to it automatically. See [docs.travsr.com](https://travsr.com) for setup instructions.

## Privacy

Telemetry is **off by default**. When enabled, only anonymous usage events are sent (tool names, connection failures). No file paths, symbol names, or code content is ever sent.

## License

Apache-2.0 - [github.com/raj-rkv/travsr](https://github.com/raj-rkv/travsr)
