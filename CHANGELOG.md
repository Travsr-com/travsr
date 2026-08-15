# Changelog

All notable changes to Travsr are documented here.

---

## Unreleased

### Added

- **`TRAVSR_EMBED_ACCEL`: install a GPU-accelerated embedding sidecar.** `travsr embed init` installs a CPU-only sidecar by default, which works everywhere with nothing to set up. Set `TRAVSR_EMBED_ACCEL` to `auto` (accelerated only where it needs no host setup — currently DirectML on Windows x86_64), `directml` (Windows x86_64; any DX12 GPU, so Intel, AMD or NVIDIA), or `cuda` (Linux x86_64 + NVIDIA; needs a host CUDA runtime, cuDNN and glibc 2.38+). `auto` never selects CUDA, because that build depends on host libraries the installer cannot verify, and choosing it for someone without them would trade a working CPU install for a broken GPU one. Accelerated builds also install the ONNX Runtime libraries they load, each verified against its own checksum. Setting the variable on a machine that already has a CPU-only sidecar reinstalls it, rather than reporting "ready" and silently keeping the CPU build. A wrong guess costs speed, not function: if the GPU turns out to be unusable at run time the sidecar logs why and falls back to its CPU engine. macOS needs none of this — its default sidecar already uses CoreML.
- The embed catalog's `arch` is now written into the sidecar's `model.toml` as `family`, so the sidecar can pick an engine that is actually able to execute the model's architecture. Previously every model reached the sidecar untagged and defaulted to `bert` — harmless for the five bundled entries, all of which are BERT, and wrong for the first non-BERT model added, which would have been handed to an engine that cannot load its graph.
- `travsr embed init` now refuses a model whose architecture the installed sidecar cannot run, and does so before downloading model files (which reach 1.3 GB) rather than failing partway through a later background reindex.

- **Read-only MCP observability tools (#636): `get_index_status`, `get_daemon_logs`, `get_graph_health`.** `get_index_status` reports schema version, indexed-vs-HEAD staleness, node/edge counts, Phase A/Phase B state (including per-language failed/unavailable/done), and semantic (embeddings/rerank) readiness. `get_daemon_logs` returns recent daemon log entries, parsed and severity-filtered, with home paths and credential-shaped substrings best-effort redacted. `get_graph_health` returns the report-only half of `travsr fsck`: ghost paths, orphan edges, and lexical-index parity, never mutating. All three are strictly read-only, never write `.travsr/daemon.lock`, and never aggregate across repos in global mode (supply `repo` when more than one repo is registered). `get_daemon_logs.source` is always a JSON array (it lists the log files that actually contributed the returned entries), and `get_index_status.phase_b.languages` reports `unavailable`, with an actionable detail, for a language whose sources are present but whose analyzer is not installed or registered for this repo, so `phase_b.state` always reaches a terminal value.

### Fixed

- **`travsr lang install` offered wrappers no travsr-lang release ever shipped (#588).** Every language ended in a raw `404 Not Found` on Windows: `current_target()` returned `x86_64-pc-windows-msvc`, the installer built an asset name from it, and the release matrix had no Windows leg — so `travsr init` offered an interactive setup that could not succeed. The cause was treating a target triple as proof a build existed for it, so two independent questions are now answered separately: what an asset is *named* (the `.exe` rule, which also fixes the missing suffix) and whether a published release actually *contains* it (`WRAPPER_RELEASE_TARGETS` / `MACOS_ONLY_WRAPPERS`). A target travsr does not ship for is refused before any network call, with `travsr lang list`, `travsr lang detect` and `travsr init` reporting "not available on `<target>` yet" and the manual path, instead of printing an install command that cannot work. `travsr lang list --json` gains `availableOnThisPlatform` / `unavailableTarget` so extensions can tell "run the install" from "no build exists". The same gate covers `travsr-lang-objectivec` on Linux, which 404'd for the identical reason and had gone unnoticed. travsr-lang v0.4.0 has since published the Windows wrappers, so `x86_64-pc-windows-msvc` is now a claimed target and Phase B installs on Windows for every language except `objectivec`, which remains Apple-only by design. Languages whose *underlying* analyzer has no Windows build (`scip-clang` for C/C++, `scip-ruby`, and the Swift and Dart index emitters) install the wrapper and report the analyzer as missing, rather than claiming call analysis they cannot perform. A release that genuinely lacks an asset reports the version and platform rather than a bare URL, missing binary and missing checksum stay distinct, and the PATH hint printed after an install uses PowerShell/`setx` on Windows instead of `export`/`~/.zshrc`. A weekly `lang-release-drift` workflow compares every asset name the installer would request against the live travsr-lang release inventory, so the two repos cannot drift apart silently again.
- **Ruby `require` vs `require_relative` false-positive blast radius (#614).** The Ruby analyzer now records which keyword produced an import, emitting `import:gem:<spec>` for a load-path `require` (gem/stdlib/in-repo lib) and `import:<spec>` for an importer-relative `require_relative`, instead of both collapsing to the same signature. `RubyResolver` only resolves the `require_relative` form, so a gem/stdlib `require 'json'` no longer false-matches an unrelated local `json.rb`. Existing indexes need a re-index to clear stale Ruby gem-require false positives.
- **Windows hardening batch (#495, #497, #498-#507).** Twelve Windows issues fixed across the daemon lifecycle (named-pipe response delivery, console detachment, duplicate-daemon startup guard), the plugin sandbox (AppContainer capability-pointer UB, toolchain env forwarding, core-count-scaled Job Object CPU cap, idempotent ACL grants), tool resolution (PATHEXT-aware executable lookup, npm/zip installs), binary upgrades while the daemon runs, and the VS Code extension (PATH auto-detect, honest Windows ARM messaging, validated MCP config export).
- **Windows upgrade note — autostart task migration.** The Task Scheduler auto-start task name now derives from a hash that is stable across travsr builds; previously it changed with every toolchain upgrade, so `travsr daemon stop` could not remove tasks registered by older builds. Tasks registered **before** this change keep their old names and cannot be found by the new scheme: after upgrading, remove them once manually — list with `schtasks /query /fo list | findstr Travsr`, then `schtasks /delete /tn "<name>" /f` for any stale entry. Tasks registered by this version onward are removed by `travsr daemon stop` as normal.

### Changed

- **Documentation-prose retrieval (#376) is on by default (#519).** `get_context`, `ask`, and `find_references`/`find_pattern`'s doc lane now search Markdown documentation (ADRs, RFCs, plans) alongside code by default, surfacing rationale and design docs relevant to a query. All five docs-lane accuracy/regression gates are green on both bench repos (travsr and kubernetes) against merged code. Turn it off with `travsr config set docs.enabled false`.

### Removed

- **Kùzu storage backend dropped (#457).** The optional, feature-gated Kùzu backend (`--features kuzu`), the `travsr migrate --to kuzu` command and its SQLite→Kùzu migration + integrity manifest, the SQLite/Kùzu parity harness, the nightly Kùzu CI workflow, and the `kuzu` dependency have all been removed. SQLite+WAL is the storage backend for both MVP and production; RocksDB remains a possible future hyperscale backend. See `docs/adrs/ADR-018-drop-kuzu-backend.md`.

---

## v0.11.0 - 2026-07-12

### travsr binary

- **`find_references` and `find_pattern` MCP tools.** All-language occurrence search over the graph: locate every use of a symbol, or match a structural pattern, across every indexed language.
- **Graph garbage collection and `fsck`.** Tier 1-2 GC reclaims orphaned nodes and edges, and a new `travsr fsck` command checks and repairs graph integrity.
- **Reindex resource governance.** A new `travsr-config` foundation adds capacity limits, cancellation, and live reconfiguration so large reindex jobs stay within bounded CPU and memory.
- **`get_snippets` duplicate-signature disambiguation.** Four-tier resolution picks the right definition when several symbols share a signature.
- **Evidence-based language edge detection.** `language_has_edge_sites` is now derived from observed edges instead of assumed, improving cross-language accuracy.
- Fixes: LSIF emitter pipe-buffer deadlock and indexer sandbox hardening (#412); six deferred plugin-host / daemon items from PR #364 (#382).

**Full changelog:** https://github.com/Travsr-com/travsr/compare/v0.10.0...v0.11.0

---

## v0.10.0 - 2026-07-05

### travsr binary

- **Semantic embeddings (RFC-018).** A downloadable embedding plugin sidecar adds vector recall alongside graph traversal. Config-driven model catalog with arctic-embed-m as the recommended model, auto-calibrated model-relative semantic floors, and a cosine oracle (RFC-019) for context-quality checks.
- **RRF fusion retrieval.** `get_context` and `ask` now fuse exact, full-text, and embedding candidates with reciprocal-rank fusion, gated by a score-aware scope check.
- **k-core decomposition.** Shell propagation recovers buried-middle nodes and boosts `get_context` coverage.
- **New analysis crate (RFC-017).** Tree-sitter parsing, AST skeleton building, and snippet extraction moved into `travsr-analysis`, powering the `get_snippets` MCP tool for kind-aware code retrieval.
- **Data-format support.** JSON, YAML, TOML, and XML are now parsed as Phase A nodes (`configures` / `external-dependency` edges).
- **`get_callers` reports exact call-site `path:line`** without a schema change.
- **Git-aware worktree resolution** and a rank penalty for generated / mock code so real definitions surface first.
- **Phase B plugin I/O is bounded** so a wedged language plugin can no longer deadlock indexing.
- **Daemon file logging** to `.travsr/daemon.log` with daily rotation.
- **Relicensed from MIT to Apache-2.0.**
- Fixes: stale Phase 1 embeddings (model.toml migration + file-node eligibility), documentation accuracy pass.

### VS Code extension (vscode-v0.9.0)

- Production parity with the daemon: live graph, blast radius, callers, and Context Explorer over MCP.
- Bundled binary reference updated to v0.10.0.

**Full changelog:** https://github.com/Travsr-com/travsr/compare/v0.9.1...v0.10.0

---

## v0.9.0 - 2026-06-11

### travsr binary

- Add native Phase B semantic indexing for Rust, TypeScript, and Python (no external SCIP tools required)
- Add Phase B support for Kotlin via Kotlin Language Server with ZipBinary install
- Add Phase B support for Swift and Dart: Phase A tree-sitter configs and GithubBinary install
- Add Phase B support for Java: sandbox, resolver, and edge wiring end-to-end
- Add Phase B support for C#: dotnet tools PATH check and toolchain grants
- Add Phase B support for Scala: sbt catalog
- Add `.travsrignore` support for excluding paths during `travsr init`
- Add live progress UI during `travsr init` with parallel batched indexing
- Add branded `--help` logo
- Add blast radius Phase 2 and 3: ImportResolver for 14 languages and Go intra-package co-file edges
- Add Tree-sitter vs Semantic toggle for the blast radius view
- Add `native_phase_b` flag for per-language configuration
- Allow network access in Phase B sandboxes; display live schema version in `travsr status`
- Fix Dart Phase B: bypass sidecar, call emitter directly
- Fix bwrap network isolation enforcement on Linux NativeIpc sandbox
- Fix `bulk_init`: create `_bulk_fts_pending` table before `write_file_graphs_batch`
- Fix generalized co-package pass and Phase B batch writes
- Fix JS/TS file extension handling, Dart sandbox, and Phase B edge writes

### VS Code extension (vscode-v0.8.0)

- Add JavaScript and C# to codelens and hover selectors
- Add Tree-sitter vs Semantic toggle in blast radius webview
- Extend hover and codelens selectors to all 13 indexed languages
- Fix re-index triggering, panel refresh, and test regressions
- Fix stale language list refresh and status bar `.travsr` watcher
- Fix MCP envelope leaking into file lists
- Fix blast radius depth slider and corpus auto-trust
- Update bundled binary reference to v0.9.0

**Full changelog:** https://github.com/Travsr-com/travsr/compare/v0.8.0...v0.9.0

---

## v0.8.0 - 2026-06-06

### travsr binary

- Add `travsr-ipc` crate: platform-agnostic control plane (Unix socket on macOS/Linux, Named Pipe on Windows)
- Add Windows Named Pipe daemon control: `travsr daemon start/stop/status` now works on Windows
- Add dual-write post-commit hook on Windows: installs both `post-commit` and `post-commit.cmd` so the hook fires from CMD, PowerShell, and Git Bash
- Add Windows Task Scheduler auto-start via `travsr daemon start --autostart`
- Add AppContainer + Job Object sandbox for plugin host processes on Windows
- Add full `CreateProcessW` spawn inside AppContainer for plugin indexers on Windows
- Add CI matrix for elevated AppContainer sandbox tests on Windows
- Fix `lang list` incorrectly showing built-in indexers as disabled on Windows
- Fix `travsr unregister` treating task-not-found as an error instead of a no-op
- Fix `graph.db` file permissions on Windows (restricted via `icacls`)

### VS Code extension (vscode-v0.7.0)

- Add `.exe`-only binary spawn on Windows with `assertExecutableBinary` path validation
- Fix `showLanguages` using a stale binary path captured at activation time
- Fix stale download URLs and `assertExecutableBinary` not being called in `reindexNow`
- Fix `assertExecutableBinary` metacharacter regex on Windows paths
- Update bundled binary reference to v0.8.0

**Full changelog:** https://github.com/Travsr-com/travsr/compare/v0.6.0...v0.8.0

---

## v0.6.0 - 2026-05-31

This release ships the complete Phase 2 retrieval stack, the VS Code graph panel,
line-number storage for go-to-definition, and a new `get_graph_stats` introspection
tool.

### Retrieval

- **0-1 knapsack token-budget enforcer** (`travsr-retrieval`): Implements RFC-010.
  Given a set of PPR-scored nodes and a token budget, the knapsack solver selects
  the highest-value subgraph that fits the budget. Powers the `get_context` MCP tool
  and `travsr ask`.

- **`get_context` MCP tool**: Full PPR traversal ranked by relevance, budget-capped
  by the knapsack solver. Accepts an optional `token_budget` argument; defaults to
  the workspace-wide constant `MAX_CONTEXT_BUDGET`.

- **`travsr ask` upgraded to PPR + knapsack**: The CLI `ask` command now uses the
  full retrieval pipeline instead of raw BFS. Results are scored by PPR and trimmed
  to the token budget.

### VS Code Extension

- **Cytoscape.js graph panel** (`travsr-vscode`): A `WebviewPanel` rendering the
  live dependency graph for the active file. Supports kind filtering, two-hop import
  traversal, unique file node IDs, and the Travsr brand logo in the titlebar. Open
  via the sidebar or `Travsr: Show Graph` in the command palette.

- **`get_graph_stats` integration**: The status bar node count now reads from the
  `get_graph_stats` MCP tool instead of a stale cached value.

- **Bug fixes** (PR #226): Cache key collision, timer leak, loading state
  inconsistency, duplicate welcome panel, and blast-radius called per-symbol instead
  of per-file.

### MCP

- **`get_graph_stats` tool** (`travsr-mcp`): Returns accurate live node and edge
  counts for the indexed graph. Used by the VS Code status bar.

### Core / Indexer

- **Line numbers stored on nodes** (PR #249, #250): Every non-file, non-import node
  now carries a 1-based `line` field populated by all four Tree-sitter parsers
  (TypeScript, Rust, Python, Go). The `travsr-mcp` and `travsr-vscode` layers
  forward the field to callers. Enables go-to-definition in IDE integrations.

### Daemon / CLI

- **Daemon reliability fixes** (PR #223): Registry guard prevents double-registration
  on concurrent `travsr init` calls; `SKIP_DIRS` now respected during walk; `init`
  totals match actual indexed counts.

- **`travsr index` JSON fixes**: Retry daemon status ping 3x before reporting running;
  include all indexed nodes in JSON output; add `corpus` field to each node entry.

- **`delete_nodes_for_path_prefix`** (`travsr-store`): New store primitive used by
  the daemon when a directory is deleted between commits.

### CI

- **Fuzz corpus seeds** (PR #251): Added missing seed files for `fuzz_go_parser`,
  `fuzz_pcst_session`, and `fuzz_pyright_lsif_parser`; the nightly fuzz job no longer
  fails with empty-corpus errors.

- **Release tag glob fix**: `release.yml` tag pattern no longer matches `vscode-v*`
  tags, preventing spurious binary publish runs on VS Code extension releases.

### Architecture

- **RFC-011 - Two-transport plugin architecture**: Defines how future language plugins
  expose both an in-process tree-sitter path (fast, zero-IPC) and an out-of-process
  LSP/LSIF path (accurate, sandboxed). Adopted in ADR-017.

- **ADR-017 - Unified plugin sandbox trust model**: Establishes the trust boundary
  for out-of-process plugins and the capability set they may request.

- **RFC-008, RFC-009, ADR-009, ADR-010**: Multi-language extension architecture docs
  covering TypeScript, Python, Go, and Rust LSIF integration strategies.

**Full changelog:** https://github.com/Travsr-com/travsr/compare/v0.5.1...v0.6.0

---

## v0.3.1 - 2026-05-21

Security patch release on top of v0.3.0. No new features. Upgrade strongly recommended.

### Security

- **Git hook shell-injection fix**: The `post-commit` hook no longer passes
  git-reported filenames through shell expansion. Previously, a filename
  containing `;`, `$(...)`, or other shell metacharacters could execute
  arbitrary code on every `git commit`. The hook now calls
  `travsr hook-run --from-hook`; the binary reads changed files from git
  directly via `std::process::Command`. No shell involved.

- **File permission hardening**: `graph.db` is now created `0600` (owner
  read/write only). `~/.travsr/` is created `0700`. `registry.json` is
  written `0600`. Prevents other local users from enumerating indexed repos
  or reading derived graph data.

### Fixes

- `travsr-retrieval`: remove redundant closure wrapping `ppr_inner`
  (clippy `redundant_closure_call`).

**Full changelog:** https://github.com/Travsr-com/travsr/compare/v0.3.0...v0.3.1

---

## v0.3.0 - 2026-05-21

This release closes Phase 2 of the Travsr roadmap. The production Kùzu storage
backend is now available, the security hardening recommended in the Phase 1 audit
is complete, and the graph identity foundation (Kythe VNames, RFC-002) is locked
in for cross-repo support in Phase 3.

⚠️ **Migration required for v0.2.x users:** RFC-002 changes the VName corpus
format. Re-run `travsr init` in each repo; old signatures are rejected by
v0.3.0. The old `.travsr/graph.db` can be deleted safely; `travsr init` rebuilds
it from source.

### Production Storage

- **Kuzu backend** (`--features kuzu`): The production graph engine is now
  available. Build with `cargo build --release --features kuzu` to enable.

- **`travsr migrate --to kuzu`**: Migrate an existing SQLite graph to Kùzu
  with a single command. The migration is idempotent; running it twice is safe.
  SQLite is never deleted; both backends coexist.

- **Migration integrity manifest**: Every migration computes a SHA-256 digest
  of all nodes and edges before and after the copy. A mismatch aborts the
  migration and leaves SQLite intact. The Kùzu store is written to a staging
  path (`graph.kuzu.new`) and atomically renamed only after the manifest matches.

- **Backend-agnostic schema migration framework**: All storage backends now
  share a versioned migration runner. Schema migrations are idempotent and
  version-gated. Re-running migrations on an already-migrated store is a no-op.

### Security

- **Prompt-injection hardening (SEC-001)**: All MCP tool outputs are now
  sanitized before being returned to the client. C0/C1 control characters are
  stripped, `<` and `>` are escaped, output is truncated to a safe maximum
  length, and every response is wrapped in a `<travsr-data>` structural envelope.

- **Path-traversal hardening (SEC-002)**: Path and symbol arguments are now
  validated at the MCP dispatch layer. `../`, `..\`, absolute paths, null bytes,
  and `%`-encoded traversal sequences are rejected before reaching the graph store.

- **Migration integrity verification (SEC-008)**: SHA-256 manifests are
  computed over every node and edge during SQLite→Kùzu migration. Mismatch
  aborts; staging directory is removed; SQLite store is left intact.

- **Git hook shell-injection hardening**: The `post-commit` hook no longer
  passes git-reported filenames through shell expansion. The hook now calls
  `travsr hook-run --from-hook`, which reads changed files from git directly
  via `std::process::Command`. Filenames containing spaces, semicolons, or
  shell metacharacters are handled safely.

- **File permission hardening**: `graph.db` is now created with `0600`
  permissions (owner read/write only). `~/.travsr/` directory is created with
  `0700`, and `registry.json` is written with `0600`. Prevents other local
  users from enumerating indexed repos or reading derived graph data.

### Architecture

- **RFC-002: VName signature format versioning**: VName signatures now include
  a version prefix and domain separator. Signatures from different versions do
  not collide. The corpus meta write is now a hard error; a repo with a
  mismatched or missing corpus identity will not index silently.

- **ARCH-102: Kythe corpus naming convention**: Corpus names are now derived
  deterministically from the git remote URL (or directory name as fallback).
  All repos indexed with v0.3.0+ use the standardized convention.

- **ADR-003: PPR algorithm constants**: The Personalized PageRank constants
  (`α = 0.85`, `ε = 1e-6`) are now defined in a dedicated module with
  compile-time bounds assertions. These will power the Phase 3 traversal engine.

### Quality

- **QA-010: SQLite/Kuzu parity harness**: A property-based test harness verifies
  that SQLite and Kùzu backends return identical results for all graph queries.
  Parity is enforced on every CI run when `--features kuzu` is enabled.

- **QA-012: MCP Phase 2 conformance suite**: The MCP server is now validated
  against the full JSON-RPC envelope spec, sanitization pipeline, and multi-repo
  `repo` argument routing.

### Breaking Changes

- **VName corpus identity (RFC-002)**: Repos indexed with v0.2.x will have a
  mismatched corpus signature under v0.3.0. Run `travsr init` again to re-index
  with the new VName format.

- **`graph.db` and `~/.travsr/` permissions**: These are now created `0600`/`0700`.
  Existing files are updated on the next `travsr init` or `travsr migrate` run.

**Full changelog:** https://github.com/Travsr-com/travsr/compare/v0.2.0...v0.3.0

---

## v0.2.0 - Phase 1 complete

Initial public release. Tree-sitter TypeScript indexer, SQLite graph store,
BFS retrieval, MCP stdio server, `travsr init` / `travsr ask` / `travsr mcp` CLI,
git post-commit hook, SHA-256 delta reindex, global multi-repo registry.

**Full changelog:** https://github.com/Travsr-com/travsr/compare/v0.1.3...v0.2.0

---

## v0.1.3 - Patch

**Full changelog:** https://github.com/Travsr-com/travsr/compare/v0.1.2...v0.1.3

## v0.1.2 - Patch

**Full changelog:** https://github.com/Travsr-com/travsr/compare/v0.1.1...v0.1.2

## v0.1.1 - Initial alpha

**Full changelog:** https://github.com/Travsr-com/travsr/releases/tag/v0.1.1
