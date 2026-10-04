# LLD 664: bring the host triple table under the release target map guard

Issue: [#664](https://github.com/Travsr-com/travsr/issues/664)

## Problem

`.github/scripts/check-target-maps.mjs` compares four copies of the published
target triple set: the `release.yml` build matrix, `TARGET_MAP` in
`packages/travsr-vscode/src/installer.ts`, `TARGETS` in
`packages/travsr-npm/scripts/ensure-binary.js` and the `TARGET_MAP` block in
`install.sh`. A fifth copy, the `(OS, ARCH) -> triple` match used to name the
current host, is outside its scope and can drift silently.

## Root cause

Line references are against `master` (c124a90).

**The issue's own diagnosis is stale.** #664 says the fifth copy is
`current_target()` in `crates/travsr-cli/src/install.rs`. It is not there any
more. `crates/travsr-cli/src/install.rs:88-96` is now a delegating wrapper:

```rust
pub fn current_target() -> Result<&'static str> {
    travsr_plugin_host::phase_b::platform::current_target().ok_or_else(|| { ... })
}
```

The triples live at `crates/travsr-plugin-host/src/phase_b/platform.rs:24-33`.
Implementing #664 literally, by adding `install.rs` to the guard's file list and
parsing `Ok("<triple>")` arms, would extract zero triples from a file that no
longer has any, and the guard would either hard-fail with "parser needs
updating" or, if written more loosely, pass while governing nothing.

**The real root cause is the guard's scoping model, not a missing file.** Its
sources are four hardcoded paths (`check-target-maps.mjs:34-37`) with a
bespoke extractor each. Nothing detects a fifth copy, and nothing noticed when
an in-scope-by-intent copy moved crates. #663 left this source out because it is
Rust match arms rather than a JS object literal, so the extractors were coupled
to file format rather than to the invariant, and the excluded copy then drifted
in location while still being excluded.

**The invariant is real and two-directional**, which is what makes the guard
worth extending rather than dropping:

- A triple `current_target()` returns that `release.yml` does not ship means
  `travsr lang install` and `travsr embed install` build an asset name for a
  platform with no published asset. That is exactly the bug recorded at
  `crates/travsr-cli/src/install.rs:98-104`: Windows was added to
  `current_target()` without a matching release leg and every language ended in
  a raw 404.
- A triple `release.yml` ships that `current_target()` cannot return means a
  user on a platform we publish a binary for hits "Unsupported platform" from
  their own installed travsr.

Both sets hold the same five triples today, so no user-visible bug exists yet.

## Options considered

**A. Add `install.rs` to the guard's file list, per the issue text.** Rejected:
the list is not there. This is the stale reading the issue invites.

**B. Add a sibling script for the Rust source.** Rejected: it would need its own
copy of the `release.yml` `artifact:` extractor and its own comparison and error
reporting, duplicating the part of `check-target-maps.mjs` that is actually
load-bearing, and would give a second CI job that fails for the same class of
mistake with different wording.

**C. Extend `check-target-maps.mjs` with a fifth extractor and a fifth
comparison (chosen).** The invariant is one invariant, so it belongs in one
place with one "these must agree" message.

**Also considered: governing `WRAPPER_RELEASE_TARGETS`** (`platform.rs:41-47`),
the sibling list holding the same five strings. Rejected, and deliberately
excluded by the region bound in the new extractor. It answers a different
question, which travsr-lang release has actually shipped a wrapper, not which
targets this repo builds; its own doc comment says so, and the CLI's
`wrapper_release_drift` test already checks it against the live release
inventory in `lang-release-drift.yml`. Comparing it to this repo's build matrix
would make a correct lag between the two release trains fail CI.
`rust_analyzer_asset` (`phase_b/catalog.rs:213-224`) was likewise left alone: it
deliberately covers a superset (`aarch64-pc-windows-msvc`) because it names a
third-party project's assets, and it is documented as such.

## Chosen design

`extractHostTargets(text)` in `check-target-maps.mjs`:

- Bounds a region from the `pub fn current_target` signature to the first line
  that is exactly `}` at column 0, which rustfmt guarantees is the function's
  own closing brace (`cargo fmt --all -- --check` gates that in CI). The bound
  is the point of the function, not incidental: `WRAPPER_RELEASE_TARGETS` sits
  eight lines below and holds the same five strings.
- Extracts `Some("<triple>")`, so the `_ => None` arm contributes nothing.
- Hard-fails with "parser needs updating" if the signature is not found, the
  brace is not found, or zero triples are extracted, matching every other
  extractor in the file.

The comparison is set equality against the release artifacts (`H` vs `R`), the
same directed-mismatch shape as the vscode and npm maps, with the two error
messages naming the user-visible consequence of each direction.

## Why this is optimal here

The guard already exists, already runs on every PR as the `target-maps` job,
and already owns this exact invariant for three other copies. Adding a fifth
extractor is a ~40 line addition that reuses the release-side extraction, the
`setDiff` comparison and the single "these five must agree" summary. Nothing
else in the repo can catch this: the Rust test at `install.rs:1137-1150`
(`current_target_returns_known_triple`) asserts the return value is in a list
written in the same crate, so it cannot see `release.yml` at all.

Retargeting to `platform.rs` also fixes the issue's premise permanently: the
guard now points at the single remaining table, and `install.rs`'s delegation
means a future move has one place to update rather than two.

## Conflict note: open PR #769

PR #769 (`fix/690-target-map-brace-scan`, issue #690) rewrites
`extractObjectValues`, adds `applyBraces`, adds a `selfTest()` with a
`--self-test` entry point immediately above the `const releaseArtifacts` line,
and adds a step to the `target-maps` job in `ci.yml`. This change deliberately
touches none of those regions: the new constant goes with the other path
constants, the new extractor goes directly after `readFileOrFail`, and the new
comparison goes in the set and diff blocks below `--self-test`'s insertion
point. `ci.yml` is not touched at all, since the `target-maps` job already runs
this script.

Follow-on once #769 merges: add `extractHostTargets` cases to its `selfTest()`
(a renamed function, a missing brace, a `WRAPPER_RELEASE_TARGETS`-shaped list
below the region bound). Doing it now would collide head-on with the block
#769 introduces.

## Test plan

The guard is green against a clean tree either way, so the test is a mutation,
run before and after the change:

1. On `master`, change `("windows", "x86_64") => Some("x86_64-pc-windows-msvc")`
   to `...-gnu` in `platform.rs`. `node .github/scripts/check-target-maps.mjs`
   exits 0 and prints "all four target maps agree". This is the gap.
2. With this change applied, the same mutation exits 1 with both directions
   reported: the extra `x86_64-pc-windows-gnu` and the now-unnamed
   `x86_64-pc-windows-msvc`.
3. Reverted, the guard exits 0 and reports 5 host triples, confirming the region
   bound excludes `WRAPPER_RELEASE_TARGETS` (10 would mean it did not).
4. Renaming `current_target` hard-fails with "parser needs updating" rather than
   passing silently.
5. `bash .github/scripts/check-em-dash.sh` stays green.

## Risks

- **The region bound depends on rustfmt output.** A closing `}` at column 0 is
  guaranteed by the `fmt` job in `ci.yml`, and the failure mode if it ever
  changes is a loud "parser needs updating", not a silent pass.
- **A conflict with #769 if that PR lands first.** Contained by construction
  (see above); the two changes are in disjoint regions of the file.
- **The "known limitation" note about naive `//` comment stripping** at
  `check-target-maps.mjs:19-21` still says "four sources". The new extractor
  strips `//` the same way and documents it on the function; the note is left
  alone only to keep this change out of the lines #769 rewrites.
- **`WRAPPER_RELEASE_TARGETS` stays ungoverned by this guard.** That is
  intentional; it is covered by `wrapper_release_drift` against the live
  inventory.
