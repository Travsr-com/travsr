# RFC-018: AI Tool Auto-Configuration on `travsr init`

**Date:** 2026-06-07
**Status:** Proposed

## Context

Travsr's entire value proposition is that an AI agent traverses a live code graph
via MCP instead of doing text RAG. But to get there a user must, by hand:

1. Register the Travsr MCP server (`travsr mcp --stdio`) in whatever AI coding
   tool they use, in that tool's bespoke config format and location.
2. Separately instruct that tool to prefer Travsr's graph tools over `grep`/`find`.

Every AI tool has a different config file, schema, and scope (project vs global),
so step 1 is error-prone, and almost no user does step 2. The result: Travsr is
installed but the agent never actually uses it. The MCP wiring we already ship for
our own VS Code extension (`packages/travsr-vscode/src/mcp.ts` spawns
`travsr mcp --stdio`) proves the connection works — we just don't bootstrap it for
the third-party agents users already run.

`travsr init` is the natural place to fix this: the user is already in their repo,
the graph is being built, and we know the absolute path to the running `travsr`
binary (`std::env::current_exe()`).

## Decision

Extend `travsr init` to detect which AI coding tools are present and, for each,
(a) register the Travsr MCP server in that tool's config and (b) write a short
"use Travsr effectively" instructions/rules file. The same code path is also
exposed as a standalone `travsr connect` command.

### Write behavior

- **Project-scoped configs are auto-written.** They live inside the repo, are
  git-tracked, and are trivially reverted, so writing them during `init` is safe
  and zero-touch.
- **Global/user-level configs are never silently mutated.** For tools that only
  support a home-directory config (Windsurf), `init` prints the exact file path
  and JSON snippet to add rather than editing `~`. This honors the project's
  "confirm outward-facing / hard-to-reverse actions" rule.
- **Idempotent.** JSON configs are merged (the `travsr` server key is
  inserted/updated; all other servers are preserved). Markdown rules files use a
  delimited managed block (`<!-- travsr:begin -->` / `<!-- travsr:end -->`) that
  is replaced, never duplicated, on re-run.
- **Non-fatal.** Detection or write failure never fails `travsr init`, mirroring
  the existing `hint_lang_detect` pattern (`crates/travsr-cli/src/init.rs:39`).
- **Opt out** with `travsr init --no-connect`.

### Architecture

A new module `crates/travsr-cli/src/connect.rs` defines one adapter per tool:

```
struct ToolAdapter {
    name: &'static str,                          // "claude-code", "cursor", ...
    detect: fn(repo: &Path) -> bool,             // marker dir/file present?
    apply:  fn(repo: &Path, spec: &McpServerSpec) -> Result<Vec<PathBuf>>,
}
```

`McpServerSpec` carries the absolute travsr binary path plus args
`["mcp", "--stdio"]`, serialized per tool. Shared helpers:

- `merge_json_server(...)` — parse existing file (or `{}`) with `serde_json`,
  upsert only the `travsr` server, write back. `serde_json` is already a CLI dep.
- `write_managed_block(...)` — replace-or-append the delimited block in markdown.
- `TRAVSR_AGENT_GUIDE` — one canonical guidance string (lifted from the "Code
  Search: travsr MCP first" section of `.claude/CLAUDE.md`, listing
  `search_symbol`, `get_dependencies`, `get_callers`, `get_blast_radius`,
  `get_execution_path`, `get_repo_map`, `get_context`), reused by every adapter.

Initial adapters:

| Tool | Detect marker | MCP config (project) | Instructions file | Mode |
|------|---------------|----------------------|-------------------|------|
| Claude Code | `.claude/` or `CLAUDE.md` or `~/.claude/` | `.mcp.json` → `mcpServers.travsr` | managed block in `CLAUDE.md` | auto-write |
| Cursor | `.cursor/` or `~/.cursor/` | `.cursor/mcp.json` → `mcpServers.travsr` | `.cursor/rules/travsr.mdc` | auto-write |
| VS Code Copilot | `.vscode/` | `.vscode/mcp.json` → `servers.travsr` | managed block in `.github/copilot-instructions.md` | auto-write |
| Windsurf | `~/.codeium/windsurf/` | `~/.codeium/windsurf/mcp_config.json` | `.windsurfrules` | print snippet |
| Zed | `.zed/` or `~/.config/zed/` | `.zed/settings.json` → `context_servers.travsr` | `.rules` managed block | auto-write |

The adapter table makes adding a tool a few-line change.

### Wiring

- `main.rs`: add `Command::Connect { tool: Option<String>, list: bool, print: bool }`
  and a `--no-connect` flag on `Init`; add `mod connect;`.
- `init.rs`: after `hint_lang_detect`, call the connect path unless `--no-connect`,
  wrapped non-fatally. Reuse `find_git_root` (`repo.rs`) and `dirs::home_dir()`.

## Alternatives Considered

**Auto-write global configs too (incl. `~/.codeium`).** Rejected: silently editing
files outside the repo is hard to discover and revert, and violates the
confirm-outward-actions principle. Printing a snippet keeps the user in control.

**Separate `travsr connect` only, leave `init` untouched.** Rejected as the
default: it preserves the manual-wiring friction this RFC exists to remove. We
still ship `travsr connect` for re-running and targeting one tool, but `init`
performs the wiring automatically.

**Ship per-tool docs instead of code.** Rejected: documentation does not move the
"agent actually uses Travsr" needle; the whole point is zero manual setup.

**LLM-driven detection of the user's tool.** Rejected on Principle 1
(algorithms first, LLM last) — presence of a tool's config dir is a deterministic
filesystem check, no model needed.

## Consequences

- After `travsr init`, a user's existing agent (Claude Code, Cursor, Copilot) is
  immediately wired to the Travsr MCP server and told to prefer it over text
  search — no manual MCP config.
- New committable files may appear in the repo (`.mcp.json`, `.cursor/...`,
  `.vscode/mcp.json`). They are idempotent and `--no-connect` disables them.
- A small, well-isolated adapter module; each adapter is pure and unit-testable
  against a tempdir, matching the test style in `install.rs`.
- Maintenance: each tool's config schema can drift across versions; adapters must
  be kept current. Continue/Cline are deferred for this reason.
- No new dependencies.

## Out of Scope

- Continue and Cline adapters (config formats vary by version).
- Installing the AI tools themselves; we only configure tools already present.
- A `travsr connect --remove` uninstall path (possible follow-up).
