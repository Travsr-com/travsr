# RFC-026: AI Tool Auto-Configuration on `travsr init`

**Date:** 2026-06-07
**Status:** Proposed (pending principal-security-engineer sign-off, see Security)

> Renumbered from RFC-018 and moved out of `docs/adrs/`. This RFC took the 018
> slot first (2026-06-07), but `docs/rfcs/RFC-018-embedding-plugin-architecture.md`
> later claimed the same number and shipped, so 018 now points at implemented
> code from CI config, four sibling RFCs, and five crates. Renumbering the
> implemented one would break all of that; renumbering this one, still
> unimplemented at the time, cost three references. `docs/rfcs/` is the
> canonical home for RFCs, so the file moved there rather than staying beside
> the ADRs.

## Context

Travsr only delivers value when an AI agent traverses the code graph over MCP
instead of doing text RAG. Getting there is manual: the user must register the
`travsr mcp --stdio` server in their tool's bespoke config, then separately tell
that tool to prefer Travsr's graph tools over `grep`/`find`. Every tool has a
different file, schema, and scope, so almost nobody does it, and Travsr sits
installed-but-unused. Our own VS Code extension already proves the wiring works
(`packages/travsr-vscode/src/mcp.ts` spawns `travsr mcp --stdio`); we just never
bootstrap it for the third-party agents users already run.

`travsr init` is the place to fix this: the user is in their repo and the binary
is on disk.

## Decision

Extend `travsr init` to detect installed AI coding tools and, for each, do **two
co-equal things**: (1) register the Travsr MCP server, and (2) write an
always-on rules/instructions file that directs the agent to use Travsr **first**
for every code-structure question. Wiring the server without the rules leaves the
agent free to keep using `grep`/`find`/full-file reads — the instructions are what
actually change behavior, so they are mandatory, not a nice-to-have. The same code
path is exposed as standalone `travsr connect`.

### Write behavior

- **Local, not committed, by default.** Generated files are written into the
  working tree but added to `.gitignore` (a managed Travsr block). Committing an
  MCP server definition is an RCE-on-clone vector for downstream users
  (attacker-defined server auto-loads on clone), and baking a local path into
  shared history violates local-first. Users who *want* to share the config opt
  in with `travsr connect --commit`.
- **Bare command, not absolute path.** The server `command` is the bare string
  `travsr` whenever `~/.travsr/bin` is on `PATH` (reuse
  `install.rs::path_contains_travsr_bin`). This keeps output stable across
  machines and avoids leaking the home dir / username. Only when `travsr` is not
  on `PATH` do we fall back to the absolute `current_exe()` path *and* print the
  PATH-fix guidance the install flow already emits.
- **Global configs are never silently mutated.** For tools with only a
  home-directory config (Windsurf), `init` prints the file path and exact snippet
  to add rather than editing `~`.
- **Each adapter owns its exact schema.** There is no single shared serializer:
  Copilot uses top-level `servers` and requires `"type": "stdio"`; Cursor/Claude
  use `mcpServers`; Zed uses `context_servers`; Cursor rules need YAML frontmatter
  to activate. A shared helper only loads/merges/writes an opaque JSON value; the
  per-tool entry is built by that tool's adapter.
- **Idempotent and non-clobbering.** JSON merge upserts only the `travsr` key and
  preserves all other servers. If the target file exists but does not parse as
  strict JSON (hand-edited / JSONC with comments, common for `.vscode` and Zed),
  the adapter **skips and warns** — it never rewrites and never drops the user's
  content. Markdown rules use a single balanced
  `<!-- travsr:begin -->` / `<!-- travsr:end -->` block; malformed, duplicate, or
  nested markers cause a skip, never a destructive replace.
- **Non-fatal, but visible.** Failures never fail `travsr init` (mirrors
  `hint_lang_detect`, `init.rs:39`), but every file written or skipped is printed
  in a one-line summary so the behavior is auditable.
- **Opt out** with `travsr init --no-connect`. **Undo** with
  `travsr connect --remove` (ships in v1; the managed-block design makes clean
  removal cheap).

### Architecture

A new module `crates/travsr-cli/src/connect.rs`. Adapters are a small enum
dispatched by `match` (five fixed tools — no need for `fn`-pointer indirection):

```
enum Tool { ClaudeCode, Cursor, VsCodeCopilot, Windsurf, Zed }
impl Tool {
    fn detect(repo: &Path) -> Vec<Tool>;            // project markers first
    fn apply(&self, repo: &Path, cmd: &McpCommand)  // builds its own schema
        -> Result<Outcome>;                         // files written / skipped / printed
}
```

`McpCommand` = the resolved command string (`travsr` or absolute fallback) +
args `["mcp", "--stdio"]`. Shared helpers:

- `merge_json_value(path, mutate)` — load existing strict-JSON (or `{}`), apply a
  closure that upserts the `travsr` key, write back pretty-printed; **return
  `Skipped` on parse failure** instead of overwriting.
- `write_managed_block(path, body)` — replace the single balanced managed block or
  append; skip on malformed markers.
- `TRAVSR_AGENT_GUIDE` — one canonical, imperative guidance string (derived from
  the "Code Search: travsr MCP first" section of `.claude/CLAUDE.md`). It is not a
  passive "Travsr is available" note — it is a directive. The text is shared; each
  adapter wraps it in its own activation envelope (Cursor frontmatter, Copilot
  instructions file, `CLAUDE.md`/`.rules` block) so it is **always loaded**, not
  agent-requested. Canonical body:

  ```
  # Use Travsr first for all code questions
  This repo has a Travsr code graph served over MCP. For ANY question about code
  structure — definitions, callers, dependencies, impact/blast radius, call paths,
  or repo overview — ALWAYS query Travsr's MCP tools BEFORE grep/find/ripgrep or
  reading whole files. Travsr is the token-efficient, hallucination-free path.
  - search_symbol(name)        — find a definition
  - get_callers(symbol)        — who calls this
  - get_dependencies(file)     — what this depends on
  - get_blast_radius(file)     — what a change here affects
  - get_execution_path(a, b)   — how a reaches b
  - get_repo_map(repo)         — high-level structure
  - get_context(query, budget) — full PPR + knapsack retrieval
  Only fall back to text search when Travsr returns nothing or is unavailable.
  ```

  Adapters that support priority/always-on flags set them (Cursor
  `alwaysApply: true`; the directive lives in the tool's auto-loaded instruction
  file) so "always use Travsr" is enforced on every turn, not just when convenient.

Initial adapters:

| Tool | Detect marker (project-first) | MCP config | Server entry | Rules file |
|------|-------------------------------|-----------|--------------|-----------|
| Claude Code | `.claude/` or `CLAUDE.md` | `.mcp.json` → `mcpServers.travsr` | `{command, args}` | `CLAUDE.md` managed block |
| Cursor | `.cursor/` | `.cursor/mcp.json` → `mcpServers.travsr` | `{command, args}` | `.cursor/rules/travsr.mdc` **with `---\nalwaysApply: true\n---` frontmatter** |
| VS Code Copilot | existing `.vscode/mcp.json` (else print-snippet) | `.vscode/mcp.json` → `servers.travsr` | `{"type":"stdio", command, args}` | `.github/copilot-instructions.md` managed block |
| Windsurf | `~/.codeium/windsurf/` | **print only** `~/.codeium/windsurf/mcp_config.json` → `mcpServers` | `{command, args}` | `.windsurf/rules/travsr.md` (preferred over legacy `.windsurfrules`) |
| Zed | `.zed/` | `.zed/settings.json` → `context_servers.travsr` | Zed-pinned shape (verify flat `{command,args}` vs nested `{command:{path,args}}` against target version) | `.rules` managed block |

Detection uses **project-local markers** to decide auto-write; a tool known only
from a global marker (`~/.claude/`, `~/.cursor/`) is reported as print-snippet,
not auto-written into an unrelated repo. Bare `.vscode/` is *not* a Copilot
signal (present in most repos without Copilot) — Copilot auto-writes only when
`.vscode/mcp.json` already exists, otherwise it prints the snippet.

### Wiring

- `main.rs`: add `Command::Connect { tool: Option<String>, list: bool, print: bool, remove: bool, commit: bool }`, a `--no-connect` flag on `Init`, and `mod connect;`.
- `init.rs`: after `hint_lang_detect`, call the connect path unless `--no-connect`,
  wrapped non-fatally. Reuse `find_git_root` (`repo.rs:5`) and `dirs::home_dir()`.

## Security & Privacy

This RFC touches MCP registration, writes agent-instruction files, and resolves a
local binary path, so it is a security surface and **requires
principal-security-engineer sign-off before leaving Proposed**.

- **No committed MCP configs by default** (see Write behavior) — avoids amplifying
  the clone-and-pwn vector where a repo-defined MCP server auto-loads on clone
  (Cursor MCPoison; VS Code skipping the trust prompt for `mcp.json`-started
  servers; Claude Code `enableAllProjectMcpServers` consent bypass).
- **No path/username leakage** — bare `travsr` command in generated files; absolute
  path only in local, git-ignored fallback.
- **No clobber, no destructive merge** — strict-JSON-or-skip, single balanced
  managed block, never overwrite a pre-existing `travsr` key of a different shape.
- **Instruction-poisoning containment** — the guidance text is static and minimal;
  the managed-block writer refuses malformed/duplicate markers so a crafted host
  file cannot trick it into deleting or preserving attacker content.

## Alternatives Considered

**Commit MCP configs by default.** Rejected: being committable is exactly what
makes these files dangerous to downstream cloners and conflicts with local-first.
Opt-in `--commit` only.

**Absolute `current_exe()` path in generated files.** Rejected as the default:
non-portable across machines/CI and leaks the home dir; used only as the
not-on-PATH fallback.

**One shared MCP serializer for all tools.** Rejected: schemas genuinely diverge
(`servers` + `type` for Copilot, `context_servers` for Zed, frontmatter for
Cursor). Each adapter owns its schema; only load/merge/write is shared.

**Separate `travsr connect` only, leave `init` untouched.** Rejected as default:
preserves the manual-wiring friction this RFC removes. `connect` still ships for
re-running and single-tool targeting.

**LLM-driven tool detection.** Rejected on Principle 1 — presence of a config dir
is a deterministic filesystem check.

## Consequences

- After `travsr init`, the user's existing agent is both wired to Travsr and given
  an always-on directive to query it first for every code-structure question —
  zero manual MCP config and zero manual prompting. The rules file is what
  converts "Travsr is available" into "Travsr is actually used."
- Generated files are local and git-ignored; nothing machine-specific enters
  shared history.
- Small, isolated module; each adapter is pure and unit-testable against a
  tempdir (matching `install.rs` test style). No new dependencies; no crate-
  dependency-rule change (stays in `travsr-cli`).
- Maintenance: per-tool config schemas and rules-activation semantics drift across
  versions and must be kept current — adapters pin and verify shapes. Continue and
  Cline are deferred for this reason.

## Out of Scope

- Continue and Cline adapters (config formats vary by version).
- Installing the AI tools themselves; we only configure tools already present.
