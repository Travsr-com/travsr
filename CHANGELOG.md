# Changelog

All notable changes to Travsr are documented here.

---

## Unreleased

---

## v1.0.0-rc.1 - 2026-08-16

> First 1.0 release candidate. Install with `npm i -g @travsr.com/travsr@rc` or
> `sh -s -- --version v1.0.0-rc.1` via the shell installer. Promotion to
> `latest` republishes this exact signed artifact unchanged; see
> [Release channels](README.md#release-channels).

### Fixed

- **Phase B "produced no symbols" false warning on native languages, and two more beta smoke-test defects (#724, #726, #727).** The zero-node warning was gated on `nodes.is_empty()`, so native Phase B analyzers (Rust, TypeScript, Python) that attach `ref`/`call` edges onto existing Phase A nodes without emitting new definition nodes were reported as broken even though their call graphs were fully queryable. A language now counts as having produced nothing only when nodes, edges, refs, unresolved calls, and positional refs are all empty; the genuine all-zero case (e.g. `scip-ruby` invoked without an input path) still fires. `find_references`'s "available in ~2 minutes" promise, which never resolves without a running daemon, is now backed by a single shared `phase_b_pending_json` helper used by every tool that waits on Phase B (`find_references`, `get_callers`, `get_execution_path`), so the same false ETA cannot resurface on one tool while fixed on another. `travsr lang status` was added as a visible alias of `lang list`, matching every other area's `status` convention (`travsr status`, `daemon status`, `embed status`, `rerank status`) and the project's own docs, which already told agents to run it.
- **Java Phase B silently fails on stock macOS bash (#724).** scip-java's generated `javac` wrapper expands empty arrays under `set -u`, which bash 3.2 (macOS's default `/bin/bash`) treats as an unbound-variable error but bash 4.4+ does not. The Maven compile failed, scip-java exited 1, and Java Phase B produced no call edges, surfacing only as the generic "produced no symbols" warning with no hint at the real cause. When that warning fires for Java on macOS with an old resolved `bash`, `travsr init` and `travsr status` now append the actual cause and fix (`brew install bash`, ahead of `/bin/bash` on PATH).
- **`--version` and the daemon's telemetry both dropped the prerelease suffix (#728).** v1.0.0-beta.1's binary reported `travsr 1.0.0`, indistinguishable from stable 1.0.0 or from a later beta, and daemon session-start logs carried the same ambiguity. `crates/travsr-cli/Cargo.toml` stays pinned to the base version, since `verify-version` in the release workflow ties the tag base to that file, so the release job now exports the full tag as `TRAVSR_RELEASE_VERSION` and the CLI reads it via `option_env!`, falling back to the crate version when unset (local and development builds are unchanged). The injected build id is `<tag base>+<short commit>`, not the raw channel name: promotion republishes the exact same signed bits under a new tag (beta.1 to rc.1 to stable), so baking the channel name in would leave a promoted stable release reporting itself as `1.0.0-beta.1` forever. The base survives promotion unchanged and the commit identifies the actual build.
- **`npm i -g @travsr.com/travsr` could report success on Windows and still leave `travsr` unusable.** The postinstall script (which downloads the native binary) silently did not run on the Windows smoke-install runner, and `bin/travsr.js` dead-ended with "binary not found. Run npm install...". The download/verify/extract logic is now shared between postinstall and a runtime fallback in `bin/travsr.js`, which downloads the binary on first invocation if it is missing, recovering regardless of why postinstall did not run (`--ignore-scripts`, an `allow-scripts` gate, or a flaky network mid-install).

**Full changelog:** https://github.com/Travsr-com/travsr/compare/v1.0.0-beta.1...v1.0.0-rc.1

---

## v1.0.0-beta.1 - 2026-08-16

> First 1.0 beta. Install with `npm i -g @travsr.com/travsr@beta` or
> `sh -s -- --version v1.0.0-beta.1` via the shell installer. Promotion to
> `rc` and `latest` republishes these exact signed artifacts unchanged; see
> [Release channels](README.md#release-channels).

### Added

- **`travsr connect`: auto-wire AI coding tools to the Travsr MCP server (RFC-018, #292).** `travsr init` now detects installed AI coding tools, Claude Code, Cursor, VS Code Copilot, Gemini CLI, Antigravity, Codex, Windsurf, and Zed, and for each one registers the `travsr mcp --stdio` server plus an always-on rules file telling the agent to query Travsr before grep or a raw file read (run standalone as `travsr connect`, or skip it with `travsr init --no-connect`). Generated files are local and git-ignored by default, since a committed MCP server definition is an RCE-on-clone vector, and the server command is the bare `travsr` on PATH rather than an absolute path that would leak a username. Writes are strict-JSON-or-skip: an existing hand-authored config that doesn't parse as the expected shape is left untouched instead of clobbered, and markdown rules files use a single balanced managed block so re-running is idempotent. `--tool <id>` scopes to one tool, `--print` previews without writing, `--remove` undoes a previous run, and `--commit` opts into committing the generated files instead of git-ignoring them. The bundled AI-tool prompt was also rewritten MCP-first (a question-to-tool table, an explicit "always pass repo" section, and edge kinds pulled from the real `EdgeKind` enum) after it was found still steering agents toward a four-command CLI-only world and away from `find_references`.
- **Beta / RC / stable release channels (#480).** `.github/workflows/release.yml` now maps tags `v<version>-beta.N` -> `v<version>-rc.N` -> `v<version>` to npm dist-tags `beta` / `rc` / `latest`. `crates/travsr-cli/Cargo.toml` and `packages/travsr-npm/package.json` stay pinned to the plain stable version for the whole train, so all three channels ship byte-identical binaries; promotion is a `workflow_dispatch` that republishes the already-signed beta/rc artifact rather than rebuilding. A `verify-version` gate ties the pushed tag to the committed `travsr-cli`/npm version, a preflight gate requires green CI on the exact tagged commit, fuzz/OSV/accuracy/AB-eval gates run as `workflow_call` promotion checks, and a post-publish smoke-install job installs from the channel's dist-tag across all four target OSes. `rc -> stable` requires a human approval on the `release` GitHub Environment. This release is the first to ship through it.
- **Cross-encoder relevance reranker (RFC-021, #460).** A new `travsr-rerank` crate runs an in-process fp16 MiniLM-L-6-v2 cross-encoder (via `tract`) that reranks graph-retrieved candidates against the query text before `get_context`/`ask` classify confidence, fixing a "confident salad" failure mode where an off-topic natural-language query whose words happened to collide with real symbol names (e.g. "delete all user accounts" hitting `SqliteStore::delete_file` or a Rust `Drop` impl) returned STRONG-confidence, entirely irrelevant results. A rarity-based exact-anchor bypass keeps genuine symbol lookups deterministic and untouched by the reranker. The daemon auto-fetches the model on start unless `TRAVSR_NO_RERANK` is set (or only the auto-fetch is skipped via `TRAVSR_NO_RERANK_AUTOFETCH`, leaving a manually-installed model active); `travsr rerank install`/`travsr rerank status` manage it directly. The path is fail-open end to end: a missing model, a panic, or a time/token budget overrun falls back to unreranked results rather than failing the query. A per-candidate character cap keeps cost repo-agnostic after a cross-repo Kubernetes (262k nodes) run surfaced a batch-padding cost blowup on long Go snippets.
- **Documentation-prose retrieval lane (RFC-023, #376).** Markdown files are now parsed as first-class graph nodes (heading-scoped chunks with front-matter title extraction, fence elision, and slugified anchors) rather than skipped, so ADRs, RFCs, and design docs become embeddable, FTS-indexed, and RBAC-filtered like code. `get_context`, `ask`, and the `find_references`/`find_pattern` doc lane can now surface the *rationale* behind code, not just the code itself (later turned on by default, see Changed). A built-in exclusion list (CHANGELOGs, templates, vendored app bundles) keeps noise out, overridable via `TRAVSR_DOCS_EXCLUDE`.
- **`search_symbol` exact-match mode (#453, #676).** `search_symbol` accepts a `mode: "exact"` argument to return only exact name matches instead of substring matches, cutting result noise on common short names.
- **Embedding lifecycle commands made real: `travsr embed switch`, `gc`, `reconfigure`, `calibrate` (#524, #481-#483).** `embed switch` now actually writes the repo's `.travsr/embed.toml` by default (`--global` for the machine-wide default) instead of only updating in-memory state, and reports the inactive-vector cost of the switch; `embed status`/`embed list` no longer disagree about which model is active. `embed gc` reclaims `embed.db` rows and HNSW index files held by inactive models, dry-run by default, with `--apply` to actually delete and `--keep <model>` to retain specific ones. `embed reconfigure` changes the reindex worker budget/priority on a running or paused reindex and applies it immediately by cancelling and respawning from partial progress, no re-embedding. `embed calibrate` re-measures the model-relative semantic floors on an existing index without a re-embed. A new cross-process `EmbedOpLock` (a non-blocking flock on `.travsr/embed.lock`) replaces two separate ad-hoc lock acquisitions that could otherwise self-deadlock when the CLI and the daemon's automatic reindex trigger overlapped.
- **VS Code diagnostics overlay plane + `travsr daemon lsp` (#688).** The daemon now holds a "leased" view of live LSP diagnostics reported by the VS Code extension: reports are keyed by editor session (so two windows on one repo don't overwrite each other's view), carry a lease that expires if the window stops renewing or closes, and are sent per-file so the payload scales with breakage rather than repo size. This is a plane beside the graph, never merged into it, since diagnostics depend on which extensions are installed and on unsaved buffer contents and can never be reproduced from the repository alone. `travsr daemon lsp` shows the last overlay the daemon received, answering "is the editor extension actually reaching the daemon, and what did it last see." The VS Code panel gained a JSON log view, a searchable daemon-log tail, and a colour-coded activity feed for what changed.
- **Native Python Phase B semantic indexing (#709).** Python joins Rust and TypeScript with native call-edge extraction that needs no external SCIP tool, plus fuzzy occurrence matching and stricter typo correction in the resolver.
- **Structured daemon logging + `travsr daemon logs` (#347, #348).** The daemon now writes JSON Lines to `.travsr/daemon.log` with stable event keys, correlated by request ID and tagged by repo, instead of free-form prose lines that didn't reconcile with the log file naming the code actually used (`daemon.log.<DATE>`, not `daemon.log`). `travsr daemon logs [-f] [--lines N] [--repo <name>] [--json]` reads the file directly, so it works after a crash without a running daemon, follows rotation with `-f`, and can filter to one repo when several share a log.
- **`curl | sh` one-line installer (#691).** `curl -fsSL https://travsr.com/install.sh | sh` installs the travsr CLI without Node, with `sh -s -- --system` to install to `/usr/local/bin` instead of the default `~/.local/bin`, and `sh -s -- --version v0.11.0` to install a specific release instead of latest stable. SHA256 verification against the release `SHA256SUMS` is mandatory on every install with no bypass; the cosign signature is verified when cosign is on `PATH`, and verification failure aborts the install (unlike the npm postinstall path, which falls back to SHA256-only). Supports Linux, macOS, and Alpine/musl; no Windows build is offered. The script's target map is guarded in CI by `.github/scripts/check-target-maps.mjs` alongside the release build matrix and the two existing installer target maps. The hosted URLs (`travsr.com/install.sh` and the GitHub Releases fallback) go live with this release; the `travsr.com` redirect follows as a separate change.
- **`TRAVSR_EMBED_ACCEL`: install a GPU-accelerated embedding sidecar.** `travsr embed init` installs a CPU-only sidecar by default, which works everywhere with nothing to set up. Set `TRAVSR_EMBED_ACCEL` to `auto` (accelerated only where it needs no host setup, currently DirectML on Windows x86_64), `directml` (Windows x86_64; any DX12 GPU, so Intel, AMD or NVIDIA), or `cuda` (Linux x86_64 + NVIDIA; needs a host CUDA runtime, cuDNN and glibc 2.38+). `auto` never selects CUDA, because that build depends on host libraries the installer cannot verify, and choosing it for someone without them would trade a working CPU install for a broken GPU one. Accelerated builds also install the ONNX Runtime libraries they load, each verified against its own checksum. Setting the variable on a machine that already has a CPU-only sidecar reinstalls it, rather than reporting "ready" and silently keeping the CPU build. A wrong guess costs speed, not function: if the GPU turns out to be unusable at run time the sidecar logs why and falls back to its CPU engine. macOS needs none of this, its default sidecar already uses CoreML.
- The embed catalog's `arch` is now written into the sidecar's `model.toml` as `family`, so the sidecar can pick an engine that is actually able to execute the model's architecture. Previously every model reached the sidecar untagged and defaulted to `bert`, harmless for the five bundled entries, all of which are BERT, and wrong for the first non-BERT model added, which would have been handed to an engine that cannot load its graph.
- `travsr embed init` now refuses a model whose architecture the installed sidecar cannot run, and does so before downloading model files (which reach 1.3 GB) rather than failing partway through a later background reindex.
- **Read-only MCP observability tools (#636): `get_index_status`, `get_daemon_logs`, `get_graph_health`.** `get_index_status` reports schema version, indexed-vs-HEAD staleness, node/edge counts, Phase A/Phase B state (including per-language failed/unavailable/done), and semantic (embeddings/rerank) readiness. `get_daemon_logs` returns recent daemon log entries, parsed and severity-filtered, with home paths and credential-shaped substrings best-effort redacted. `get_graph_health` returns the report-only half of `travsr fsck`: ghost paths, orphan edges, and lexical-index parity, never mutating. All three are strictly read-only, never write `.travsr/daemon.lock`, and never aggregate across repos in global mode (supply `repo` when more than one repo is registered). `get_daemon_logs.source` is always a JSON array (it lists the log files that actually contributed the returned entries), and `get_index_status.phase_b.languages` reports `unavailable`, with an actionable detail, for a language whose sources are present but whose analyzer is not installed or registered for this repo, so `phase_b.state` always reaches a terminal value.

### Fixed

- **`find_references`/`graph`'s `--path` hint only matched a filename suffix (#647, #719).** A directory prefix (`crates/travsr-retrieval`) or an interior fragment (`retrieval`) matched nothing and returned a bare empty result, indistinguishable from a genuine zero-references answer, the same misleading abstention the rest of `find_references` was already hardened against (#299, #450). A shared `path_hint_matches` now accepts whole-path, `/hint` suffix, `hint/` prefix, `/hint/` segment, and bare-word substring forms (the dot check keeps the existing filename boundary guard, so `tools.rs` never bleeds into `mytools.rs`), and `find_references` re-resolves without the hint and reports where a symbol actually is defined when a path hint filters out every match instead of returning nothing.
- **Method-call receiver-type resolution to fix bare-leaf collisions (#529).** Method calls (`recv.method()`) previously resolved to a target purely by unique leaf name, so unrelated methods sharing a name (e.g. `Session::filter` vs. `Iterator::filter`) collided into false `ref/call` edges. The extractor now recovers the receiver's type where syntactically possible (`self`, or a local `let`/parameter binding, peeling references and one `Option`/`Arc`/`Box`/`Rc` layer) and resolves to an exact `fn:T.method` match, to nothing when the receiver type is not in the graph, or falls through to the prior unique-leaf behavior when the type exists but lacks the method. Measured on this repo: false edges into unique-leaf qualified methods dropped from 222 to 181 with a +236 call-site recall win, with zero regressions on the Kubernetes (Go) control graph. A related multi-language pass (N1-N7) qualified config-driven and Java methods as `method:Type.name` with containment edges, migrated Rust methods from `fn:Type.method` to `method:Type.method`, resolved Rust default trait methods, distinguished C#/PHP/Swift struct-vs-class-vs-enum-vs-actor kinds, emitted Go struct fields and C/C++ macros as their own node kinds, and captured Ruby `require`/`require_relative` and TypeScript/JS ES + CommonJS imports as real dependency edges (the latter had none at all: `require()` matched no import query, so CommonJS files contributed nothing for blast radius to traverse).
- **MCP tool correctness: several tools returned a confident-looking wrong or empty answer instead of an error.** `find_pattern` shelled out to `git grep`, invisible to untracked files and anything the graph's own walker indexed that git had not (#448, #600). `get_execution_path` relayed its source-rooted BFS fallback as if it were a real path when source and sink were disconnected, and returned a bare empty envelope on an unresolved endpoint, both read as authoritative "no path exists" answers instead of "could not determine" (#620, #628). `get_graph_json` promoted every bare-name substring match to a hop-0 root instead of resolving exact-first like `get_callers`/`find_references` already did, flooding subgraphs with unrelated symbols (#618, #626). `get_dependencies(file=...)` returned an empty envelope with no hint when passed a symbol name instead of a file path, reading as "this file has no dependencies" (#619, #627). `get_blast_radius` was one-hop instead of transitive for every Phase-2 language (Ruby, Go, Java, Kotlin, Scala, PHP, C#, C/C++, Swift, Dart, Objective-C): the BFS continuation was dead code because Phase 1's queue had already drained before Phase 2 ran (#613, #616). `travsr fsck` reported a clean graph while `ref/call` edges were actually orphaned, since the report path only computed the ghost-node set and the only orphan-detection logic was DELETE-only and gated behind `--fix` (#580, #635); a HEAD-drift reconcile could also leave ghost nodes behind entirely when a deleted file fell outside the tracked-at-HEAD set it re-scanned (#645), and `find_references`/`get_dependencies`/`get_graph_json` now all carry the same HEAD-mismatch note the call-graph tools already had, so a drifted checkout (a linked worktree, or an unreconciled HEAD move) gets a signal instead of confident stale `path:line` output (#661). Symbol ambiguity resolution undercounted results whenever a query returned exactly the display-truncation limit, silently suppressing the "more results exist" notice (#565, #625).
- **`get_repo_map` reworked into a ranked orientation map (#632).** Previously an alphabetical, size-guillotined file dump; now an adaptive directory rollup (splitting large subtrees while keeping small ones whole) ranked by actual dependents, resolved from the two-hop import chain plus `ref/call` edges rather than a manifest or per-language parse, so it stays language-agnostic and stops silently truncating.
- **RFC-022 conceptual retrieval over-abstention (#462, #463, #472, #475).** A conceptual natural-language query could return ~62% off-topic nodes as near-zero-score budget filler; `get_context` now drops structural noise (CI/workflow package nodes, bare file nodes) from the final candidate list and raised its relevance floor from 0.01 to 0.03 (chosen on a richness sweep: this cut removes ~82% off-topic nodes without thinning genuine multi-concept answers), while four separate uncalibrated stages in the seed-selection cascade that were discarding a correctly-ranked top embedding match are now gated on one shared, model-relative recall floor. Swift/Objective-C `find_references`/`get_callers` also stopped missing real matches (#449, #475).
- **CLI/UX polish batch, UX-001 through UX-023 (#707).** Selected fixes from a manual hand-testing pass: `init` nudges to install a language analyzer only for languages that actually need one (excluding built-ins) and suppresses the tip on a no-op re-run; unsandboxed rust-analyzer opt-in and sandbox-unavailable notices downgraded from ERROR to WARN so they stop reading as failures; `init --force` (alias `--rebuild`) added to purge and fully rebuild instead of skipping unchanged files; `init` now reconciles deleted files on every run so ghost nodes are swept without a separate `fsck`; `travsr ask`'s cold-path docs note only fires on a grounded result; `travsr config unset <key> [--repo]` added; and CLI output for `find_pattern` results is no longer HTML-entity escaped.
- **Phase B resilience and status honesty: one crashing sidecar no longer poisons the whole repo (#712).** A single language sidecar that crashed (e.g. the Objective-C libclang dyld crash on a machine without the build host's Xcode path) left `phase_b_commit` pinned behind HEAD forever, so the daemon re-ran Phase B on a loop, `travsr status` reported `semantic: not run` for the whole repo, and `get_callers` / `find_references` warned "building in the background, references available in ~2 minutes" indefinitely, even for languages that had already finished. Phase B now advances the completion marker whenever any language produced results: the healthy languages are complete and queryable at HEAD, the crashed one is recorded, and the loop settles instead of retrying a persistently broken sidecar. `travsr status` reports `partial (crashed: <lang>)` rather than a flat `complete` that contradicts the per-language reality. A language whose analyzer ran but produced zero nodes over its source files (a silent build-free failure, e.g. scip-ruby invoked without an input path) is now surfaced as a warning instead of a zero-node "success", and `write_phase_b_batch` drops edges whose endpoints do not exist so an incomplete sidecar result never leaves the orphan half-edges `travsr fsck` used to flag. `travsr status` reports the actual installed scip-java version (recorded at install time in a `<bin>.version` file) rather than the coursier launcher's meaningless `0.0.0`, and the crash-retry hint points at the working force path (`travsr init --semantic --force`). The MCP server reports its real version in `serverInfo.version` (the crate was pinned to the stale workspace `0.7.0`), `get_lang_status` no longer tells the user to install an analyzer that is already active, and the catalog's scip-ruby fallback uses the correct `--index-file` flag with a positional input path. A macOS end-to-end pass on fastlane extended this: scip-ruby class methods (encoded by Sorbet as the singleton class `<Class:X>`) now unify onto their tree-sitter node instead of orphaning a duplicate that stole the call edges, so `find_references` on a Ruby class method resolves its callers again; the Objective-C sandbox grants libclang the `/Library` read it needs beyond `/Library/Developer`, without which every translation unit failed and the emitter returned an empty index; Phase B sidecar stderr is drained into a bounded ring and logged on crash or a zero-node invoke so a silent analyzer failure is diagnosable instead of swallowed; the "no orphan edges" guarantee is now a store invariant enforced at the Phase A staging-flush promotion as well, closing a dangling tree-sitter `defines` edge from an un-emitted container; `travsr lang install <lang> --reinstall` re-runs the underlying SCIP tool download (not just the wrapper) so the recorded scip-java version is actually refreshed; the embeddings activation tip gates on the repository's active backend rather than the global one; and `travsr init` distinguishes languages that produced symbols from those that ran but produced none, instead of calling both "enabled".
- **`travsr lang install` offered wrappers no travsr-lang release ever shipped (#588).** Every language ended in a raw `404 Not Found` on Windows: `current_target()` returned `x86_64-pc-windows-msvc`, the installer built an asset name from it, and the release matrix had no Windows leg, so `travsr init` offered an interactive setup that could not succeed. The cause was treating a target triple as proof a build existed for it, so two independent questions are now answered separately: what an asset is *named* (the `.exe` rule, which also fixes the missing suffix) and whether a published release actually *contains* it (`WRAPPER_RELEASE_TARGETS` / `MACOS_ONLY_WRAPPERS`). A target travsr does not ship for is refused before any network call, with `travsr lang list`, `travsr lang detect` and `travsr init` reporting "not available on `<target>` yet" and the manual path, instead of printing an install command that cannot work. `travsr lang list --json` gains `availableOnThisPlatform` / `unavailableTarget` so extensions can tell "run the install" from "no build exists". The same gate covers `travsr-lang-objectivec` on Linux, which 404'd for the identical reason and had gone unnoticed. travsr-lang v0.4.0 has since published the Windows wrappers, so `x86_64-pc-windows-msvc` is now a claimed target and Phase B installs on Windows for every language except `objectivec`, which remains Apple-only by design. Languages whose *underlying* analyzer has no Windows build (`scip-clang` for C/C++, `scip-ruby`, and the Swift and Dart index emitters) install the wrapper and report the analyzer as missing, rather than claiming call analysis they cannot perform. A release that genuinely lacks an asset reports the version and platform rather than a bare URL, missing binary and missing checksum stay distinct, and the PATH hint printed after an install uses PowerShell/`setx` on Windows instead of `export`/`~/.zshrc`. A weekly `lang-release-drift` workflow compares every asset name the installer would request against the live travsr-lang release inventory, so the two repos cannot drift apart silently again.
- **Ruby `require` vs `require_relative` false-positive blast radius (#614).** The Ruby analyzer now records which keyword produced an import, emitting `import:gem:<spec>` for a load-path `require` (gem/stdlib/in-repo lib) and `import:<spec>` for an importer-relative `require_relative`, instead of both collapsing to the same signature. `RubyResolver` only resolves the `require_relative` form, so a gem/stdlib `require 'json'` no longer false-matches an unrelated local `json.rb`. Existing indexes need a re-index to clear stale Ruby gem-require false positives.
- **Windows hardening batch (#495, #497, #498-#507).** Twelve Windows issues fixed across the daemon lifecycle (named-pipe response delivery, console detachment, duplicate-daemon startup guard), the plugin sandbox (AppContainer capability-pointer UB, toolchain env forwarding, core-count-scaled Job Object CPU cap, idempotent ACL grants), tool resolution (PATHEXT-aware executable lookup, npm/zip installs), binary upgrades while the daemon runs, and the VS Code extension (PATH auto-detect, honest Windows ARM messaging, validated MCP config export).
- **Windows upgrade note: autostart task migration.** The Task Scheduler auto-start task name now derives from a hash that is stable across travsr builds; previously it changed with every toolchain upgrade, so `travsr daemon stop` could not remove tasks registered by older builds. Tasks registered **before** this change keep their old names and cannot be found by the new scheme: after upgrading, remove them once manually, list with `schtasks /query /fo list | findstr Travsr`, then `schtasks /delete /tn "<name>" /f` for any stale entry. Tasks registered by this version onward are removed by `travsr daemon stop` as normal.

### Changed

- **Documentation-prose retrieval (#376) is on by default (#519).** `get_context`, `ask`, and `find_references`/`find_pattern`'s doc lane now search Markdown documentation (ADRs, RFCs, plans) alongside code by default, surfacing rationale and design docs relevant to a query. All five docs-lane accuracy/regression gates are green on both bench repos (travsr and kubernetes) against merged code. Turn it off with `travsr config set docs.enabled false`.

### Security

- **`opentelemetry` bumped 0.24 -> 0.32/0.33 line (CVE-2026-48504).** `opentelemetry_sdk` 0.24.1's `BaggagePropagator` did not enforce W3C Baggage size limits before parsing an inbound baggage header, letting an oversized attacker-controlled header cause excessive CPU work and short-lived heap allocations (DoS) (#508).
- **`tract-nnef` bumped to 0.21.16 and `tract-onnx` to 0.21.17 (#494), `crossbeam-epoch` bumped to 0.9.20 (#493), `quinn-proto` bumped to 0.11.15 (#485).** Routine CVE-advisory dependency bumps surfaced by the nightly OSV scan and `cargo-deny`.

### Removed

- **Kùzu storage backend dropped (#457).** The optional, feature-gated Kùzu backend (`--features kuzu`), the `travsr migrate --to kuzu` command and its SQLite→Kùzu migration + integrity manifest, the SQLite/Kùzu parity harness, the nightly Kùzu CI workflow, and the `kuzu` dependency have all been removed. SQLite+WAL is the storage backend for both MVP and production; RocksDB remains a possible future hyperscale backend. See `docs/adrs/ADR-018-drop-kuzu-backend.md`.

**Full changelog:** https://github.com/Travsr-com/travsr/compare/v0.11.0...v1.0.0-beta.1

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
