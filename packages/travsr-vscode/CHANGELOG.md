# Changelog

## [0.3.0] — 2026-05-26

### Added

- Auto-install: extension detects a missing `travsr` binary and offers a one-click in-editor install, downloading the correct platform binary from GitHub Releases.
- Settings UI: new `travsr.mcpPath`, `travsr.logLevel`, and `travsr.telemetry.enabled` settings configurable from VS Code Settings.
- Opt-in telemetry: anonymous usage events (activation, graph refresh, install) reported only when `travsr.telemetry.enabled` is `true` (default: `false`).
- Activity Bar SVG icon replaces the PNG placeholder.
- Actionable error notifications with "Install Travsr" and "Open Settings" quick-fix buttons when the daemon or binary is unreachable.

### Fixed

- Daemon OOM-kill regression: the daemon no longer spawns one thread per file-watcher event (was 800 MB RSS spike on 100-file floods). Single dedicated indexer worker used instead.
- Repeated `daemon start` calls no longer spawn multiple 700 MB background processes; the singleton guard now checks the control socket in the parent process before spawning.

## [0.2.0] — 2026-05-26

### Added

- Activity Bar panel (Travsr Graph) showing live Dependencies and Callers for the active file and cursor symbol, debounced on selection change.
- First-run welcome WebView panel shown on initial activation; re-openable via `Travsr: Show Welcome` command.
- `travsr.refreshGraph` command wired to the panel's refresh button.
- Cache invalidation on file save across all three providers (status bar, code lens, hover, tree).

## [0.1.1] — 2026-05-24

### Changed

- Prepared extension for VS Code Marketplace publication: added `icon.png`, `galleryBanner`, `repository`, `homepage`, `bugs`, and `keywords` fields to `package.json`.
- Added `Programming Languages` to extension categories.
- Added MIT `LICENSE` file.
- Rewrote `README.md` for marketplace audience: added install steps, supported languages table, feature descriptions, and links.

## [0.1.0] — initial

- Status bar showing live graph health (`get_repo_map` polled every 30 s).
- Blast radius code lens on `.ts`, `.tsx`, `.rs`, `.py` files (`get_blast_radius`).
- Callers and blast radius hover card on symbol hover (`get_callers` + `get_blast_radius`).
- MCP stdio transport over a single multiplexed JSON-RPC 2.0 pipe.
