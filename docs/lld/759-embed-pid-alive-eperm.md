# 759: `embed_catalog::pid_alive` reads an EPERM sidecar as dead

## Problem

`terminate_inflight_reindex()` (daemon shutdown, and `stop-embed`) decides whether
the reindex sidecar is still running with `embed_catalog::pid_alive`. The Unix arm
shells out to `kill -0 <pid>` and maps any non-zero exit to "dead", so `EPERM`
(the process exists, this uid may not signal it) and `ESRCH` (no such process)
collapse into the same answer.

When the sidecar runs under a different uid, `!pid_alive(pid)` is true on the first
iteration of the grace poll: the daemon logs `reindex drained gracefully after
cancel`, returns, and never reaches the `kill_pid` fallback. The sidecar is
orphaned and the log positively asserts a clean shutdown.

## Root cause

Not "the Unix probe is wrong" on its own. The crate has had a correct probe since
#636 (`crates/travsr-plugin-host/src/lib.rs:114`, `unix_pid_is_alive`, `EPERM =>
true`), and this same file already routes one caller through it
(`embed_catalog.rs:493`, `pid_liveness`). The defect is an unmigrated third copy:

- `crates/travsr-plugin-host/src/embed_catalog.rs:646-654` still holds the
  `Command::new("kill").args(["-0", ..])` probe introduced by 709c035 (#437,
  2026-07-06), predating the correct helper.
- 998f46f (#636, 2026-08-14) added `unix_pid_is_alive` and migrated
  `travsr-mcp`'s copy (`crates/travsr-mcp/src/observability.rs:176`), but not this
  one, so the crate that owns the helper is the crate that still shells out.
- Nothing structural blocked reuse: `unix_pid_is_alive` is `pub`, in the same
  crate, and `embed_catalog.rs:496` already calls it. The miss was an oversight,
  and the file's two independent `cfg` dispatches (`pid_liveness` and `pid_alive`)
  are what let one be fixed while the other rotted.
- #768 (`fix/759-traceability`) touched only a doc comment on the `lib.rs` test and
  left the defect in place, so the issue number appears in the tree while the bug
  is live.

The issue's diagnosis of the symptom is accurate, including that the second
`pid_alive` guard before `kill_pid` is unreachable on that path. It is incomplete
in one respect: it presents this as a single bad function rather than as one of two
parallel platform dispatches in the same file, which is the condition that made the
divergence possible and would let it happen again.

## Options considered

1. **Fix the Unix arm in place** (`Err(EPERM) => true` around a `nix::kill` call in
   `embed_catalog.rs`). Rejected: a fourth hand-written liveness probe, and it
   leaves the two dispatches free to diverge again.
2. **Delegate the Unix arm to `crate::unix_pid_is_alive`** (the issue's suggestion).
   Correct, and safe: `pub` in the same crate, `#[cfg(unix)]` on both sides. Still
   keeps a second three-arm `cfg` dispatch whose Windows and fallback arms restate
   `pid_liveness`.
3. **Express `pid_alive` in terms of the existing `pid_liveness`** (chosen).

## Chosen design

```rust
fn pid_alive(pid: u32) -> bool {
    pid_liveness(pid) != Liveness::Dead
}
```

The `#[cfg(unix)] / #[cfg(windows)] / #[cfg(not(any(..)))]` trio is deleted. One
platform dispatch remains in the file, `pid_liveness`, already correct on all three
arms.

The grace poll's condition moves into a named `reindex_has_drained(pid)` so the
shutdown decision, not just the probe, can be asserted in a test without the test
having to signal the process it names.

Behaviour per platform:

- Unix: `unix_pid_is_alive`, so `EPERM` now reads as alive. This is the fix.
- Windows: `crate::windows_pid_is_alive`, which delegates to
  `sandbox::windows::pid_alive`, exactly what the deleted arm called. Unchanged.
- Neither: `Liveness::Unknown != Dead` is `true`, which is the conservative "report
  alive so shutdown waits the grace window and always runs the kill fallback" the
  deleted stub documented. Unchanged.

## Why this is optimal here

It fixes the reported bug and removes the structure that produced it, without adding
a line of platform code. `Liveness` already draws the distinction the shutdown path
needs ("we could not tell" must not mean "dead"), and mapping only `Dead` to `false`
states the shutdown policy in one line instead of in a doc comment on a stub.

## Test plan

In `crates/travsr-plugin-host/src/embed_catalog.rs`:

- `pid_alive_reads_an_unsignallable_live_process_as_alive` (new, `#[cfg(unix)]`):
  PID 1 must read alive. Skips when the suite runs as root, where `kill` returns
  `Ok` and the `EPERM` arm is never exercised, mirroring the skip in
  `lib.rs::unix_pid_tests`.
- `an_unsignallable_sidecar_is_not_reported_as_drained` (new, `#[cfg(unix)]`):
  asserts the grace poll's own condition, `reindex_has_drained`, is false for a
  registered sidecar whose process is alive but unsignallable. Pins the call site,
  not just the probe. It exercises `reindex_has_drained` rather than
  `terminate_inflight_reindex` because the latter would reach `kill_pid` against
  the PID under test.
- `pid_alive_reports_running_and_exited_processes` (existing, #500 regression):
  unchanged, still covers alive-self and reaped-child on every platform.
- `pid_liveness_*` (existing): unchanged, still covers the shared dispatch.

Plus `cargo test -p travsr-plugin-host`, `cargo clippy --workspace --all-targets
-- -D warnings`, and `bash .github/scripts/check-em-dash.sh`.

## Risks

- **An EPERM process now reads as alive during shutdown, so the grace poll runs.**
  Intended, and it cannot hang: the loop is bounded by `CANCEL_GRACE_SECS`
  (`Instant::now() < deadline`), not by the probe. Worst case, daemon shutdown
  against a foreign-uid sidecar now takes the full grace window instead of
  returning instantly with a false success. That is the correct trade: the instant
  return was the bug.
- **`kill_pid` now actually fires for a foreign-uid sidecar.** The `SIGTERM` will
  itself fail with `EPERM` and the sidecar survives, but the daemon then logs the
  attempt instead of claiming a graceful drain, so the orphan is visible. Making
  the kill succeed across uids is out of scope.
- **A recycled PID reads as alive**, the trade-off already documented on
  `unix_pid_is_alive`. Cost here is one bounded grace window plus a stray
  `kill_pid`, the same exposure the Windows and lock-contention callers accept.
- **Out of scope**: `crates/travsr-cli/src/main.rs:2218` (`pid_is_alive`) still
  shells out to `kill -0` and has the same EPERM defect on the `daemon stop` wait
  loop. Different crate, different call path, not covered by #759; it needs its own
  issue.
