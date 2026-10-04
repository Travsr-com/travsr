# LLD 345: run every in-process grammar in the nightly fuzz matrix

Issue: [#345](https://github.com/Travsr-com/travsr/issues/345)

## Problem

`.github/workflows/fuzz.yml` runs 7 of the 17 fuzz targets that exist on disk.
The 10 language-parser targets added during the v0.6.x multi-language work are
never executed by CI, so a Tree-sitter grammar panic in C, C++, C#, Dart, Java,
Kotlin, PHP, Ruby, Scala or Swift would reach users before it reached CI.

ADR-017 Rule 4.3 makes this a compliance gap, not just a coverage gap: a grammar
may only run in the daemon's own address space if "a `cargo-fuzz` target exists
for the grammar under `fuzz/` **and runs in the nightly fuzz workflow**"
(`docs/adrs/ADR-017-unified-plugin-sandbox-trust.md:312`). All 15 grammars in
`registry.rs` are registered in-process; 4 languages satisfy Rule 4.3 today.

## Root cause

Line references below are against `master` (c124a90). The issue reports the
surface symptom (a short matrix). There are three distinct causes underneath,
and two of them make the issue's own diagnosis wrong.

**1. Nothing ties the matrix to the target list.** Four files describe the same
set and no check compares them: `fuzz/fuzz_targets/*.rs`, the `[[bin]]` tables in
`fuzz/Cargo.toml`, `matrix.target` in `.github/workflows/fuzz.yml:26-37`, and
`FUZZ_TARGETS` in `crates/travsr-plugin-host/src/registry.rs:14-30`. Adding a
target updates the first two (the build breaks otherwise) and silently skips the
third. `check_fuzz_targets_once` (`registry.rs:45-72`) does check the first
against the fourth, but only at daemon runtime and only at `tracing::debug!`, so
its output never reaches a reviewer.

**2. The 10 targets do not fuzz a grammar at all.** Every one of them calls
`travsr_indexer::Indexer::parse_file`
(for example `fuzz/fuzz_targets/fuzz_swift_parser.rs:14` before this change).
That dispatcher (`crates/travsr-indexer/src/lib.rs:727-778`) has arms for
TypeScript, Rust, Python, Go, the data formats and Markdown only; every other
extension falls to the `_` arm at `lib.rs:766-777` and returns
`ParseOutput::default()` without touching a parser.
`crates/travsr-indexer/Cargo.toml:12-30` does not even depend on
`travsr-plugin-host`, so it cannot reach these grammars. The Java, Kotlin, Ruby,
C#, PHP, Scala, C++, C, Swift and Dart grammars are registered as in-process
plugins (`registry.rs:151-179`) and are reachable only through
`PluginIndexer::parse_file_with_vname`
(`crates/travsr-plugin-host/src/indexer.rs:301`).

So the issue's claim that "no other changes needed" is wrong. Merging the matrix
change alone would produce 10 green nightly jobs, satisfy the acceptance
criteria, and fuzz a function that returns an empty struct: a false ADR-017 Rule
4.3 signal, which is worse than the current honest gap.

**3. Objective-C has no target at all.** `objc::CONFIG` is registered in-process
at `registry.rs:175`, and `registry.rs:29` carries
`("objectivec", "fuzz_objc_parser.rs"), // TODO(#345): create this fuzz target`.
The file was never created. This is inside #345's scope by the issue's own
comment and by that TODO's issue reference, so it is created here rather than
dropped: leaving it out would leave Rule 4.3 unmet for a grammar the daemon
already parses in-process, and would leave a TODO pointing at a closed issue.

Two further details in the issue are stale: the target count is 17, not 16
(`fuzz_markdown_chunker` was added by #376), and only the 7 matrix targets have
seed corpora under `fuzz/corpus/`, not all of them.

## Options considered

**A. Add the 10 names to the matrix and stop (the issue's fix).** Rejected: root
cause 2 makes it coverage theatre. It also leaves root cause 1 in place, so the
next language repeats the bug, and leaves Objective-C non-compliant.

**B. Retarget the fuzz targets and make the CI gate a Rust test.** A
`#[test]` in `travsr-plugin-host` asserting each `FUZZ_TARGETS` entry appears in
`fuzz.yml` would run on the existing test job. Rejected: it puts CI workflow
parsing inside a shipped crate, it cannot see `fuzz/Cargo.toml` (a separate
workspace), and any edit to that crate's `src/` trips the ADR-017 Rule 5
`plugin-hashes.lock` gate, which forces an unrelated `plugin_version` bump on
every future guard tweak.

**C. Retarget the targets, create the missing one, and add a standalone guard
script (chosen).** Fixes all three causes and matches the five guards already in
`.github/scripts/`.

## Chosen design

1. `fuzz/fuzz_targets/common/mod.rs`: one shared driver that writes the fuzz
   input to `input.<ext>` and parses it through a process-wide `PluginIndexer`.
   The indexer is a `thread_local` because `PluginIndexer::new` compiles every
   registered grammar's Tree-sitter query, which costs far more than one parse.
   It lives in a subdirectory so it is not mistaken for a target source.
2. The 10 language targets become three lines each over that driver, and
   `fuzz_objc_parser.rs` joins them. `fuzz/Cargo.toml` gains the
   `travsr-plugin-host` dependency and the `fuzz_objc_parser` `[[bin]]`.
3. All 11 targets go into the `fuzz.yml` matrix, each with a seed corpus under
   `fuzz/corpus/<target>/` matching the convention of the existing 7.
4. `.github/scripts/check-fuzz-targets.mjs` asserts set equality between the
   target sources, the `[[bin]]` tables and the matrix, that each `[[bin]]`
   path matches its name, and that every in-process grammar in `FUZZ_TARGETS`
   names a target that exists. Wired into `ci.yml` as the `fuzz-targets` job.
5. The stale `// TODO: create this fuzz target` comments in `registry.rs` are
   removed and the doc comment points at the enforcing guard.

`fuzz_go_parser` and `fuzz_treesitter_indexer` are deliberately left on
`travsr_indexer::Indexer`: Go, TypeScript, Rust and Python do have arms in that
dispatcher, so those targets already fuzz real grammars through the path they
name.

## Why this is optimal here

The guard is the part that makes the fix permanent. The four lists are
structurally redundant and cannot be collapsed (cargo-fuzz requires a `[[bin]]`
per target; GitHub Actions requires a literal matrix), so the only remaining
option is to compare them in CI, which is exactly what
`check-target-maps.mjs`, `check-plugin-hashes.sh` and `check-advisory-expiry.sh`
already do for other redundant lists in this repo. The guard runs in seconds
with no toolchain, so it gates every PR rather than the nightly.

Routing through `PluginIndexer` rather than a lower-level `Dispatcher` handle
means the fuzzer exercises the same call the daemon makes, including the
extension dispatch and the `.h` content sniffing in `dispatcher.rs:112`, so a
panic found by the fuzzer is reproducible by indexing a file.

## Test plan

- `node .github/scripts/check-fuzz-targets.mjs` fails on `master` naming the 10
  unmatrixed targets and the missing `fuzz_objc_parser.rs`, and passes after the
  change (18 targets across all four sources).
- `bash .github/scripts/check-plugin-hashes.sh` passes: `plugin-hashes.lock` is
  regenerated for the comment-only `registry.rs` edit.
- `node .github/scripts/check-target-maps.mjs` and
  `bash .github/scripts/check-em-dash.sh` stay green.
- The nightly run itself is the acceptance test: 18 jobs at
  `-max_total_time=300`. A grammar that panics on its own seed corpus fails in
  the first seconds, which is the outcome this issue exists to surface.

## Risks

- **Nightly CI cost roughly triples** (7 jobs to 18, each 5 minutes plus a
  build). Accepted: the jobs are parallel, `fail-fast: false` already, and the
  budget question is explicitly deferred to the issue's stretch goals.
- **A grammar may panic on the first nightly.** That is the finding, not a
  regression; the existing "file issue on panic" step handles it, and
  `fail-fast: false` keeps the other targets reporting.
- **The shared `PluginIndexer` keeps a parse cache** across iterations. It is
  bounded at 64 MB by `MAX_CACHE_BYTES` (`crates/travsr-plugin-host/src/cache.rs:26`),
  well under libFuzzer's default RSS limit.
- **`plugin-hashes.lock` moves without a `plugin_version` bump.** The
  `registry.rs` change is comments only, so parse output is byte-identical and
  bumping the version would needlessly invalidate every user's parse cache.
- **The guard's parsers are line based**, like `check-target-maps.mjs`. Each one
  hard-fails with "parser needs updating" when it extracts nothing, so a
  restructured `fuzz.yml` or `registry.rs` breaks loudly rather than silently
  passing.
