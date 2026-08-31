# Travsr for VS Code

**Graph-native code intelligence. Live call graph, blast radius, and callers, powered by a local daemon over MCP.**

Travsr replaces vector RAG with a deterministic graph. Every function call, import, field read and type reference becomes a graph edge. Ask "what calls this?", "what breaks if I change this?", or "show me the execution path", and get exact, always-fresh answers.

## Features

- **Live call graph**: an interactive graph webview (Cytoscape) for the active file or any symbol, with kind filtering, two-hop traversal, node search, and a detail panel for the selected node
- **Diagnostics overlay**: the problems your language extensions report, drawn onto the graph, so a file no extension has looked at is shown as not diagnosed rather than clean
- **Blast radius**: every file and function that depends on the current symbol, transitively, in all supported languages
- **Callers and dependencies**: all call sites for any function across the repo, and the imports of any file, in the sidebar tree
- **Context Explorer**: graph-ranked context for a natural-language query, grouped by whether a result matched exactly, semantically, or through the graph
- **Code lens and hover**: inline "N callers" counts on definitions, a dependency list on imports, and a high-blast warning before you edit a high-impact file
- **Languages panel**: which languages have full cross-file analysis on this machine and in this repository, what each one still needs, and a one-click install for the ones that are missing
- **Stats panel**: index and daemon health, plus a searchable daemon log with severity filters, a per-day file picker, and optional auto-refresh
- **Multi-repo**: pick the active repository once when several are open; installs and re-indexes go where you meant them to
- **MCP integration**: registers the local Travsr MCP server with Claude Desktop, Cursor and Continue, so the same graph answers your agent's questions
- **Always fresh**: the graph updates on every git commit via hooks, and on save through the daemon's watcher; no stale context
- **Zero cloud dependency**: the daemon runs locally and your code never leaves your machine

## Requirements

- VS Code 1.85 or later
- The `travsr` binary. The extension resolves it from `travsr.binaryPath`, then `~/.travsr/bin`, then PATH, and offers to download a verified release build if none of those work.
- travsr v1.0.0 or later is recommended. An older binary does not report everything the Languages panel needs; the panel detects that and says so, rather than rendering the gaps as answers.

## Getting Started

1. Install the extension
2. Open a git repository in VS Code
3. The Travsr daemon starts automatically and indexes your repo
4. Open the **Travsr** panel in the Activity Bar to see your live call graph
5. Use `Travsr: Show Callers` or `Travsr: Show Blast Radius` from the command palette

For full cross-file analysis in languages beyond TypeScript, JavaScript, Python and Rust, open `Travsr: Languages` and install the analyzer for the ones you use. The panel names any tool the analyzer needs first (a JDK, Node.js, and so on) and what is already available on this machine.

## Commands

| Command | What it does |
|---|---|
| `Travsr: Show Graph` | Interactive graph for the active file or a symbol you name |
| `Travsr: Show Callers` | Every call site of the symbol under the cursor |
| `Travsr: Show Blast Radius` | Everything transitively affected if this file or symbol changes |
| `Travsr: Show Dependencies` | What the active file imports |
| `Travsr: Show Execution Path` | Lowest-cost path between two symbols, with nearby context |
| `Travsr: Open Context Explorer` | Graph-ranked context for a natural-language query |
| `Travsr: Ask Symbol` | Look a symbol up by name across the graph |
| `Travsr: Languages` | Language analyzer status, prerequisites, and installs |
| `Travsr: Graph Stats` | Index health, daemon status, and the daemon log |
| `Travsr: Select Repository` | Choose which open repo actions target |
| `Travsr: Registered Repos` | Every repo in the global registry |
| `Travsr: Re-index Now` | Re-index the active repository |
| `Travsr: Register MCP Server in Agent` | Wire the local MCP server into Claude Desktop, Cursor or Continue |
| `Travsr: Copy Graph Context for Chat` | Copy budget-capped graph context for pasting into a chat |
| `Travsr: Manage Synonyms` | Teach Travsr that two names mean the same thing |

## Configuration

| Setting | Default | Description |
|---|---|---|
| `travsr.binaryPath` | (auto) | Absolute path to the travsr binary. Leave empty to auto-detect from PATH or use the built-in installer. |
| `travsr.statusBarPosition` | `left` | Which side of the status bar the Travsr indicator appears on. |
| `travsr.cloudEndpoint` | (empty) | Remote SSE endpoint URL. Leave empty to use the local daemon. |
| `travsr.telemetry.enabled` | `false` | Send anonymous usage events (opt-in). |
| `travsr.contextTokenBudget` | `2000` | Token budget for "Copy Graph Context for Chat" pulls. |
| `travsr.suggestSynonyms` | `true` | Offer to record a synonym when the symbol you pick differs from what you typed. |
| `travsr.blastRiskThreshold` | `20` | Dependent-file count at which the blast-radius code lens escalates to a high-risk warning. |

## Using with Claude / AI Agents

When the Travsr daemon is running it exposes an MCP server. `Travsr: Register MCP Server in Agent` writes the server definition for Claude Desktop, Cursor and Continue after validating that the binary it names is actually runnable. From a terminal, `travsr connect` wires up every AI coding tool it detects. See [travsr.com](https://travsr.com) for setup instructions.

## Privacy

Telemetry is **off by default**. When enabled, only anonymous usage events are sent (tool names, connection failures). No file paths, symbol names, or code content is ever sent.

## License

Apache-2.0 - [github.com/Travsr-com/travsr](https://github.com/Travsr-com/travsr)
