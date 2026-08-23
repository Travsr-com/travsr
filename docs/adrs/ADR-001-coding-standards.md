# ADR-001: Rust Coding Standards

| Field  | Value      |
|--------|------------|
| Status | Accepted   |
| Date   | 2026-05-15 |
| Scope  | All Rust crates under `crates/` |

## Context

Travsr is a multi-crate Rust workspace built and maintained by multiple engineers and AI agents. Without uniform standards, the codebase will drift on error-handling style, logging, CLI ergonomics, and unsafe usage. We need a small, prescriptive rule set that CI can enforce mechanically.

## Decision

The following standards are mandatory for every Rust crate in the workspace.

### Language and toolchain

- **Edition:** `2021` in every `Cargo.toml`.
- **MSRV:** `1.88`, declared via `rust-version` in the workspace `Cargo.toml`, which is
  the single source of truth. `rust-toolchain.toml` pins a channel (`stable`), not a
  version, and `clippy.toml`'s `msrv` deliberately lags at `1.75`; see below.
- **Unsafe:** Every library crate must begin with `#![forbid(unsafe_code)]`. Removing it requires an RFC and Tech Lead sign-off, as stated in `CLAUDE.md`.

### Error handling

- **Application/binary crates** (`travsr-cli`, `travsr-daemon`): use `anyhow::Result` and `anyhow::Context` for ergonomic error propagation.
- **Library crates** (`travsr-core`, `travsr-indexer`, `travsr-store`, `travsr-retrieval`, `travsr-mcp`): define typed errors with `thiserror`. No `anyhow` leakage across library boundaries.
- **No `.unwrap()` in library code.** `.expect("invariant: <reason>")` is permitted only when the failure is a true invariant violation; the message must justify it. Tests may use `.unwrap()` freely.

### Logging and diagnostics

- Use the `tracing` crate everywhere. `println!` and `eprintln!` are forbidden except in `travsr-cli` for user-facing output.
- Spans must be opened at every public async entry point.
- No secrets, file contents, or full source bodies in logs above `debug` level.

### CLI

- All CLI parsing uses `clap` with the derive feature. No hand-rolled argument parsers.
- Every subcommand has a `--help` example and an exit code documented in code.

### Async

- The async runtime is `tokio` (multi-thread, current-thread for tests). No mixing of `async-std`, `smol`, or custom executors.
- Blocking work (Tree-sitter parsing, SQLite writes) must run inside `tokio::task::spawn_blocking`.

### Formatting and lints

- Formatting is enforced by the existing `rustfmt.toml` (edition 2021, `max_width = 100`, `imports_granularity = "Crate"`, `group_imports = "StdExternalCrate"`). Do not override locally.
- Linting is enforced by the existing `clippy.toml`. CI runs `cargo clippy --workspace --all-targets -- -D warnings`.
  Its `msrv` is still `1.75`, below the real `1.88`. That is the safe direction: clippy
  withholds lints newer than the value, so it never suggests an API the MSRV forbids.
  Raising it turns on new lints that `-D warnings` would fail the build on, so it is a
  code change rather than a version edit and is tracked separately.

### Documentation

- Every public item (`pub fn`, `pub struct`, `pub enum`, `pub trait`, `pub mod`) carries a one-line `///` doc comment. `#![deny(missing_docs)]` is enabled in library crates.
- Doc comments describe behavior and invariants, not implementation.

### Tests

- Unit tests live alongside source as `#[cfg(test)] mod tests { ... }` at the bottom of the file under test.
- Integration tests live in `crates/<crate>/tests/`.
- Property tests use `proptest`. Benches use `criterion`. Neither is required per-PR but both are encouraged for algorithmic code.

## Consequences

- New contributors and AI agents have an unambiguous style to follow; review time drops.
- Library crates remain embeddable (typed errors, no `anyhow` in public APIs).
- `unsafe` cannot creep in silently.
- Slight friction: removing `.unwrap()` requires writing real error types early. This is intentional.
- Doc-comment requirement adds minor overhead but is enforced by the compiler, not reviewers.

## Enforcement

| Rule                                | Mechanism                                         |
|-------------------------------------|---------------------------------------------------|
| Edition, MSRV                       | `Cargo.toml`, `rust-toolchain.toml`, CI matrix    |
| `#![forbid(unsafe_code)]`           | Compiler — build fails on violation               |
| No `.unwrap()` in lib code          | `clippy::unwrap_used` denied at lib crate root    |
| Formatting                          | `cargo fmt --check` in CI                         |
| Lints                               | `cargo clippy --workspace -- -D warnings` in CI   |
| Missing docs                        | `#![deny(missing_docs)]` per lib crate            |
| `tracing` only (no `println!`)      | `clippy::print_stdout` / `print_stderr` denied    |
| Test layout                         | Code review                                       |

Any exception requires an inline `#[allow(...)]` with a comment citing the reviewing Tech Lead and a tracking issue.
