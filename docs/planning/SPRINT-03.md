# Sprint 3 — MCP Server (stdio) + CLI Polish + npm Package

- **Dates:** 2026-06-15 (Mon) to 2026-06-26 (Fri)
- **Phase:** 1 (MVP) — closes Phase 1 and gates the public launch
- **Sprint goal:** Expose the graph through the only sanctioned external interface (MCP, per `CLAUDE.md` principle 4), ship `travsr` on npm as a thin wrapper around the prebuilt Rust binary, and prove the loop works end-to-end in Claude Desktop on macOS and Linux.

---

## Stories

| ID | Area | Story | Acceptance |
|---|---|---|---|
| S3-1 | `travsr-mcp` | stdio JSON-RPC 2.0 handler implementing the MCP server spec: `initialize`, `tools/list`, `tools/call`. Register two tools: `get_dependencies(file)` and `get_callers(symbol)`. Errors mapped to JSON-RPC error codes. | Conformance test sends `initialize` and both tool calls over a piped child process; responses validate against the MCP schema. |
| S3-2 | `travsr-cli` | `travsr mcp --stdio` entrypoint: spawns the MCP server, binds stdin/stdout, logs to stderr only. Honors `--db <path>` and `--repo <root>` for non-default layouts. | Manual run: `travsr mcp --stdio` round-trips a tool call from a test client. Logs never leak to stdout. |
| S3-3 | packaging | npm wrapper package `travsr`: postinstall downloads the matching prebuilt Rust binary from GitHub Releases (linux-x64, linux-arm64, macos-x64, macos-arm64), verifies SHA256, places it on PATH. Falls back with a clear error on unsupported triples. | `npm install -g travsr` on macOS and Linux yields a working `travsr` on PATH; checksum mismatch aborts install. |
| S3-4 | docs | README quickstart: install, init, Claude Desktop config snippet, the two MCP tools. CONTRIBUTING.md, LICENSE (MIT), CODE_OF_CONDUCT. | A new user can copy-paste their way to a working Claude Desktop integration in under 5 minutes. |
| S3-5 | QA | Manual Claude Desktop integration test on macOS and Linux; record a 90-second screen capture for launch. | Claude Desktop lists `get_dependencies` and `get_callers`; both return correct answers on the fixture. |

---

## Definition of Done

- [ ] `npm install -g travsr` works on macOS (arm64, x64) and Linux (arm64, x64)
- [ ] `travsr init` is idempotent in a fresh clone
- [ ] The README `claude_desktop_config.json` snippet works verbatim
- [ ] MCP stdio server passes the conformance test
- [ ] `cargo test --workspace` and `cargo clippy -- -D warnings` are green on the CI matrix
- [ ] Release artifacts attached to a GitHub Release tag (`v0.1.0`) with SHA256SUMS
- [ ] No `unsafe`, no `unwrap()` in lib code

---

## Risks

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Cross-platform binary build matrix is brittle | High | High | DevOps owns the GitHub Actions matrix in week 1 of the sprint; cache `target/`; smoke test each artifact in CI before publishing. |
| MCP spec ambiguity around tool error semantics | Medium | Medium | Pin to the MCP spec version we target; mirror Anthropic's reference server behavior; conformance test locked to that. |
| npm postinstall blocked in corporate networks | Medium | Medium | Document `TRAVSR_BINARY` override env var; allow pointing to a local file; mention in README troubleshooting. |

---

## Demo Plan (Fri 2026-06-26)

1. Fresh machine: `npm install -g travsr`, `travsr init` in a sample repo.
2. Paste the README snippet into `~/Library/Application Support/Claude/claude_desktop_config.json`, restart Claude Desktop.
3. Ask Claude: "who calls PaymentService.charge in this repo?" — show Travsr being invoked and returning structural answers.
4. Show the same on a Linux box over SSH.
5. Play the 90-second screen capture earmarked for launch.

---

## MVP Launch Checklist

- [ ] GitHub repository flipped to public
- [ ] LICENSE file present (MIT)
- [ ] README quickstart verified by a non-author
- [ ] CONTRIBUTING.md present
- [ ] CODE_OF_CONDUCT.md present
- [ ] `v0.1.0` tagged with signed release artifacts and SHA256SUMS
- [ ] npm package `travsr@0.1.0` published
- [ ] travsr.com landing page live
- [ ] 90-second demo video uploaded and linked from README
- [ ] Security disclosure policy (SECURITY.md) present
- [ ] Issue templates and PR template in `.github/`
- [ ] CI badge green on `main`

---

## Out of Scope (Deferred to Phase 2)

- LSIF integration
- Kuzu storage backend
- PPR retrieval
- VS Code extension
- MCP SSE transport
