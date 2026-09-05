# LLD 843: Phase B zero-node outcome states what was observed, not a guessed cause

## Problem

A Phase B analyzer that handshakes cleanly, invokes cleanly and returns no graph
output is reported to the user as a build problem:

> '{lang}' analysis ran but found no symbols ... it usually means the analyzer
> could not read or build this project's sources (a missing SDK or an
> unbuildable project). Fix the project setup, then re-run ...

The user is told to fix a build that is often perfectly fine, and the one piece
of evidence that would name the real cause (the analyzer's own stderr, already
captured) is emitted at `debug` and never seen.

## Root cause

Two separate defects, neither of which is "the wording is too strong".

**1. The diagnosis is made where no evidence exists.**

The stderr tail is held only by `Sidecar` in
`crates/travsr-plugin-host/src/transport.rs:470-489`, which logs it and drops
it. `LangResult` (`crates/travsr-plugin-host/src/indexer.rs:697-712`) carries no
diagnostics field, so `classify_empty_output` at `indexer.rs:1233` reduces the
whole outcome to a language name, and
`crates/travsr-daemon/src/lib.rs:2971-2972` persists it as the string
`zero_nodes:<lang>`. By the time `crates/travsr-cli/src/status.rs:269` and
`crates/travsr-mcp/src/observability.rs:359-370` render it, the only input is a
language name. With nothing to diagnose from, both consumers hardcoded the most
common guess, and the guess reads as an assertion.

The guess is also demonstrably wrong on the path it claims.
`crates/travsr-plugin-host/src/resolver.rs:389-400` already skips a language up
front when the wrapper's underlying analyzer tool is absent, with the honest
`travsr lang install` remedy. A `zero_nodes` outcome therefore only reaches the
user when the analyzer and its tool were present and ran to completion, which is
the case "a missing SDK" does not describe.

**2. Promoting the log to `warn`, on its own, changes nothing for the user.**

This is where the issue's own remedy is incomplete. `travsr init --semantic` runs
Phase B in the CLI process (`crates/travsr-daemon/src/lib.rs:1773`, inside
`init_repo_with_progress`), and the CLI's stderr subscriber defaults to `error`
(`crates/travsr-cli/src/main.rs:610-612`). A `tracing::warn!` there is filtered
out unless `RUST_LOG` is set. Only the daemon defaults to `info`
(`crates/travsr-daemon/src/lib.rs:9097-9098`), so the promotion helps the
background Phase B path and the log file, not the interactive one. The
user-facing message must therefore name the command that reveals the evidence;
the log level alone is not a fix.

## Options considered

**A. Plumb the stderr tail into `PhaseBOutcome` and persist it in meta.**
Rejected. It requires a new field on `LangResult` and edits at all eight of its
construction sites, and `phase_b_warnings` is a comma-separated
`class:lang[:extra]` string that cannot carry multi-line free-form text without a
format change that collides with #760 / PR #792, which is restructuring exactly
those classes. It would also persist unbounded analyzer output, including
absolute paths, into the store. Large blast radius for evidence that is already
recoverable in one command.

**B. Lower the CLI's default stderr filter so `warn` is visible.**
Rejected. It is a global change to every command's output for one warning, and
the stdio MCP path deliberately keeps the terminal quiet (the comment at
`crates/travsr-cli/src/main.rs:647-650` says so).

**C. State the observed fact plus the plausible causes, and name the command
that produces the evidence; promote the transport log to `warn` so that command
(and the daemon log) actually carries it.** Chosen.

## Chosen design

1. `transport.rs`: the zero-node stderr echo becomes `tracing::warn!`, matching
   the two sibling paths in the same file (handshake failure, and `mark_crashed`)
   which already warn. The daemon path then records it in the log file that
   `travsr daemon logs` reads.
2. `status.rs`: extract `zero_nodes_warning_lines(lang)` and reword. It states
   what was observed (ran, completed, returned nothing), says reinstalling will
   not help, lists the plausible causes instead of asserting one, and points at
   `RUST_LOG=travsr_plugin_host=warn travsr init --semantic --force`. The
   existing catalog-prerequisite line and the macOS Java bash hint are kept and
   move into the helper. Extraction is what makes the wording testable at all:
   the current code prints to stderr from inside `run`.
3. `observability.rs`: the same softening for the agent-facing `zero_nodes`
   detail, keeping that surface's existing terse style.

Wording stays per-surface rather than shared, matching how every other class in
these two files is written (they are kept in step by
`phase_b_warning_classes_match_the_cli`, which pins the class set, not prose).

## Why this is optimal here

It reuses conventions already in the codebase: the `no_references` arm in the
same `match` already ends with "re-run with `RUST_LOG=travsr_plugin_host=...` to
see its own diagnostics", so the zero-node arm now reads the same way; the
catalog prerequisite lookup already in place is retained; and the `warn!` level
becomes consistent with the other two stderr echoes in `transport.rs`. It touches
no protocol, no persisted format and no class set, so it cannot conflict
semantically with PR #792.

## Test plan

- `travsr-cli`: `zero_nodes_warning_lines` states the observed outcome, never
  asserts "unbuildable project" / "missing SDK" / "fix the project setup", names
  more than one candidate cause, and names the diagnostics command.
- `travsr-cli`: the catalog prerequisite line is still emitted for a language
  that has one.
- `travsr-mcp`: the decoded `zero_nodes` detail is still terminal (`failed`),
  drops the asserted remedy, and names the diagnostics command; the existing
  `phase_b_warning_classes_match_the_cli` guard keeps passing.
- Log level is not unit-observable (a macro-level choice with no pure function
  behind it) and is covered by the two sibling call sites it now matches.

## Risks

- Longer status output for a zero-node language. Accepted: the previous line was
  the same order of length and was wrong.
- Textual conflict with PR #792, which moves this `match` into
  `phase_b_warning_lines`. The conflict is one arm's body; the helper introduced
  here drops straight into it.
- `warn` level makes the daemon log noisier for a repo that legitimately has no
  in-corpus references. Bounded by `StderrRing`'s 64-line cap and emitted at most
  once per language per run.
