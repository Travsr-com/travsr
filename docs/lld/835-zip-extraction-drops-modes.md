# LLD 835: zip extraction drops unix modes, so the KLS launcher is not executable

## Problem

`travsr lang install kotlin` unpacks kotlin-language-server's `server.zip` into
`~/.travsr/kls/` and writes a `#!/bin/sh` wrapper into `~/.travsr/bin/` that
`exec`s `~/.travsr/kls/server/bin/kotlin-language-server`. The wrapper is
executable; the launcher it targets lands as `-rw-r--r--`, so the `exec` fails
with EACCES. The sidecar reads a zero-length LSP response and the user is told
"semantic analyzer ran but produced no symbols for: kotlin", with nothing
pointing at permissions. Every fresh Kotlin install is affected.

## Root cause

Not "the install step forgets to chmod the launcher". The zip extractor throws
away every entry's recorded mode:

- `crates/travsr-cli/src/install.rs` `extract_zip` writes each entry with
  `std::fs::File::create`, which creates at `0o666 & !umask` (0644 in practice),
  and never reads `file.unix_mode()`.

This is a regression from commit `0097e78` ("safe archive extraction, and verify
every downloaded tool (#410 M1, M2a, M2b)", PR #642), which replaced the
`unzip -qo` and `tar -xzf` subprocesses with in-process extractors so that
traversal and size limits were owned by this repo rather than inherited from
whichever `unzip`/`tar` the host shipped. `unzip` applies each entry's mode; the
replacement did not, and the mode loss was not covered by the tests added with
it.

The issue's own diagnosis stops at the symptom. Chmod-ing the KLS launcher at
install time would fix Kotlin and leave the defect in place for every other
asset the zip path unpacks, which is where the fix belongs.

The tar path does **not** have the same defect: `extract_tar_gz` unpacks through
`tar::Entry::unpack`, which applies the header mode (masked to `0o777`, since
`preserve_permissions` defaults to false). It is only the zip path that lost
modes.

## Options considered

1. **chmod the KLS launcher in `cmd_lang_install`** (next to the existing
   wrapper chmod at `crates/travsr-cli/src/lang.rs:1472-1476`). Rejected: fixes
   one asset, leaves `extract_zip` lossy for every future one, and encodes
   knowledge of an archive's internal layout in the install command.
2. **Apply `file.unix_mode()` verbatim.** Rejected: honors setuid/setgid/sticky
   and group- or world-writable bits from a downloaded archive. The in-process
   extractor exists precisely so archive-declared values are validated here.
3. **Apply a masked mode in both extractors** (chosen).

## Chosen design

One constant states the invariant, and both extractors enforce it:

```rust
const EXTRACTED_MODE_MASK: u32 = 0o755;
```

- zip: after writing an entry, `apply_entry_mode(&target, file.unix_mode())`
  sets `mode & EXTRACTED_MODE_MASK`. It is a no-op on non-unix.
- tar: `archive.set_mask(0o777 & !EXTRACTED_MODE_MASK)` so the mode `unpack`
  already applies is clipped to the same bits.
- zip entries are now removed before `File::create`, since an archive may
  declare a read-only mode and a reinstall must still overwrite. This is what
  `tar::Entry::unpack` already does.

Directory entries keep their default mode; a restrictive mode taken from the
archive would break extraction of the entries beneath it.

## Why this is optimal here

The fix lives at the layer that lost the information, so every zip asset (KLS
today, anything added later) is covered without the install command knowing what
is inside an archive. It restores parity with the `unzip` behaviour that #642
replaced while keeping that change's premise: what an archive declares is
validated by this repo, not trusted.

## Security analysis of restoring modes

Restoring modes reintroduces attacker-controlled bits, so the mask is the
control:

- **setuid/setgid/sticky** are outside `0o755` and are dropped. An archive
  cannot produce a setuid file under `~/.travsr/`, which for a user-owned
  install directory would be a local privilege escalation primitive.
- **group- and world-writable** are dropped. `~/.travsr/bin` holds tools travsr
  later spawns; a `0o777` entry would let any other local account replace one.
  This is stricter than `unzip`, which applies the mode subject only to umask.
- **Owner bits pass through**, so an archive can make its own launcher
  executable. This is the intended capability and the fix for #835.
- **Executable content is not a new capability.** Everything unpacked here is
  already invoked by travsr, and the archive is fetched over HTTPS from a pinned
  release tag and checked against a vendored sha256 in `verify_and_extract_zip`
  *before* a byte is extracted. The mask bounds what a compromised-but-verified
  asset could do, not whether its code runs.
- Path traversal, link entries and the size cap are untouched; this change adds
  no new write target.

## Test plan

Unix-gated tests in `install.rs::extraction_tests`, written before the fix and
confirmed failing with mode `0o644`:

- `an_executable_zip_entry_stays_executable` — a `0o755` entry extracts `0o755`.
- `setuid_and_setgid_bits_in_a_zip_entry_are_dropped` — a `0o6755` entry
  extracts `0o755` with no bits above `0o777`.
- `group_and_world_write_bits_in_a_zip_entry_are_dropped` — `0o777` becomes
  `0o755`.
- `a_read_only_zip_entry_extracts_twice` — a `0o444` entry does not break a
  reinstall.
- `an_executable_tar_entry_stays_executable` — pins the tar path's behaviour now
  that the zip path is expected to match it.

The setuid case cannot be built with `SimpleFileOptions::unix_permissions`
(which masks to `0o777`), so the test helper writes the mode into the central
directory's external-attributes field directly.

Plus `cargo test -p travsr-cli`, `cargo clippy --all-targets -- -D warnings`,
`cargo fmt --all -- --check`.

## Risks

- An asset that genuinely relied on a group- or world-writable extracted file
  would now get `0o755`. No asset in the catalog does.
- Modes are not applied on Windows; unchanged behaviour there, where the wrapper
  is a `.cmd` and the execute bit has no meaning.
- Users who already installed Kotlin keep the 0644 launcher until they reinstall
  (`travsr lang install kotlin`); a manual `chmod +x` also works.

## Out of scope

Bug 2 of issue #835, `travsr-lang-java` invoking `scip-java` without
`--build-tool`, lives in the sibling `Travsr-com/travsr-lang` repo. No part of
it is fixable here, and it is not addressed by this change.
