# Changelog

## [Unreleased]

### Added

- **The Health panel's Languages table separates "installed on this machine" from "on for this repository", and can enable a language for the repository in one click.** `travsr lang list` has always reported both facts, but the table collapsed them into one column, so a language whose analyzer was installed everywhere and merely switched off for the open repository showed the same "Install analyzer" button as one with no analyzer at all, and that install changed nothing. The table now has a **Global installed** column (the analyzer is on this machine) and a **This repo** column (the CLI's own per-repository state: always on, enabled, not enabled, no analyzer), and the Action column offers the one step that applies: **Install analyzer** when nothing is on the machine (the install also enables it here), **Enable for this repo** when it is installed but off here (runs `travsr lang install <language> --skip-wrapper`, which turns it on in config without downloading, then offers a semantic re-index), **Allow & enable** where the OS needs a one-time permission, **Re-run semantic** where a warning on the page says the language resolved nothing, and **Disable** beside any enabled non-builtin. Prerequisites (a JDK, a Go toolchain) sit under the language name. Only languages the graph actually found in the repository (`repo_languages`) are listed: enabling Kotlin for a repository with no Kotlin files changes nothing, and sixteen catalog rows for a three-language repository buried the ones that mattered. A repository the graph has not indexed yet is not gated, so the table never goes empty on a fresh checkout. Languages the repository uses but this OS cannot analyse say so instead of offering an install that would dead-end. The partial count and the Semantic tile cover only languages that are in the repository and can run here.

- **The Health panel leads with a verdict, and every problem it reports carries the fix (#820).** The panel opened with five metric tiles and a grey "1 warning" line, which is the wrong shape for the question it is opened with. A session captured while writing this had the daemon stopped four minutes earlier, a graph nine hours stale and Java resolving zero symbols in an all-Java repository, and the page still rendered green. It now opens with one of five verdicts, each carrying a word as well as a colour: **Not running**, **No graph yet**, **Stale**, **Degraded**, **Healthy**. A stopped daemon outranks staleness, because nothing below the banner is live in that state and the metrics are being read from the graph on disk rather than from a live index; staleness outranks analyzer warnings, because a stale graph makes every other number on the page describe a commit you are no longer on. The verdict carries its own action where one exists: Start daemon, Index this repo, Reindex.
- **Drift is read from the daemon rather than re-derived.** An Index and daemon section names the indexed commit, the checkout HEAD and how far ahead it is, whether the working tree has changes that are not indexed yet, and the Phase A state. It comes from `get_index_status`, whose `is_stale` is tri-state and already handles short-SHA comparison and linked worktrees (#636); recomputing that from the Git extension's HEAD would have given the panel a second opinion that can disagree with `travsr status`. A binary too old to serve that tool reports "not reported by this binary" rather than having its gaps rendered as answers, and the Last indexed tile turns amber **and says "stale"**, so the state never rides on the colour alone.
- **Fixes run from the panel.** A diagnostic's command gets a Run button beside a Copy button, and Run opens a terminal named "Travsr" that is reused while it is alive and recreated once closed. The webview posts an index, never the command text: the extension looks the command up in the diagnostics it just rendered, and `parseTravsrInvocation` admits only `travsr` followed by argument-shaped tokens, so a hint carrying `;`, `$(...)`, a pipe or quotes is copied rather than run. Quoting is chosen per shell, with PowerShell getting the call operator only when the executable path actually needed quoting.

### Fixed

- **A long repository name no longer paints over its status in the Repositories section.** The key column of every Health row was a fixed 110px box, so `inboundrequesthandler` ran under the tick beside it. The column is now a minimum width that wraps long names inside their own cell, and the full name is in the cell's tooltip.
- **The Daemon section's "Last stopped" now carries the date.** It showed a bare clock time read from the log, which on a panel opened days later reads as "today". The log's own RFC3339 stamp carries the day, so the row now shows local date and time (`2026-09-04 20:15:09`).
- **The Health panel claimed the daemon was running when it was not.** It read the MCP client's connection state as the daemon's state, and those are two different processes: the extension spawns its own `travsr mcp --stdio` child, which opens the database directly and keeps answering with no daemon anywhere. So the page said "running" beside a terminal saying `daemon: not running`. It now asks `travsr daemon status`, the same source a terminal shows, and renders that sentence verbatim so "starting", "not responding" and "last start failed" stay distinct rather than collapsing into one word. Daemon state and query state are reported as separate rows, because they are separate: a stopped daemon costs freshness, not answers, and that case gets its own verdict, **Not watching**, naming the consequence that commits and saves will no longer refresh the graph.
- **The Languages table contradicted the warning beneath it, and invented a column.** `lang list` reports what is installed and enabled; `travsr status` reports what actually resolved. A language could therefore show a tick and the word "full" directly above a card saying it had produced no symbols. Rows are now cross-referenced against the warnings on the same page and point at the card instead. The Symbols column is gone: that JSON carries no symbol counts, so its "resolved" and "0 symbols" values were fabricated, and it now shows the CLI's own status line. The Semantic tile counted the same way, reading "0 symbols" while five of six languages were fully analysed; it now counts languages.
- **A partial language offered the wrong fix.** Every incomplete row offered a semantic re-index, while the CLI's own status line for a language with no analyzer on disk says to run `travsr lang install <language>`. Those are different commands and the re-index changes nothing there, so the row now offers whichever fix actually applies.
- **The Log file row's Open button pointed at a path that does not exist** (`.travsr/logs/` rather than `.travsr/`), so it could only ever fail. It now scrolls to the Daemon log section on the same page, which reads the same file with severity filters, a rotated-file picker and an auto-refresh.
- **A diagnostic no longer prints its whole message twice.** `title` is `hint` with the trailing "run `cmd`" clause cut off, and the card rendered both, so the heading read as a sentence truncated mid-thought ("Fix the project setup, then") and the body repeated it in full underneath. The heading is now the claim and the body appears only when it says something the heading did not.

### Changed

- **The Languages panel is folded into Health.** It showed the same `travsr lang list --json` rows the Health page reads, so the two surfaces were one table rendered twice, with the second one missing the diagnostics cross-reference. The `Travsr: Languages` command and its view-title menu entry are gone from the palette; the command id `travsr.showLanguages` remains registered as an alias that opens Health, so an existing keybinding still lands somewhere useful. The stale-binary guard (#755) moved with the table: a `lang list --json` payload that predates the fields the table reads is withheld and the Languages section names the binary and offers the download and the setting, exactly as the panel did. Node counts per language, which the old panel listed from `repo_languages`, are not carried over.
- **`Travsr: Graph Stats` is now `Travsr: Health`.** The panel already reported daemon status, index freshness and the repository's diagnostics, so naming it after one of its metric groups sent people looking elsewhere for the answer to "is Travsr working". The command id `travsr.showGraphStats` and the `travsrStats` view type are unchanged, because both are API: an existing keybinding, and the view-title menu entry, keep working. The in-panel heading and its subtitle change with the command title (RFC-028).
- **The graph webview labels landmarks instead of everything.** Cytoscape has no label collision avoidance and every node asked for its label at every zoom, so a file like this repository's own `crates/travsr-core/src/exec.rs`, at 157 nodes, drew 157 names on top of each other and the structure the graph exists to show was buried under its own annotation. Ordinary nodes now give up their label until either the view is zoomed in past `LABEL_ZOOM`, or the pointer is on their neighbourhood, and the seed node plus the ten busiest hubs keep theirs at every zoom, so the count of labels no longer grows with the graph. Anything the user singled out (the current selection, a search hit, the blast rings) keeps its name whatever the zoom, since suppressing those would break the feature that put the node on screen. Package tiles, ghosts and compound parents are exempt throughout: for those the label is the content, not an annotation on a dot. The seed node also gains a brighter, bolder name, having previously been capped at the same size as any hub and so indistinguishable from one.

- **The binary the extension offers to download moves to v1.0.0.** `DOWNLOAD_VERSION` is the release the extension fetches when no `travsr` binary resolves from `travsr.binaryPath`, `~/.travsr/bin` or PATH. It had been held at `0.11.0` for the whole 1.0 prerelease train on purpose, because the release workflow exempts pre-releases from the lockstep check and pointing the one recovery path at a stable tag that does not exist yet would have made it a guaranteed 404. With `v1.0.0` published that exemption ends, and leaving it would make a fresh install pull 0.11.0 over a 1.0 daemon (#486).

## [0.11.0] - 2026-08-23

Requires travsr v1.0.0-beta.2 or later for the language-status fields the
Languages panel reads; an older binary is detected and reported rather than
rendered as a table of wrong answers (see #755 below).

### Added

- **A chosen active repository for multi-repo workspaces.** With several git repos open (a multi-root workspace, or one folder holding sibling repos), the Languages panel had no way to know which repo an install should target: it always used the first workspace folder, which was wrong or ambiguous, and it could not see sibling repos nested inside a single folder at all. `ActiveRepo` discovers every open repo through the built-in Git extension API (falling back to workspace folders), lets you pick the target once, remembers it per workspace, and shows it in a clickable status-bar item so the destination is always visible. `lang install`, `lang detect` and `init` all run with that repo as their working directory, and when the choice is ambiguous and unpicked the first such action asks once and then remembers. Adds the `Travsr: Select Repository` command and a "change" affordance in the Languages panel header, shown only when more than one repo is open.
- **The Languages panel renders every language state honestly, not just "partial".** A language with no analyzer for this OS gets a disabled "Not available on <OS>" cell and is never offered an install; one that needs your permission to run outside isolation gets a single "Allow and enable" action that records the permission and re-indexes; everything else gets a plain Install. A Prerequisites column names the tool each language actually needs (JDK and Gradle, Node.js, and so on), and a "This repo" column says whether full analysis is on for the repository you are working in, with a tooltip giving the one command that changes it.
- **"Detect and install" installs.** The button spawned `lang detect` without a terminal, so the CLI only printed a list and nothing was installed. It now runs the non-interactive install path under a cancellable progress notification, and reports the CLI's real final status rather than a fixed timer's guess.
- **The daemon log now reads across rotated files (#765).** The reader opened only the newest `daemon.log.<DATE>`, and rotation is daily, so "the last 500 lines" is rarely 500 lines of one file: shortly after 00:00 UTC today's file holds a handful of entries and the rest of the answer sits in yesterday's. The panel came back short without saying so, which reads as a daemon that logged almost nothing, and a user watching across midnight saw the log empty itself while the daemon was healthy. The CLI had this right all along in `LogTail::backfill`; the panel had its own TypeScript reader and never received the fix. `readDaemonLogTail` now mirrors that walk in-process, pinned against the CLI on a shared fixture so the two cannot drift, and the fixed 512 KB single read is replaced by a backward chunked scan per file so cost is proportional to the tail rather than to file size. Lines are accumulated per file rather than concatenated, so a rotated file whose last write was torn cannot fuse its final entry onto the next file's first, and the chunks are decoded once at the end rather than per chunk, so a UTF-8 sequence straddling a boundary is not lost to two replacement characters.
- **Pick which rotated log file the stats panel shows (#765).** The daemon log rotates daily and the panel had no way to ask for a particular day: it read the last N lines across rotations, so a day boundary was a divider inside one continuous stream. A File control now lists the rotated files newest first and reads one of them, defaulting to the newest. Labels carry the date, `today`/`yesterday` where that is what the date means, and the file size. Sizes rather than line counts on purpose: a line count cannot be known without reading the whole file, and the panel redraws on every reindex, so labelling every file with one would read all seven each time. The list is capped at seven and says `7 of 12 files` when there is more on disk, because that cap is one the daemon's `prune` applies and not a guarantee about the directory. The days are UTC and the control says so, since `rolling::daily` rotates on the UTC date while the rows render local times.
- **Auto-refresh the log on an interval you choose (#765).** An `Auto` control offers off, 5s, 15s, 30s and 60s, replacing the `Follow` checkbox that never worked (below). A tick replaces the log lines and nothing else, so the filter box, the severity chip, the UTC and JSON toggles, all four selects and the scroll position survive it, and the view stays pinned to the tail if that is where you already were. The metric cards and the health banner are deliberately not on the timer, because moving those means rebuilding the panel and discarding everything above; the Refresh button still moves them. Off by default.
- **The `Lines` control does something.** It was a purely client-side hide over rows already in the DOM, so 500 was a hard ceiling whatever it offered. Widening the window now re-reads, narrowing keeps the fast local path, and the options run to 2000 and "All (max 5000)". The cap is real and named in the label: the log directory is pruned at 50 MB, which is fine to stream to a terminal and far too much to turn into DOM nodes.

### Changed

- **The Languages panel shows what you can act on.** The Sandbox column (`Standard`/`Elevated` enum internals) and the Package column (npm names nothing installs from any more) are gone, matching the CLI's own `lang list`, leaving Language, Semantic, This repo, Prerequisites and Action. The consent form for languages that reach the network during indexing is gone too: that approval is auto-granted for local use now, so those languages render as a plain Install. The field names an older CLI still emits are kept so its JSON continues to parse.
- **Plain words instead of internal vocabulary.** The blast-radius toggle reads Structural and Semantic rather than Tree-sitter and SCIP/LSIF (the wire values are unchanged), the Context Explorer legend matches the CLI's wording, the approval form's placeholder no longer says SCIP, and a rendered panel is asserted by test to contain no internal jargon. Language status is rendered verbatim from what the CLI reports instead of being re-derived from a `builtin` flag, so the panel and the terminal can no longer disagree about a language.
- **Em-dashes are out of the extension's user-facing strings**, in step with the workspace-wide sweep, with one deliberate exception: the `<sig> (<kind>) ... <path>` node header is a wire format that four parsers here split on, so it is restored and marked as protocol at each site, alongside the single-glyph placeholder used for an empty cell.

### Fixed

- **The panels followed the desktop appearance instead of the editor theme.** Reported from a machine with a light Windows and a dark VS Code theme: the stats panel rendered its linen palette inside a dark editor and switching themes did nothing at all. Both webviews and `media/graph.css`, which carries the largest palette in the extension, put their light palette behind a `prefers-color-scheme` media query, which inside an Electron window resolves from the OS appearance. Light OS with a dark editor is the broken pair, and it is unreachable from the editor, because the theme was never the input. All three now key on the theme kind VS Code stamps on the webview body, which is authoritative and updated live, so a theme change repaints without a reload; high-contrast light is named alongside `vscode-light`, being a light kind of its own that `vscode-light` does not match.
- **The graph webview crashed outright on ordinary repositories.** A node kind that collides with an `Object.prototype` member (`constructor`, common in Java) made the kind-to-style lookups return the inherited value instead of the fallback, so cytoscape was handed the `Object` function as a shape and threw inside the render loop, killing zoom, pan, hover and every button at once; lookups are own-property only now, and unknown or prototype-named kinds fall back safely. On a strict-CSP build, `style-src` carried no `unsafe-inline`, so cytoscape's inline styles were blocked and the renderer crashed the same way (`script-src` stays nonce-based). And node ids carrying whitespace, backticks, `#` or brackets, which synthesized external-symbol nodes routinely do, broke the cytoscape selector in both query and overview mode; ids are remapped to safe tokens with edge endpoints rewritten through the same map, while the tiles keep the real path for drill-in and `exportJson` emits real CLI node ids. The hover tooltip element that `graph.js` reads and `graph.css` styles, but that the HTML never contained, was added, so hovering no longer throws.
- **The panel rendered a stale binary's language table as confident wrong answers (#755).** With `travsr.binaryPath` empty, whatever is on PATH is resolved, and an older npm-bundled build omits `status`, `statusLine`, `repoState` and `prerequisites` from `lang list --json`. The panel read all four, so every row came out Semantic=partial, This repo=undefined and Prerequisites=`-` (which is the claim that the analyzer needs nothing), and builtins were offered an Install. Nothing said the binary was old, and nothing could: both builds self-report `1.0.0`. The gate is field presence rather than the version string, with a contract revision now stated per row so skew is detected positively; the rows are withheld behind one sentence naming the resolved binary and what it failed to report, with Download and Set-binaryPath buttons, and per-cell lookups degrade to "unknown" with a remedy tooltip. The panel's `lang list --json` call also had the 4s timeout shared with the other read-only `lang` calls, while that command sweeps PATH per catalog language and takes about 17s on Windows, so it was killed mid-flight and its partial output parsed as an empty catalog: the panel claimed no analysis tools existed on exactly the machines slow enough to need the list.
- **Installing a language through the panel granted the wrong repository.** It passed `--corpus <folder-basename>`, while the daemon keys the per-repo gate on the git-remote-derived identity, so the grant landed on a key that never matched and the repo still read as untrusted after installing from the extension. The guess is gone: the CLI runs with the workspace folder as its working directory and derives the identity itself, exactly as it does in a terminal. The install spawn timeout also rose from the shared 4s default to 120s, because an install can be downloading a binary from GitHub Releases and the old timeout could kill it mid-download, before the grant was written.
- **A failed install was reported as success.** The panel ignored the CLI's exit code, so an older binary that still has the approval gate (exit 1), or a set-up language whose project build tool is missing (exit 2), both showed as enabled. Any non-zero exit is now an error naming the real cause, and the consent flow stops instead of proceeding to re-index.
- **The tree, hover and Context Explorer views showed raw text instead of parsed results.** The em-dash sweep rewrote the `<sig> (<kind>) ... <path>` separator in three parsers and their fixtures, and that string is protocol, printed by `get_callers` and `get_context`: the regexes then matched nothing and the parsers returned zero nodes. Restored, with a note at each site so the next sweep stops at the right boundary.
- **The log reader could return nothing over a perfectly good file.** The byte ceiling and the partial-line drop did not agree about what the scan window contains: a file with one line longer than the ceiling has exactly one newline in the window, the drop consumed it, and the panel rendered "No daemon log yet" over readable entries. The other way it landed produced an 8 MB single row, embedded four times over in the HTML for roughly four times the documented budget for 5000 rows. The scan now records whether the ceiling stopped it, an absent boundary drops everything rather than being read as a cut at zero, and a window with no complete line returns one bounded, marked row naming the limit and pointing at `travsr daemon logs`, which has no ceiling.
- **`Follow` in the stats panel's log polled exactly once and then stopped (#765).** It set a three second interval inside the webview and posted a refresh on each tick, but a refresh assigns `panel.webview.html` wholesale, so the document holding the timer was replaced by the very tick that triggered it. The re-rendered checkbox carried no `checked` attribute either, so it cleared itself about three seconds after being ticked and never polled again, having read as enabled in the meantime. Replaced by the `Auto` control above, whose timer lives in the extension where a redraw cannot reach it.
- **The daemon enforced neither log-retention cap while it was running (#765).** `prune` ran at daemon start and nowhere else, on the reasoning that daily rotation left no window in which files could accumulate. The window was the daemon's own uptime: `rolling::daily` opens a file a day and deletes nothing, so a daemon left up for a month held a month of files with neither `MAX_LOG_FILES` (7) nor `LOG_BUDGET_BYTES` (50 MB) applied since boot. Both caps now re-apply when the day rolls, checked on the existing five-minute tick, and a sweep that actually drops files logs `daemon.log_pruned` so a log directory that shrank says why.

## [0.10.0] - 2026-08-16

### Added

- **Live LSP diagnostics overlay on the graph webview, plus a reworked stats panel (#688).** The daemon now holds a leased, per-session view of live diagnostics the extension reads from `vscode.languages.getDiagnostics` and publishes over MCP; `travsr daemon lsp` shows the last overlay it received. Graph nodes get a file-scoped diagnostic badge (a file no language extension has reported on is shown as not diagnosed, not clean), the detail panel lists diagnostics and marks them in the peek view, and the implementations bar was reworked alongside it: one row instead of wrapping into six, chips keep their natural width instead of collapsing to two characters, a shared file path is stated once instead of repeated per chip, and Cmd+F now toggles the node search overlay instead of only opening it. The stats panel gained a JSON log view (previously rendered blank, then unreadable token nesting), a searchable daemon-log tail with the CLI's severity colours, and a colour-coded recent-activity feed that opens the files it names.

### Fixed

- **SHA256SUMS verification rejected every real published checksum (#585).** `verifyChecksum` required an exact match on the SHA256SUMS filename field, but the release workflow generates entries against a `dist/<tarball>` path (and the Windows leg adds a `*` binary-mode marker), so no entry ever matched and the in-editor binary download failed on first run on every platform. Comparison now matches on the basename after stripping a leading `*`, and `win32/arm64` was additionally dropped from `TARGET_MAP` (no such release artifact exists), each release target now has its own regression test pinned to the real published checksum format.
- **Tree view (`get_dependencies`/`get_callers`) parsed the raw `<travsr-data>` envelope as if it were the response body (#593).** Dependency and caller counts in the sidebar tree were wrong whenever the envelope wrapper was present; the tree provider now strips it before parsing.
- Blast radius is now transitive across all languages (#613): the code lens count, the "Blast radius" panel, the hover card, and the pre-edit and rename warnings previously reported only direct importers for the languages resolved without `ResolvesTo` edges (Ruby, Go, Java, Kotlin, Scala, PHP, C#, C/C++, Swift, Dart). They now walk importers to a fixpoint, so an edit two or more hops away is reported, and Objective-C, which previously returned nothing, is supported. Counts rise accordingly and are more complete, so a file may cross the "high blast" code-lens threshold it did not before.
- Unvalidated MCP export (#498): "Travsr: Register MCP Server" now resolves and validates the binary before writing external agent configs (Claude Desktop, Cursor, Continue). The command follows the same recovery ladder as the connect path (configured value, `~/.travsr/bin`, PATH resolution, npm shim substitution) and refuses to write a config when nothing spawnable exists, instead of exporting a `.cmd` shim or a bare `travsr` that fails silently inside the external agent.
- Windows-on-ARM guaranteed-404 download (#497): removed the `aarch64-pc-windows-msvc` claim from the installer's `TARGET_MAP` (releases never ship that artifact), aligning it with the release matrix and the npm `TARGETS` map. On platforms with no published prebuilt binary the extension no longer offers the doomed "Download?" prompt; it explains that no prebuilt binary exists and links to the `travsr.binaryPath` setting instead.
- PATH auto-detect dead-end (#495): with `travsr.binaryPath` empty, a travsr install found on PATH is now resolved to its absolute location (preferring `.exe` and skipping `.cmd`/`.bat` shims on Windows), persisted to `travsr.binaryPath`, and connected immediately. Previously the PATH check returned early while the client rejected the bare `travsr` fallback, leaving the status bar permanently dead with no recovery prompt.
- Windows npm-shim dead-end (#486): a configured `travsr.binaryPath` is now validated on activation (not just checked for existence), so an invalid path, e.g. the `travsr.cmd` shim from `where travsr`, falls through to recovery instead of failing silently. When the path (or PATH lookup) resolves to an npm shim, the extension auto-adopts the packaged native binary at `node_modules/@travsr.com/travsr/bin` and persists it.
- Stale `DOWNLOAD_VERSION` (0.10.0 to 0.11.0) that made "reinstall via the extension" downgrade npm users; stable releases now CI-check the constant against the tag.

### Changed

- **Context Explorer groups results by match source (RFC-022 §14).** Exact, Semantic, and Relevant sections render in that order (score within a section), matching the backend's grouped `get_context` format now that its match-source classifier is on by default; a hoisted seed row carries no via-badge. The four separate inline copies of the `<travsr-data>` envelope stripper across `contextExplorer.ts`, `extension.ts`, and `copyContextForChat` were consolidated onto one tested `stripEnvelope`.
- **Repo Files tree and `get_repo_map` mode now rank by dependents instead of alphabetically (#632).** The sidebar file tree shows "N deps, M files" per directory using the backend's dependents-ranked rollup, and index-time test-role categorization (#479) moves test declarations out of the Context Explorer's exact/semantic groups into a capped, below-the-fold section instead of crowding out real matches.

## [0.9.0] - 2026-07-05

### Changed

- Bundled binary reference updated to travsr v0.10.0.
- Production parity with the daemon: live call graph, blast radius, callers, and the Context Explorer panel over MCP.

## [0.8.0] - 2026-06-11

### Added

- JavaScript and C# language selectors for codelens and hover providers.
- Tree-sitter vs Semantic toggle in the blast radius webview.
- Codelens and hover extended to all 13 indexed languages.

### Fixed

- Re-index triggering, Activity Bar panel refresh, and test regressions.
- Stale language list after adding or removing a language.
- Status bar `.travsr` file watcher picking up noise from terminal output.
- MCP envelope object leaking into file path lists.
- Blast radius depth slider and corpus auto-trust.
- Update bundled binary reference to v0.9.0.

## [0.7.0] - 2026-06-06

### Added

- Windows: `.exe`-only binary spawn with `assertExecutableBinary` path validation (WS1, PSE R5).

### Fixed

- `assertExecutableBinary` metacharacter regex incorrectly rejecting valid Windows paths.
- Stale download URLs and `assertExecutableBinary` not called in `reindexNow`.
- `showLanguages` using a binary path captured at activation time instead of the current resolved path.
- Update bundled binary reference to v0.8.0.

## [0.6.0] - 2026-06-04

### Added

- Visual graph UI panel (Cytoscape.js WebviewPanel) showing an interactive call graph for the active file (#245).
- File graph view with kind filter, two-hop import traversal, and stable unique file node IDs.
- Brand logo in graph panel titlebar.
- Go-to-definition support: line numbers are now stored on graph nodes and returned by `get_callers` (#249).
- Context provider (F4): push model with EventEmitter, KEYWORDS filter, and structured logging.
- Builtin semantic badge shown for all built-in-indexed languages; init button and auto-reconnect added.

## [0.4.0] - 2026-05-27

### Added

- CI publish pipeline: `vsce package` + `vsce publish` + `ovsx publish` triggered automatically on `vscode-v*` tags, with a `dry_run` dispatch option.
- VS Code test matrix expanded to 15 combinations (3 OS × 5 VS Code versions: 1.85, 1.90, 1.95, stable, insiders).
- GitHub Environment gate (`marketplace`) on the publish job - every tag-triggered publish requires manual approval before marketplace steps run.
- `.vsix` artifact uploaded to GitHub Release on every tag push.

### Fixed

- `.vscodeignore` now excludes `out/test/**` - compiled test files were being bundled into the `.vsix` (18 files / 32 KB, down from 28 files / 49 KB).

## [0.3.0] - 2026-05-26

### Added

- Auto-install: extension detects a missing `travsr` binary and offers a one-click in-editor install, downloading the correct platform binary from GitHub Releases.
- Settings UI: new `travsr.mcpPath`, `travsr.logLevel`, and `travsr.telemetry.enabled` settings configurable from VS Code Settings.
- Opt-in telemetry: anonymous usage events (activation, graph refresh, install) reported only when `travsr.telemetry.enabled` is `true` (default: `false`).
- Activity Bar SVG icon replaces the PNG placeholder.
- Actionable error notifications with "Install Travsr" and "Open Settings" quick-fix buttons when the daemon or binary is unreachable.

### Fixed

- Daemon OOM-kill regression: the daemon no longer spawns one thread per file-watcher event (was 800 MB RSS spike on 100-file floods). Single dedicated indexer worker used instead.
- Repeated `daemon start` calls no longer spawn multiple 700 MB background processes; the singleton guard now checks the control socket in the parent process before spawning.

## [0.2.0] - 2026-05-26

### Added

- Activity Bar panel (Travsr Graph) showing live Dependencies and Callers for the active file and cursor symbol, debounced on selection change.
- First-run welcome WebView panel shown on initial activation; re-openable via `Travsr: Show Welcome` command.
- `travsr.refreshGraph` command wired to the panel's refresh button.
- Cache invalidation on file save across all three providers (status bar, code lens, hover, tree).

## [0.1.1] - 2026-05-24

### Changed

- Prepared extension for VS Code Marketplace publication: added `icon.png`, `galleryBanner`, `repository`, `homepage`, `bugs`, and `keywords` fields to `package.json`.
- Added `Programming Languages` to extension categories.
- Added MIT `LICENSE` file.
- Rewrote `README.md` for marketplace audience: added install steps, supported languages table, feature descriptions, and links.

## [0.1.0] - initial

- Status bar showing live graph health (`get_repo_map` polled every 30 s).
- Blast radius code lens on `.ts`, `.tsx`, `.rs`, `.py` files (`get_blast_radius`).
- Callers and blast radius hover card on symbol hover (`get_callers` + `get_blast_radius`).
- MCP stdio transport over a single multiplexed JSON-RPC 2.0 pipe.
