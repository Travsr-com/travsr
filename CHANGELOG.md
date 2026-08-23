# Changelog

All notable changes to Travsr are documented here.

---

## v1.0.0-beta.2 - 2026-08-23

> Second 1.0 beta, cut from `master` after `v1.0.0-rc.1`. Install with
> `npm i -g @travsr.com/travsr@beta` or `sh -s -- --version v1.0.0-beta.2` via
> the shell installer. It carries everything in `v1.0.0-rc.1` plus the work
> below, so the `beta` channel is ahead of `rc` and `latest` in content while
> sorting below `v1.0.0-rc.1` under semver, which is why an `rc` or `latest`
> install does not pick it up. The next release candidate promotes from here,
> republishing these exact signed artifacts unchanged; see
> [Release channels](README.md#release-channels).

### Added

- **`travsr ask` answers questions about Travsr itself, and two catalogues say what you can ask and what you can run (#746, #700).** `travsr ask "what is this repo written in?"` used to match "repo" against symbol names and return a screen of `var:REPO` hits with scores that made it look like an answer. A question about the tool, or about the repository as a whole, is not a question the graph can answer, so those are now recognised before retrieval and answered in place from a 27-entry catalogue (`crates/travsr-cli/src/faq.txt`): what travsr is, how it differs from vector search, how the two indexing passes and the retrieval pipeline work, install and first run, what the graph actually contains, how MCP and the VS Code extension work, the two logs and their event keys, whether code leaves the machine, and where the data lives. A repository-composition question runs `lang list`, an index question runs `status`, and `travsr: <anything>` is an explicit route that resolves before the repository is even located, since a question about travsr is answerable with no index. Matching is against the catalogue's own questions rather than a parallel phrase list, so a new entry is reachable the moment it is written, and it is strict in one direction on purpose: a query the catalogue cannot fully account for goes to retrieval, because a confident wrong answer to a code search is worse than a missed meta question. `what calls install_hook`, `how does the parser work`, `repo_languages` and `NodeId` are pinned as tests that must still reach retrieval. `travsr ask --examples` lists the question shapes travsr can answer, filled in with the most depended-upon real symbols from your own index so every line runs as printed, and `travsr ask --cmds` prints the whole command surface, subcommands included, grouped by what each is for rather than in declaration order. Both lists are checked against clap at test time, so neither can advertise a command that does not exist, which is exactly what #727 was. An abstention now offers the nearest real symbol, the command matching the intent in the wording, and a text search, instead of "try rephrasing".

- **Live per-language progress while cross-file analysis runs (#755).** `travsr init` printed `Finalizing` and then blocked in the Phase B fan-out with nothing on screen, so a JVM cold start on kotlin or scala looked like a hang and users killed the run. A new `PhaseBLiveness` view marks each language at the start and finish of its work item, a heartbeat thread polls it every 500ms, and the CLI renders it on all three surfaces, for example `semantic: kotlin 1m34s . scala 12s (up to 5m00s per external analyzer, JVM startup is slow on first run) 2m 10s`. The ceiling comes from the transport's real handshake-plus-invoke watchdog rather than being restated by hand, and it is quoted only when something currently running is actually bounded: dart, rust, typescript, javascript and python run in-process with no per-language timeout, and telling a native TypeScript pass that it had passed a limit nothing enforces produces exactly the "it is wedged, kill it" conclusion this signal exists to prevent. A work item that panics unwinds past the normal return path, so the mark is released by a drop guard rather than by the tail, and a dead analyzer can never render as still running with a climbing elapsed. `--json` reports per-language `elapsed_s` and `bounded`, with a null `budget_s` when nothing running is bounded.

- **Struct and class fields are addressable in the graph, and field reads are use sites (#757).** Fields were reachable in 2 of 16 languages: Go emitted them, and Swift emitted them unqualified so they collided across types. Go's scheme, `field:Owner.name` contained by the owning type, is now standard across C, C++, C#, Dart, Java, Kotlin, Objective-C, PHP, Rust, Scala, Swift and TypeScript, including C's anonymous `typedef struct`, C++ unions, and Swift and Kotlin enum bodies, which parse under a different grammar node than their classes and were silently dropped by the first version of the capture. Python and Ruby stay out of scope: their fields are runtime assignments with no declaration node to anchor to. A field read (`x.foo`) now surfaces in `find_references` and `travsr references` as a real `path:line` site, carried on a new `ref/field` edge kind rather than `ref/call`, so field use sites are visible without polluting the call graph `get_callers` and `get_blast_radius` traverse; SCIP languages record theirs the same way. Resolution fails closed on ambiguity: a field read carries no crate or type hint, so two distinct types both named `Config` yielding two `field:Config.active` candidates emits nothing rather than fabricating a use site onto each. Measured on this repository: 1338 `ref/field` occurrences, `references field:SqliteStore.conn` returns 159 sites, and no `ref/call` edge targets a field node.

- **Consent-gated unsandboxed analysis on Windows, for the toolchains isolation cannot host (#743).** Java (Gradle) and Scala (sbt) cannot run inside the Windows AppContainer, so they failed inside it with nothing to show for the attempt. They now run with the user's own privileges after an explicit, persistent grant: `travsr lang allow-unsandboxed <language>` (`--revoke` to undo), which states the trade-off and requires an interactive confirmation or `--yes`, and refuses a silent grant when there is neither. The consent is per-language and machine-local, never repo-resident. Without it the language is skipped honestly and says so, rather than producing a silent zero-node run. The unsandboxed child starts from a cleared environment with an OS-essentials allowlist, so `GITHUB_TOKEN`, `AWS_*`, `SSH_*` and `NPM_TOKEN` never reach Gradle or sbt. macOS and Linux are unchanged. Recorded as ADR-017 Amendment A4 with Principal Security Engineer sign-off.

- **Every language states what it needs before it can work (#743).** A `prerequisites` field on the Phase B catalog ("JDK, Maven or Gradle" for java, "Node.js" for typescript, javascript and python, and so on), ground-truthed by reading each analyzer rather than guessed, surfaced through `travsr lang status`, `lang list --json`, the zero-symbols warning in `travsr status`, MCP `get_lang_status`, and the VS Code Languages and Blast Radius panels. It is platform-aware: the Windows Java driver builds Gradle projects only, and a Maven project fails there with an explicit message, so Windows reports "JDK + Gradle" instead of sending a Maven user to install a build tool that then does not work.

- **`travsr lang install rust` no longer requires rustup.** Rust's only route to full cross-file analysis was `rustup component add rust-analyzer`, which stranded Homebrew, distro, standalone and cargo-only CI installs. A GitHub-release fallback downloads a pinned rust-analyzer (a gzipped single binary on macOS and Linux, a zip on Windows), verified against a vendored sha256 before decompression, with a platform that has no vendored hash refused rather than fetched unverified. The daemon's resolver also searches `~/.travsr/bin`, so a stripped PATH still finds it. The fallback lives in the shared install path, so `lang install`, `lang add`, `lang detect` and the VS Code extension all inherit it.

- **`travsr lang detect --yes`** installs every detected language without prompting, for scripts and for the VS Code "Detect and install" button, which previously spawned `lang detect` with no terminal and therefore only printed a list.

- **`travsr embed reindex --rebuild`** re-embeds the whole index from scratch through the sidecar's re-embed mode, for use after switching models when older vectors may be inconsistent with newer ones. It is rejected in combination with `--phase1`: the sidecar clears the model before re-embedding, so a phase-restricted rebuild would delete every vector and refill only the phase 1 tier, silently dropping the phase 2 index from a command that reads as safe.

- **`travsrAutomation`: a release sanity and regression suite, wired into CI (#742).** The beta.1 release pass was done by hand and found four defects, three of which had already shipped in a tagged artifact; doing it by hand again for rc.1 found a fifth. This is that pass as a stdlib-only Python suite with two modes. `--binary` runs the checks against a local build, so a regression is caught before a tag exists rather than after it ships; `--tag` runs them against a published release, covering artifact checksums and cosign signatures across all five targets, build-id identity, first-run behaviour with no daemon, cross-language resolution, graph traversal, fsck, status honesty, the MCP protocol surface over a hand-rolled stdio client, and smoke coverage of every CLI subcommand. SKIP is a first-class outcome listed in the summary, because a check that did not run must never read as one that passed, and every check names the issue it guards, so a failure names the regression rather than only its own wording. `selftest.py` tests the harness itself, needs no travsr binary and no network, and runs on every pull request.

- **The daemon enforces its log-retention caps while it is running (#765).** `prune` ran once in `Daemon::run` and nowhere else, on the reasoning that daily rotation left no window for files to accumulate. The window was the daemon's own uptime: `rolling::daily` opens a file a day and deletes nothing, so a daemon left up for a month held a month of files with neither `MAX_LOG_FILES` (7) nor `LOG_BUDGET_BYTES` (50 MB) applied since boot. Both caps now re-apply when the day rolls, checked on the existing five-minute tick, and a sweep that actually drops files emits `daemon.log_pruned`, so a log directory that shrank says why.

### Changed

- **MSRV raised from Rust 1.75 to 1.88, and the MSRV job now actually enforces it.** The `time` fix above sets the floor: every `time` release carrying it (0.3.46 and later) declares `rust-version = "1.88.0"`, so 1.88 is the lowest version at which the advisory can be closed at all. `tract-onnx` 0.22 (1.85) and the `icu_*` 2.2 chain (1.86) sit below that and are not the binding constraint. `kstring` is pinned back to 2.0.2, because 2.0.3 raised its own requirement to 1.96 and would otherwise have dictated the workspace floor on its own; 2.0.2 needs only 1.73 and reaches the build solely through `liquid` in tract-linalg's build script. The workspace compiles clean on 1.88 with no source changes.

  The job that was supposed to catch all of this never ran. `dtolnay/rust-toolchain@1.75.0` installs 1.75 and makes it the default toolchain, but the repo root carries a `rust-toolchain.toml` pinning `channel = "stable"`, and a directory override outranks the default. So `cargo check --workspace` in the MSRV job resolved to stable on every run and reported success no matter what the lockfile required. Master today cannot build on 1.85, let alone the 1.75 it advertised, and the job was green throughout. The check step now sets `RUSTUP_TOOLCHAIN`, which outranks the override file, so the job fails when the workspace stops building on its declared MSRV. Anyone bumping MSRV must now update `rust-version`, the installed toolchain, and that env value together.

- **Elevated languages no longer ask for approval (#756).** Java, Kotlin, Scala and C# reach the network while indexing and required a per-user approval recorded in `lang.toml` before they could be installed. That gate is removed for local use: the resolver synthesizes the Elevated sandbox policy directly from the catalog, so the runtime isolation policy is byte-for-byte what it was and only the human approval moment is gone. `travsr lang approve` is removed with it, and `lang list --json` reports `needsApproval` as a constant false while keeping the field for older consumers. ADR-017 Amendment A5 records the two honest caveats rather than presenting the change as cosmetic: the `permitted_hosts` allowlist was never enforced by any shipped sandbox backend, and on macOS the Elevated policy already skips `sandbox-exec`, so removing the consent moment is a genuine security delta. Linux keeps bubblewrap filesystem confinement. An index built before this release that recorded `needs_approval` still reports `partial (not run: java)` rather than a false `complete`, and an existing `[[elevated_approvals]]` block is preserved as an opaque round-trip instead of being deleted on the next save.

- **`travsr connect` writes MCP wiring only; the always-on rules file is now opt-in (#746).** The rules block `connect` wrote into `CLAUDE.md`, `AGENTS.md`, `GEMINI.md` and Cursor's `.mdc` is re-read on every turn of every conversation, and it had reached 2270 characters, roughly 567 tokens per turn, competing with the user's own rules for attention. Most of it was a routing table naming which tool answers which kind of question, which the MCP client already sends: `tools/list` against `travsr mcp --stdio` hands the model all 26 tool names and descriptions, about 1245 tokens, before the conversation starts, so that mapping was being paid for twice. What remains is what a tool schema cannot say, 1076 characters, and it ships only behind `travsr connect --rules`. The trade-off is in the flag's help rather than presented as a free win: without the nudge an agent decides for itself whether to query the graph or grep. A block travsr already wrote is still refreshed by a default run, so existing installs are not stranded on the old body, while a `CLAUDE.md` that exists for its own reasons still never acquires one. A test holds the guidance under a 1200-character budget, because nothing made this cost visible before, which is how it grew.

- **One honest vocabulary for language status, and internal jargon out of every surface a user reads (#740).** Language state was computed and worded in four places that drifted: they disagreed (java "disabled" against "active", cpp "tool missing" against "registered"), overstated "built-in" for languages whose analyzer was not present, leaked "Phase B", "SCIP", "LSIF", "sandbox", "corpus", "PPR" and "RefCall", and printed Windows-impossible remedies such as "install bubblewrap". A single `phase_b::status` module now owns it (active, partial, needs approval, unsupported, with one renderer), the duplicate language catalog in travsr-mcp is deleted, and the VS Code panel renders the computed status verbatim instead of re-deriving it from a `builtin` flag. The same sweep reached the MCP tool descriptions (including `get_execution_path`, which described itself as PCST while the search is Dijkstra plus a lambda-corridor), CLI help and `fsck` output, the extension's Context Explorer legend and blast-radius toggle, and the daemon, indexer and plugin-host tracing lines. Remedies are `cfg!`-gated so every platform's string is compiled and checked rather than only the host's, and tests fail if a banned term or a decorative glyph reappears in a rendered line.

- **`travsr lang list` shows what you can act on, and installing a language enables the repo you are standing in.** The PACKAGE column (npm names nothing installs from any more) and the SANDBOX column (`Standard`/`Elevated` internals) are gone, replaced by a THIS REPO column that mirrors the per-repo gate exactly, so a language can no longer read "active" while full analysis is off for the repo in front of you. `travsr lang install <language>` run inside a repo is itself the consent signal for that repo, so it derives the repo identity the way the daemon does and grants the per-repo trust automatically: no `--corpus` flag to remember, no second command, and no untrusted-repo dead end. The gate that protects repos you never opted into is intact. The legacy `npm install -g @travsr-plugin/<lang>` install path is removed and `lang add` now forwards to `install`, so there is one install path. `lang list --json` gains a stable `repoState` tag per language and a `contract` revision per row, and `travsr status` collapses its one-warning-per-language "not enabled for this repository" output into a single line.

- **Every query surface renders one clean node name (#743).** External and unresolved Phase B nodes are synthesized from the raw compiler symbol so their edge has a destination, and those raw names leaked into output as `scip:{path}:semanticdb maven . . com/demo/Greeter#<init>().` or `sdb:com/demo/Main.g.`, cleaned inconsistently or not at all depending on which surface you asked. travsr-core now owns the single canonical renderer, and the graph tree, dot, JSON and candidate views, `get_callers`, `find_references`, `search_symbol`, `get_context`, `ask`, `get_execution_path`, `get_repo_map`, `get_dependencies`, `get_snippets` and `get_graph_json` all route through it. The SCIP symbol is decoded to the spec rather than by splitting on the first colon, so a path containing spaces or a backticked descriptor name survives. Raw identity is deliberately kept where exact identity is the point: the `travsr index` graph dump and the `explain` diagnostics. Matching logic is untouched, since nodes carry both a raw signature and a clean label.

- **`travsr graph --format json` gains `label` and edge endpoints; `travsr references --format json` returns structured JSON (#700).** The JSON graph keeps `signature` raw, so schema_version 1 readers are unaffected, and adds a clean `label` plus `from_id`/`to_id` on edges, so two endpoints whose labels collapse to the same text stay distinguishable. `references --format json` previously wrapped the human-readable envelope body in an `output` string, which was not machine-readable; it now reports symbol, resolved target, candidates, sites, total and truncated, produced by the same resolver and occurrence store the text path uses so the two cannot disagree. `total` is null rather than 0 when no count was taken, so a genuine zero stays distinguishable from an ambiguous symbol. A symbol that resolves to no definition now says "not found", distinct from a resolved symbol with zero recorded uses.

- **Em-dashes are gone from every string a user reads, and a CI gate holds it (#748).** 474 in the first pass and more after, across error messages, help text, log lines and printed output. Not a character swap: the dash was almost always joining a diagnosis to its consequence, so an imperative on the right takes a semicolon and a continuation takes a comma. Three uses are protocol rather than prose and are deliberately kept, marked at their definition and allowlisted in the gate: the `<sig> (<kind>) ... <path>` node header, which `get_snippets` and four parsers inside the VS Code extension split on, the marker line written into a user's git hooks and matched byte-for-byte to recognise travsr's own, and the single-glyph placeholder for an empty cell. Rewriting the first of those broke the round trip during the sweep and was caught by tests on both sides. `.github/scripts/check-em-dash.sh` matches the literal characters and the `\u{2014}` and `\u{2013}` escape spellings, skips comments, and runs on every pull request, which is what caught the en-dash ranges in the MCP tool descriptions and the error strings in the two LSIF emitter packages that the manual sweep never reached.

- **`travsr embed` reports what it measured, not what it projected (#772).** The interactive "Full" CPU choice now resolves to an explicit 100 percent instead of deferring to whatever `embed.capacity` happened to say, the three `embed status` percentages are clamped to 100 while the raw done and total counts stay as they are, and the projected ETA is removed from both `embed status` and the live reindex bar in favour of measured elapsed time. `embed init` with no backend and no terminal now fails naming both ways to answer, instead of printing a menu and exiting 0 having installed nothing, which reads as success in CI; `--yes` installs the recommended model. `embed list` names the config layer that decided which model is active, so it can no longer read as contradicting `embed status`.

- **The embedding sidecar floor is raised to v1.6.0 (RFC-025, ADR-019).** The re-embed mode and the model.toml key-preserving init both need it, and `embed init` now merges onto an existing `model.toml` rather than clobbering keys it does not own, such as a hand-set `macos_engine`. The floor is a single host constant, so a sidecar at or below v1.5.0 is refused for all embedding, background reindexing included, until it is reinstalled. ADR-019 records that blast radius as an accepted trade-off for consistency, with a per-operation split noted as the escape hatch.

### Fixed

- **Unbounded memory in eight places, found by the #736 RCA (#736, #737).** All of them were portable application logic, so the growth reproduced identically on macOS, Linux and Windows. (1) `ParseCache` had no eviction path at all, so every init worker held a full clone of the parsed graph for the whole run; it is now a byte-budgeted FIFO (64 MB) whose insertion order is a `VecDeque`, and a re-insert credits the existing entry's cost before the budget check so a refresh is charged as a delta rather than evicting neighbours to make room for bytes it is about to release. (2) The indexer's dirty-file channel was unbounded, which defeated the 256-slot watcher backpressure while being fed 100k paths per reindex from inside its own consumer; it is now a bounded `sync_channel` and every producer sheds with `try_send`, which never blocks, so the self-feeding path cannot deadlock, and shed events are recovered by the gc-tick head reconcile. Shedding logs in aggregate, at most one warning per 10s with a count, instead of one per event during exactly the burst the bound exists to absorb. (3) Embed hooks spawned a thread per invocation and abandoned it on every 600ms timeout, so a slow or cold sidecar piled them up behind the sidecar mutex without bound; one long-lived `HookWorker` per hook type replaces that, requests carry the caller's deadline so a slow sidecar does not burn its time producing results nobody can receive, a failed spawn leaves the hook unarmed rather than stalling every caller for 600ms forever, and a worker whose handler panicked reports itself once instead of silently returning empty. (4) `QueryCache` gained a 64 MB byte budget beside its 256-entry cap, since values are whole JSON query results that can legitimately reach 256 MiB. (5) The MCP SSE ring buffer gained a per-session 4 MiB byte budget with an RAII in-flight guard, so a client disconnect can no longer leak an rpc-id entry, and an oversized payload is refused outright rather than buffered after draining the ring. (6) The embed supervisor's respawn swapped the new sidecar inside the `Arc` the injected hooks hold, instead of replacing the `Arc` and orphaning every hook on the dead child. (7) The indexer caps child stdout at 512 MiB with kill and reap on overflow, streams LSIF ingest from any `BufRead` so the full dump is never materialised as a `String`, and no longer leaves zombies behind a kill without a wait. (8) Worker and buffer sizing now reads cgroup v2 and v1 CPU quota and memory limits, so a container gets its real limit rather than the host's core count; the CLI bounds the tokio blocking pool at 4x effective cores, and daemon control connections take a 64-permit semaphore before their blocking work.
- **Daemon runaway memory/CPU after a killed `embed reindex` left stale lock state (#735).** Three defects compounded. (1) The daemon's 60s embed tick had no single-flight guard: a tick body still running when the next tick fired simply overlapped it, and in the crash-residue state (`embed_text_model_id` stale after an interrupted regeneration) every overlapping pass re-ran `clear_all_embed_texts` plus a full-repo parse, undoing the others' progress, so passes and their full-corpus buffers accumulated without bound (the reported ~3 GB/min, with no log line because the skip path only ever logged at debug). The tick body is now single-flighted (`EmbedTickGuard`), and the tick uses `MissedTickBehavior::Delay` so a stalled runtime can no longer replay missed ticks as a burst. (2) The reindex no-progress watchdog opened a fresh read-only SQLite connection to embed.db twice per 500ms poll; against the multi-GB `embed.db-wal` a killed reindex leaves behind (its final `wal_checkpoint(TRUNCATE)` never ran, and a read-only connection cannot truncate it), every open paid full WAL recovery and a heap-built wal-index, silently burning a core. The watchdog now holds one lazily-opened connection per reindex run (`ProgressCounter`), paying recovery at most once. (3) `EmbedOpLock::try_acquire` classified EVERY `try_lock_exclusive` failure as contention and rendered it with whatever pid the (possibly stale) `embed.lock.info` contained, so a non-contention lock failure surfaced as "another embed operation is already running (reindex, pid <dead>)" forever. Only the platform's contended error now classifies as contention; other lock failures surface immediately as hard errors naming the real cause. The info file is now advisory only and honestly lifecycle-managed: it is deleted on clean release (leftover metadata is therefore an honest crash signal), logged and replaced when a free lock is acquired over residue from a dead holder, and, when the lock is genuinely held while the recorded pid is dead, the conflict message says the record is stale instead of naming a process that exited days ago. The OS lock remains the single source of truth, so a held lock is never stolen based on recorded-pid liveness (which would race PID reuse), and a dead holder can never wedge future acquisitions (flock/LockFileEx release on process death).
- **Windows dropped every rust-analyzer edge, silently (#738).** The LSIF path normaliser could not relativize rust-analyzer's forward-slash `file://` URIs (`D:/...`) against the daemon's backslash, `\\?\`-extended repo root, so every resolved reference was dropped fail-closed and no type-resolved semantic edge landed on Windows at all. Both operands are now normalised (verbatim and UNC prefixes stripped, separators unified, drive letter lowercased) and matched with a plain case-sensitive prefix strip, so Unix behaviour is byte-for-byte unchanged. The existing tests masked the bug by pre-normalising the root. On this repository the fix takes ref/call edges from 5,993 to 9,954, of which 9,444 are type-resolved. Two contributing defects from the same investigation are fixed with it: the rust-analyzer version probe now requires a successful exit, so a failing rustup proxy shim is no longer read as available, and a run where rust-analyzer executed but zero references survived resolution is now reported as degraded instead of being invisible.

- **`travsr status` reported a healthy index as permanently stale (#741).** `phase_b_commit == last_commit` with `phase_b_dirty = 1` renders as "stale (run travsr init to refresh)". The daemon's semantic pass cleared that flag when it advanced the marker; the init path stamped the marker and left the flag alone. Init's first pass sets the flag (rewriting a file's nodes drops its ref/call edges), init's semantic pass then rebuilds exactly those edges, so by the time the marker was stamped the flag described a degradation that no longer existed, and since every subsequent `travsr init` repeated the sequence, the remediation the message named was the command that reproduced the state. The graph was correct throughout, which made this a status-honesty bug rather than staleness. The message also now names `travsr init --semantic`, because plain `init` defers the semantic pass to the daemon and genuinely cannot clear the flag, so the old advice could not work even after the fix. Clearing is guarded against a concurrent reindex: `init.lock` serialises two inits but does not exclude the watcher, so a save during the semantic pass could set the flag for an edit init never saw and have it cleared anyway, turning an honest `stale` into a silent false `complete`. A counter bumped whenever a reindex marks dirty now decides, and init logs why it left the flag set.

- **`travsr pattern` failed in any repository that had moved since it was indexed, and blamed the regex for it (#747).** `meta.repo_root` is stamped at index time and named wherever the repo lived then, and `find_pattern` handed it to `git -C`, so a moved checkout, a clone that inherited a copied `.travsr/`, a CI mount at a different point or a renamed home directory all failed identically with `fatal: cannot change to ...` followed by advice about POSIX ERE. The database travels with the repository, so its own location cannot go stale the same way: the root is now derived from the database path (requiring the `<repo>/.travsr/graph.db` shape rather than assuming it, so a fixture database opened elsewhere yields nothing instead of a confident wrong root), and the stored value is preferred only while it still resolves and the derived path is not itself a repository. All seven readers of the key go through the resolver, not just `find_pattern`: `get_context`, `get_snippets`, the call-site expansion in `get_callers`, `get_daemon_logs` (which returned nothing at all from a moved repo), the layered config load (which silently fell back to global and env defaults) and the reranker's snippet text (which scored candidates on empty strings). `reindex_files` re-stamps the key as well as `init`, so the stored value self-corrects. The "use --fixed" hint is now conditional on the failure actually being a bad pattern, matched against the messages git really prints on both glibc and BSD, with quoted spans ignored so a repository path cannot trigger it.

- **A language whose analyzer returned definitions but not one reference was reported as a success (#724).** The zero-output warning was widened for native analyzers, which legitimately emit no new definition nodes because they attach edges to existing tree-sitter nodes. That reasoning does not carry to an external analyzer, where nodes, edges, references and unresolved calls all arrive in one response: definitions with no occurrence of any kind means the tool succeeded and dropped every reference, so no call edge can ever come from it. That is what scip-java was doing, and because the SCIP it returned was full of definitions, nothing warned; the run reported success and the graph gained no Java relationships. Those languages now land in their own bucket with their own message, and the warning is persisted as a `no_references:{lang}` class so it reaches `travsr status` and MCP `get_index_status`, not only the inline progress renderer that the default `init` path never prints. The `zero_nodes` and `needs_consent` classes, which had the same silent-`done` gap on the MCP side, are decoded there too.

- **The VS Code Languages panel rendered a stale binary's output as a table of confident wrong answers (#755).** With `travsr.binaryPath` empty the extension resolves whatever is on PATH, which can be an older npm-bundled build whose `lang list --json` rows omit `status`, `statusLine`, `repoState` and `prerequisites`. The panel read all four, so every row came out Semantic=partial, This repo=undefined and Prerequisites=`-` (which is the claim that the analyzer needs nothing), builtins were offered an Install, and nothing said the binary was old, because both builds self-report `1.0.0`. The gate is therefore field presence rather than a version string: the parser returns the rows and a verdict on their shape together so a caller cannot render one without seeing the other, a field counts as missing only when no row carries it (extra fields from a newer binary are never an error, and empty or error output is not evidence of age), and the panel withholds the rows behind one sentence naming the resolved binary and what it failed to report, with Download and Set-binaryPath buttons. Per-cell lookups degrade to "unknown" with a remedy tooltip instead of asserting a default. `lang list --json` also states a contract revision per row so skew is detectable positively, pinned by a test on each side. Two things surfaced while wiring it up: the panel ran `lang list --json` under the 4s timeout shared with the other read-only calls, and that command sweeps PATH per catalog language and takes about 17s on Windows, so it was being killed mid-flight and its partial output parsed as an empty catalog, meaning the panel claimed no analysis tools existed on exactly the machines slow enough to need the list.

- **Seven smaller correctness and honesty defects folded in from the same end-to-end pass (#755).** `lang install --version` reached only the analyzer, so the wrapper came down at latest and a hash-pinned analyzer then failed, leaving a wrapper version the user never asked for; the pin is now decided before any network work. A language whose analyzer never ran left `graph` and `callers` silently empty while `travsr status` said `partial (not run: php)`, so a per-query degraded note now covers exactly the classes `status` downgrades for. The bad-symbol error for `get_execution_path` hedged "source X and/or sink Y" when only one side had failed, and now names the side that failed, without stating why, so a denied symbol and a nonexistent one stay byte-identical. `travsr graph --budget` no longer prints "N more nodes beyond budget" when the budget was too small to render anything, and a seeded `--budget 1` says the budget fits only the queried symbol. A same-file struct and impl collision, where the only advice offered (a `--path` hint) narrows nothing, now points at the exact signature that resolves uniquely and appends a runnable one to each candidate.

- **A fresh TypeScript index left orphan edges that `fsck` could not explain (#755).** The LSIF emitter and tree-sitter disagreed on the identity of a top-level `const` bound to an arrow function: tree-sitter classifies `const shout = (s) => ...` as `fn:shout`, while the emitter computed `var:shout` because the TypeScript AST binds references to the variable declaration. The ingester builds edges to the identity the emitter reports and tree-sitter owns the nodes, so every reference to an arrow-function const became an edge to a node that was never written. The emitter now mirrors tree-sitter's two rules exactly: a top-level declarator whose initializer is an arrow function or function expression is `fn:`, a generator (`function*`) is not, because tree-sitter parses it as a distinct node kind and writes `var:`, and a non-top-level declarator gets no identity at all, since tree-sitter drops locals entirely and any identity for one guarantees an orphan for every reference to it. An ambient `declare const` gets none for the same reason. The fix recovers edges rather than merely satisfying fsck: the formerly orphaned references now land on `fn:shout` and answer `graph --direction callers`.

- **Windows semantic analysis, across five separate failures.** C# produced zero nodes because the daemon's repo root is canonicalized with the `\\?\` extended-length prefix and scip-dotnet feeds it into `System.Uri`, which reads that as a UNC authority and aborts the whole index before `dotnet restore` runs; the prefix is now stripped once for every external analyzer when building the invocation root. The dotnet SDK root is resolved with a PATHEXT-aware lookup that requires an actual `sdk/` directory, since the old PATH scan never matched `dotnet.exe`. scip-java is a coursier polyglot JAR and stays available on Windows, so the installer writes a `.cmd` that runs it through the JVM, and kotlin-language-server gets a `.cmd` targeting its `.bat` launcher instead of a non-runnable shell script. The AppContainer now grants read and execute on toolchain bin directories (never on module or package caches) so a sandboxed analyzer can run the toolchain it shells out to, and on `~/.travsr/kls`, where kotlin-language-server extracts the jars its launcher execs. Occurrence paths that surfaced as `//?/C:/...` in references are normalised in both slash shapes and at the emitter.

- **The graph webview crashed on ordinary repositories, taking every control with it.** Three independent causes. A node kind that collides with an `Object.prototype` member, `constructor` being common in Java, made the kind-to-style lookups return the inherited value instead of the fallback, so cytoscape was handed the `Object` function as a shape and threw inside the render loop, killing zoom, pan, hover and every button at once; lookups are now own-property only, and unknown or prototype-named kinds fall back safely. On a strict-CSP build, `style-src` had no `unsafe-inline`, so cytoscape's inline styles were blocked and its renderer crashed the same way (`script-src` stays nonce-based). And node ids carrying whitespace, backticks, `#` or brackets, which synthesized external-symbol nodes routinely do, broke the cytoscape selector in both query and overview mode; ids are remapped to safe tokens with edge endpoints rewritten through the same map, while display fields and the ids `exportJson` emits stay real. The hover tooltip element the renderer had always read and styled but never created was also added.

- **`travsr init --force` did not actually re-parse (#757).** The graph purge left the per-file content-hash cache intact, so the hash delta skipped every unchanged file and reported "up to date" over an index an older binary had built, which is why newly added field nodes never appeared after an upgrade. `--force` now clears the hash cache so every file re-parses.

- **The embed lock blamed a process that had exited days earlier (#735, #745).** The contention message read `.travsr/embed.lock.info` and named whatever pid it contained, unchecked, and that file is only written on a successful acquire, so a reindex killed mid-run left it naming a process that no longer exists: the daemon logged a conflict against a dead pid once a minute and told the user to stop something that was not there. The recorded pid was never evidence of the holder, since the OS releases the lock when a process dies, and two opens in the same process conflict with each other, so "another operation" was wrong as well as unhelpful. The message now distinguishes the cases it can actually establish and prescribes only actions it can justify, names `lsof`/`fuser` on Unix and `handle.exe` or Resource Monitor on Windows for finding the real holder, and says nothing about liveness on a platform with no safe probe. Supporting fixes: a pid of 0 no longer reads as alive (`kill(0, ...)` addresses the caller's whole process group, so the check trivially succeeded), 0 and values above `i32::MAX` are treated as a corrupt file rather than a stale pid because travsr cannot have written them, the info file is cleared before the lock is released so a long-lived daemon cannot match its own stale record against a lock somebody else now holds, and the retry path logs its own line instead of emitting the settled conflict message up to 50 times while the caller is still waiting.
- **`npm i -g @travsr.com/travsr` failed to extract the binary on Windows.** `ensure-binary.js` ran `tar -xzf <path>` with an absolute Windows path (`C:\...\node_modules\@travsr.com\travsr\bin\travsr-*.tar.gz`), and GNU tar (the `tar` on PATH in a Git-for-Windows shell) parses the colons in it, both the drive letter and the `@travsr.com` package scope, as a `[user@]host:path` remote-tape spec, so it tried to rsh/ssh into the "host" and failed with `Cannot connect to ...: resolve failed` instead of extracting. `v1.0.0-beta.1`'s and `v1.0.0-rc.1`'s Windows smoke-install jobs both hit this at the same step. tar now runs with its working directory set to the destination and relative file names only, which contain no colons; this works identically under GNU tar and bsdtar (macOS's and stock Windows' `tar.exe`, which notably rejects GNU tar's `--force-local` escape hatch, so that flag was not an option).
- **Every release tag would have failed preflight.** The MSRV bump renamed the `ci.yml` job to match, but `release.yml`'s preflight matches required checks by exact string and still listed `MSRV (rust 1.75)`. No check by that name is produced any more, so the loop marked it MISSING and exited 1, failing every tag on a check that could not go green rather than on a real regression. The MSRV bump instructions now name this list, and the `ci.yml` comment says to grep the old version before bumping, and warns that rationale citing the MSRV needs rereading rather than renumbering.

- **The promote path attached the wrong installer, and could publish a release with none at all (#713, #714).** Promotion checked out the dispatch ref, so it attached master's `install.sh` to the promoted release instead of a snapshot of the tag being promoted; the script now comes from `git show "$FROM_TAG:install.sh"`, matching how every other promoted asset is sourced. Separately, `action-gh-release` skips a listed file that does not exist without failing, so promoting a ref that predates `install.sh` would have published a release with no installer and stayed green, which 404s `travsr.com/install.sh` for everyone (it redirects to `releases/latest/download/install.sh`). Three guards now cover it: the extraction step errors with a readable message before the tag is created, `fail_on_unmatched_files` is on for both release steps, and a post-release assertion downloads the published asset and compares the bytes against the installer the run attached, keyed to the release's own tag.

- **Nothing checked the installer pointer users actually hit (#750).** The per-tag assertion proves the release just cut serves the right installer and says nothing about `releases/latest`, which is what `travsr.com/install.sh` redirects to. This repository had already been bitten there: `latest` sat on v0.11.0 while v1.0.0-rc.1 existed, so the installer being served was months older than the newest release, and nothing noticed. Both the publish and promote jobs now resolve `releases/latest`, assert it names the tag this run published, then fetch the installer through the redirect a user actually follows and compare the bytes, gated on stable releases since a prerelease does not move the pointer. The download, the listing and the comparison all sit inside the retry loop, so a CDN lagging for a moment fails the gate no more than a genuinely bad release passes it.

- **OSV Scan had been red for 76 consecutive nightlies, and would have blocked the promotion to stable.** `osv-scanner` exits non-zero on any finding and knows nothing about `deny.toml`, so the three advisories the project had already triaged failed it on input `cargo-deny` reports as clean. Two of them (`paste` and `proc-macro-error2`, both unmaintained notices with no patched release and no runtime attack surface, being compile-time proc-macros) are now recorded in `osv-scanner.toml` with the same reasoning `deny.toml` carries; the third had a fix and was fixed rather than suppressed. The `proc-macro-error2` entry carries an expiry rather than being permanent, because it is a dated build risk rather than a security one, and both files state where they deliberately diverge so the next maintainer does not "fix" an intentional difference. Relying on a scanner going red as the reminder for an expiring ignore is not enough here: four scheduled workflows were chronically red when this was written, so a fifth would have been invisible for the same reason the original failure went unnoticed for two months. `.github/scripts/check-advisory-expiry.sh` now parses the deadlines and runs on every pull request, warning at 60 days and failing at 14. Two silent-skip bugs in the checker itself were found and fixed while testing it, including a date regex using interval expressions that the runner's `awk` does not support, which made it find no deadlines at all and pass.

- **The new fuzz-lockfile audit was passing without checking.** `fuzz/` pins nightly for cargo-fuzz's sanitizer flags, so cargo-deny's crates-index fetch went looking for a `stable` toolchain, did not find it, and gave up with "unable to check for yanked crates" on every crate while still exiting 0. cargo-deny reads only the manifest and lockfile, so the step now pins `RUSTUP_TOOLCHAIN=stable`, matching the root step above it.

### Security

- **Cleared every open dependency advisory that had an upstream fix (RUSTSEC-2026-0009, GHSA-4w2j-m93h-cj5j).** `time` moves 0.3.41 to 0.3.55, closing the RFC 2822 parser stack-exhaustion DoS. The blocker was not `time` itself but `tract-linalg` 0.21.17, whose build script declared `time = ">=0.3.23, <0.3.42"`; because Cargo unifies a package to a single version across normal and build dependency graphs, that build-only ceiling also held `tracing-appender`'s runtime copy below the fix. `tract-onnx` therefore moves 0.21 to 0.22.3, which relaxes the bound to `^0.3.23`. The new floor is `0.22.2`, not `0.22.0`: RUSTSEC-2026-0217 (integer overflow in `tract_nnef::tensors::read_tensor` giving an out-of-bounds read on model load) is unpatched in 0.22.0 and 0.22.1, so a plain `^0.22` would have reintroduced the very advisory the earlier 0.21.17 pin existed to escape. Separately, `quinn-proto` moves 0.11.14 to 0.11.17 in `fuzz/Cargo.lock`, closing a remotely triggerable memory exhaustion via unbounded out-of-order stream reassembly; the root lockfile was already on the patched 0.11.15.
- **`fuzz/Cargo.lock` is now audited in CI.** `fuzz/` is a deliberately separate workspace (isolated so `cargo-fuzz` can use nightly) with its own lockfile, and the `cargo-deny` job only ever ran against the root manifest, leaving that entire dependency graph unscanned. That gap is how the vulnerable `quinn-proto` above went unnoticed while the root lockfile was already patched. A second `cargo-deny` step now runs with `--manifest-path fuzz/Cargo.toml`, reusing the root `deny.toml`. It checks advisories only, since the fuzz package is unpublished and carries no `license` field. Note that `cargo-deny` alone would not have caught this one: the advisory has no RUSTSEC ID and lives only in the GitHub Advisory Database, so Dependabot remains a necessary second source.
- **`h2` 0.4.14 to 0.4.16 (RUSTSEC-2026-0258).** It accepts and queues empty DATA frames without limit, so an unbounded number can accumulate when streams are not drained, ending in unbounded memory use or a panic on overflow. Low severity, and it reaches the workspace only transitively, through hyper under axum, reqwest and tonic. Lock-only change.

- **The Windows unsandboxed analysis path cannot see the daemon's secrets.** The consent-gated plain child added for Java and Scala starts from a cleared environment with an OS-essentials allowlist, so `GITHUB_TOKEN`, `AWS_*`, `SSH_*` and `NPM_TOKEN` never reach Gradle or sbt. Separately, Scala's repo-root write grant is narrowed to `target/`, `project/target/` and the generated `.travsr-semanticdb.sbt`, with the repo root staying read-only under both bubblewrap and Seatbelt; a Linux security test asserts that a write outside those subpaths is still denied.

- **Three supply-chain and injection gaps in the new release-check harness, found in review before it ever gated a release (#742).** Its archive extraction validated member names but not link targets, so a symlink member pointing at `../../../etc/passwd` passed the check and was created; it now extracts with the data filter, with a hand-rolled name and linkname validation for interpreters older than 3.12. `ci.yml` interpolated `${{ }}` dispatch inputs directly into bash in a job holding a repository token, so a tag of the form `x"; curl ... | sh; #` would have executed as script; every such value is now bound through `env:` and the tag is validated against a git-ref character set before use. And the cosign gate accepted signatures the installer itself would reject: it matched any workflow in the repository on any ref, while `install.sh` and `release.yml` both pin the workflow and the tag ref, so a suite reporting the supply chain as verified was testing something weaker than what users run. The identity is now read out of `install.sh` at runtime and compared, so a tag-grammar change cannot leave this copy behind, and the loop iterates the tarballs rather than the signature bundles, so an artifact that was never signed fails instead of simply not appearing in the list.

### VS Code extension (vscode-v0.11.0)

Full detail in [packages/travsr-vscode/CHANGELOG.md](packages/travsr-vscode/CHANGELOG.md).

- **A chosen active repository for multi-repo workspaces.** With several git repos open, a multi-root workspace or one folder holding siblings, the Languages panel always targeted the first workspace folder, which was wrong or ambiguous and could not see sibling repos nested inside a single folder. Repos are now discovered through the built-in Git extension, the target is picked once and remembered per workspace, and it is shown in a clickable status-bar item, so `lang install`, `lang detect` and `init` all run where you meant them to. Adds the `Travsr: Select Repository` command.
- **The Languages panel tells the truth about what can run here.** Every CLI status is rendered instead of collapsing to "partial": a platform-unsupported language gets a disabled "Not available on <OS>" cell and never an install button, a consent-gated one gets a single "Allow and enable" action, and the Prerequisites column names the tool a language actually needs. The "Detect and install" button installs (it previously spawned a command with no terminal and only printed a list), install and init run under a cancellable progress notification with no wall-clock kill, and a non-zero exit is reported as an error instead of a false success.
- **The daemon log in the stats panel.** It read only the newest rotated file, so shortly after 00:00 UTC "the last 500 lines" was a handful of lines and the rest of the answer sat in yesterday's file, which reads as a daemon that logged almost nothing. A File control now lists the rotated files newest first, labelled with the day and size, and reads the one you pick. An `Auto` control refreshes on a chosen interval (off, 5s, 15s, 30s, 60s, off by default) by swapping only the log rows, so the filter box, severity chip, toggles, selects and scroll position all survive a tick. The `Follow` checkbox it replaces never worked: its timer lived in the document that each refresh replaced, so it polled exactly once.
- **The panels follow the editor theme, not the desktop appearance.** Both webviews and `media/graph.css` put their light palette behind `prefers-color-scheme`, which inside an Electron window resolves from the OS. A light desktop with a dark VS Code theme therefore rendered a linen panel inside a dark editor, and no theme change could fix it, because the theme was never the input. They now key on the theme kind VS Code stamps on the webview body, which is authoritative and updated live, so a theme change repaints without a reload.
- **The graph webview no longer dies on ordinary repositories:** a node kind colliding with an `Object.prototype` member (`constructor`, common in Java), a strict-CSP build blocking cytoscape's inline styles, and node ids carrying whitespace, backticks or brackets each crashed the renderer outright, taking zoom, pan, hover and every button with it.
- **Installs land where they should.** The panel passed `--corpus <folder-basename>` on install while the daemon keys the per-repo gate on the git-remote-derived identity, so the grant landed on a key nothing matched and the repo still read as untrusted after installing through the extension. The guess is gone; the CLI is run with the target repo as its working directory and derives it the same way the terminal does.

**Full changelog:** https://github.com/Travsr-com/travsr/compare/v1.0.0-rc.1...v1.0.0-beta.2

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
