# Changelog

## [Unreleased]

### Fixed

- PATH auto-detect dead-end (#495): with `travsr.binaryPath` empty, a travsr install found on PATH is now resolved to its absolute location (preferring `.exe` and skipping `.cmd`/`.bat` shims on Windows), persisted to `travsr.binaryPath`, and connected immediately. Previously the PATH check returned early while the client rejected the bare `travsr` fallback, leaving the status bar permanently dead with no recovery prompt.
- Windows npm-shim dead-end (#486): a configured `travsr.binaryPath` is now validated on activation (not just checked for existence), so an invalid path — e.g. the `travsr.cmd` shim from `where travsr` — falls through to recovery instead of failing silently. When the path (or PATH lookup) resolves to an npm shim, the extension auto-adopts the packaged native binary at `node_modules/@travsr.com/travsr/bin` and persists it.
- Stale `DOWNLOAD_VERSION` (0.10.0 → 0.11.0) that made "reinstall via the extension" downgrade npm users; stable releases now CI-check the constant against the tag.

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
