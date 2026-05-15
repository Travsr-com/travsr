# Contributing to Travsr

## Prerequisites

- Rust 1.75 or later (`rustup install stable`)
- Git

## Build & Test

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

## Branch Naming

```
feature/<crate>-<short-description>   e.g. feature/travsr-retrieval-ppr
fix/<crate>-<short-description>       e.g. fix/travsr-indexer-tsx-parsing
rfc/<number>-<title>                  e.g. rfc/005-kuzudb-migration
```

## Pull Request Requirements

- CI must be green (fmt, clippy, tests on Linux + macOS)
- No `unwrap()` or `expect()` in library code (`crates/` except `travsr-cli`)
- One reviewer minimum (Tech Lead for core crates, peer for docs/scripts)
- Update docs if behaviour changes

## Coding Standards

- `#![forbid(unsafe_code)]` in every crate — exceptions require an RFC
- Errors: `anyhow` in CLI, `thiserror` or `anyhow` in libraries
- Logs: `tracing::info!/warn!/debug!` — never `println!` in libraries

## Reporting Bugs

Use the [bug report template](.github/ISSUE_TEMPLATE/bug_report.md).

## Security

See [SECURITY.md](SECURITY.md) — please do **not** file public issues for vulnerabilities.
