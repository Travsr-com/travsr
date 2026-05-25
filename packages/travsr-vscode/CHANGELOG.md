# Changelog

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
