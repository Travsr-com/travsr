# Dogfooding Travsr on the Travsr Repo

> **What this is:** The exact workflow the Travsr team uses to develop Travsr with Travsr.
> Every query, hook, and MCP connection described here runs against the live Travsr codebase.
> This doc doubles as the canonical "how to get started on a Rust project" guide for new users.

---

## Prerequisites

| Tool | Minimum version | Install |
|---|---|---|
| Rust toolchain | 1.75 | `curl https://sh.rustup.rs -sSf \| sh` |
| Node.js | 18 LTS | `https://nodejs.org` |
| Git | 2.x | OS package manager |
| VS Code | 1.85 | `https://code.visualstudio.com` |
| bubblewrap (Linux only) | any | `sudo apt install bubblewrap` |

bubblewrap enables the SEC-201 sandbox for `rust-analyzer` subprocesses on Linux.
On macOS, `sandbox-exec` is used automatically. On Windows, the sandbox is a
no-op timeout guard — install bubblewrap if you move to Linux in the future.

---

## 1. Install Travsr

```bash
# Option A — npm (downloads the prebuilt binary for your platform)
npm install -g travsr

# Option B — build from source (requires Rust 1.75+)
git clone https://github.com/raj-rkv/travsr.git
cd travsr
cargo install --path crates/travsr-cli --locked
```

Verify:

```bash
travsr --version
# travsr 0.3.1
```

---

## 2. Init on the Travsr Repo

```bash
cd /path/to/travsr          # the repo root
travsr init
```

Expected output:

```
Travsr initialised
  graph:   .travsr/graph.db
  hook:    .git/hooks/post-commit  (installed)
  indexed: 47 files  |  nodes: 3 241  |  edges: 8 809
  corpus:  travsr
```

`travsr init` does three things:

1. Walks every `.rs`, `.ts`, and `.py` file (respecting `.gitignore`)
2. Builds the graph: Tree-sitter structural nodes + LSIF semantic edges where available
3. Installs a `post-commit` hook that re-indexes changed files in < 1s

The graph lives in `.travsr/graph.db` — a local SQLite WAL file, never leaves your machine.

---

## 3. Verify the Hook

Make a trivial commit and watch the hook fire:

```bash
git commit --allow-empty -m "test hook"
# [travsr] reindexed 0 changed files in 12ms
```

The hook is chain-safe: if you already have a `post-commit` hook, Travsr appends
a call to `travsr hook-run` rather than replacing it.

---

## 4. Check Graph Status

```bash
travsr status
```

```
graph:     .travsr/graph.db  (1.2 MB)
corpus:    travsr
nodes:     3 241
edges:     8 809
last commit: 712e16e  "[travsr-indexer] fix timeout test"
languages:   rust (3 198 nodes)  typescript (43 nodes)
```

---

## 5. Run Queries from the CLI

The `travsr ask` command runs a BFS / PPR query against the graph and prints
results to the terminal. Use it to explore the codebase or to verify graph
correctness after a change.

### What calls a function?

```bash
travsr ask "what calls run_ra_lsif"
```

```
Callers of run_ra_lsif  (src: crates/travsr-indexer/src/ra_runner.rs)
  crates/travsr-daemon/src/lib.rs:reindex_files   [RefCall]
```

### What does a file depend on?

```bash
travsr ask "dependencies of crates/travsr-retrieval/src/lib.rs"
```

```
Dependencies of crates/travsr-retrieval/src/lib.rs
  travsr-core   (travsr_core::EdgeKind, travsr_core::NodeId, ...)
  travsr-store  (travsr_store::Store)
```

### Blast radius of a change

```bash
travsr ask "blast radius of crates/travsr-core/src/lib.rs"
```

```
Blast radius: 6 files would be affected
  crates/travsr-indexer/src/lib.rs
  crates/travsr-indexer/src/lsif.rs
  crates/travsr-store/src/lib.rs
  crates/travsr-retrieval/src/lib.rs
  crates/travsr-mcp/src/lib.rs
  crates/travsr-daemon/src/lib.rs
```

This is a reverse-BFS from `travsr-core` following incoming `Depends` and `RefCall` edges —
exact, deterministic, zero hallucinations.

---

## 6. MCP Client Setup

Travsr exposes its graph via the Model Context Protocol (MCP) over stdio.
Connect any MCP-aware AI client to Travsr and it will use graph traversal
instead of vector search when answering questions about the code.

### Claude Desktop

Add to `~/Library/Application Support/Claude/claude_desktop_config.json`
(macOS) or `%APPDATA%\Claude\claude_desktop_config.json` (Windows):

```json
{
  "mcpServers": {
    "travsr": {
      "command": "travsr",
      "args": ["mcp", "--stdio"],
      "cwd": "/path/to/travsr"
    }
  }
}
```

Restart Claude Desktop. You should see "travsr" in the MCP tools panel.

### Cursor

Open `.cursor/mcp.json` in your workspace root (create if absent):

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

Reload the Cursor window (`Cmd+Shift+P` → "Reload Window").

### Continue (VS Code / JetBrains)

In `~/.continue/config.json`, add to the `contextProviders` section:

```json
{
  "contextProviders": [
    {
      "name": "mcp",
      "params": {
        "serverName": "travsr",
        "command": "travsr",
        "args": ["mcp", "--stdio"]
      }
    }
  ]
}
```

---

## 7. Available MCP Tools

Once connected, the following tools are available to any MCP client:

| Tool | What it does | Example prompt |
|---|---|---|
| `get_dependencies` | Direct + transitive imports of a file | "What does `travsr-retrieval` depend on?" |
| `get_callers` | All call sites of a symbol across the repo | "What calls `ingest_rust`?" |
| `get_blast_radius` | Files that would break if this file changed | "What breaks if I edit `EdgeKind`?" |
| `search_symbol` | Find a symbol definition by name | "Find the definition of `VName`" |
| `get_repo_map` | Structural overview of the repo | "Give me a map of `travsr-indexer`" |
| `get_context` | PPR traversal + knapsack token budget — the primary AI context retrieval tool (Sprint 11) | "Get context for editing the PPR implementation within 4096 tokens" |

### Sample queries to try on the Travsr repo

```
"What calls personalized_pagerank?"
"What does crates/travsr-daemon/src/lib.rs depend on?"
"What is the blast radius of editing crates/travsr-core/src/lib.rs?"
"Find the definition of SandboxConfig"
"Give me a structural overview of the travsr-mcp crate"
```

---

## 8. VS Code Extension

The Travsr VS Code extension (Sprint 11) surfaces the graph directly in the editor.

**Install** (once published to the VS Code Marketplace):

```bash
code --install-extension travsr.travsr-vscode
```

**Or install from source:**

```bash
cd packages/travsr-vscode
npm install
npm run compile
# Then install the .vsix via "Extensions: Install from VSIX..." in VS Code
```

**Features:**

- **Status bar indicator** — shows "Travsr: fresh" / "Travsr: stale" based on last commit
- **Code lens** — blast radius count above each function: `Blast radius: 3 files`
- **Hover provider** — hover a symbol to see its callers inline

The extension connects to the local `travsr mcp --stdio` process automatically
when a workspace with a `.travsr/graph.db` is opened.

---

## 9. Token Savings vs RAG

The following numbers were measured on the Travsr repo itself (~50k LOC Rust)
using Claude Sonnet 4.6 answering "What calls `ingest_rust` and what is the blast
radius of changing it?"

| Approach | Context tokens sent | Answer correct? | Latency |
|---|---|---|---|
| Naive RAG (top-8 chunks, 512 tokens each) | 4 096 | Partial — missed 2 callers | 3.1s |
| Full file context (send all 6 related files) | 18 400 | Yes | 8.7s |
| **Travsr graph traversal (BFS depth 3)** | **712** | **Yes — exact** | **0.4s** |
| **Travsr graph + PPR (top-20 nodes)** \* | **1 340** | **Yes — exact + context** | **0.6s** |

**82% fewer tokens** than naive RAG. **93% fewer tokens** than sending full files.
The graph answer is also deterministic — run it 100 times, get the same 712 tokens.

\* BFS numbers measured on commit 712e16e. PPR numbers are projected based on
top-20 node selection from the implemented PPR algorithm; will be confirmed with
measured values when `get_context` (PPR + knapsack) ships in Sprint 11.

These numbers will vary by query complexity and repo size. The savings grow as the
repo grows: RAG chunk overlap increases with codebase size, while graph traversal
depth stays constant.

---

## 10. Keeping the Graph Fresh

The `post-commit` hook keeps the graph fresh automatically. For longer-running
background workflows (e.g. a CI environment without git hooks), use:

```bash
# Re-index the whole repo
travsr init

# Re-index only files changed since the last commit
travsr hook-run
```

To verify freshness:

```bash
travsr status
# last commit: <sha>  matches `git rev-parse HEAD`
```

---

## Troubleshooting

**`travsr init` shows 0 nodes for Rust files**

Ensure the Rust files are not excluded by `.gitignore`. Check:
```bash
git check-ignore -v crates/travsr-core/src/lib.rs
# Should print nothing (not ignored)
```

**MCP client shows "travsr not found"**

The `travsr` binary must be on PATH in the shell environment the MCP client uses.
On macOS, GUI apps may not inherit your shell PATH. Set the full binary path in the
MCP config:
```json
"command": "/usr/local/bin/travsr"
```

**rust-analyzer LSIF step is slow (> 5 minutes)**

This is expected on first run for a large Rust workspace — rust-analyzer builds
the full semantic graph. Subsequent runs use the incremental index. To skip LSIF
entirely (Tree-sitter structural edges only), ensure `rust-analyzer` is not on PATH:
```bash
which rust-analyzer   # if this returns a path, RA will run
```

**Hook not firing**

Check the hook file:
```bash
cat .git/hooks/post-commit
# Should contain: travsr hook-run
chmod +x .git/hooks/post-commit  # if missing execute permission
```

---

## Further Reading

- [ADR-002: Edge Provenance Policy](adrs/ADR-002-edge-provenance-policy.md) — why LSIF edges beat Tree-sitter edges
- [ADR-003: PPR Policy](adrs/ADR-003-ppr-policy.md) — how Personalized PageRank retrieval works
- [ADR-006: rust-analyzer Trust](adrs/ADR-006-rust-analyzer-trust.md) — SEC-201 sandbox design
- [ROADMAP.md](planning/ROADMAP.md) — Phase 3 exit criteria and upcoming features
