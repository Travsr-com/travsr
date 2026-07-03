# ADR-016: Hook Dual-Write (sh + .cmd) on Windows

**Date:** 2026-06-05
**Status:** Accepted

## Context

Travsr's `install_hook` writes a single `post-commit` POSIX shell script. This
works for Git Bash on Windows (Git for Windows invokes hooks through its bundled
sh interpreter), but fails silently for users running `git` from plain cmd.exe or
PowerShell: those environments have no `sh` on PATH and cannot execute `#!/bin/sh`
scripts, so the hook is never triggered and commits never enqueue a reindex.

## Decision

On Windows, `install_hook` dual-writes two hook files:

1. `post-commit` — existing POSIX shell script, unchanged. Continues to serve Git
   Bash users.
2. `post-commit.cmd` — new Windows Batch file with CRLF line endings. Picked up
   by git when invoked from cmd.exe or PowerShell.

Both files carry a platform-specific Travsr ownership marker so re-runs detect
ours-vs-foreign without ambiguity:

- sh marker: `# installed by travsr — do not edit this line`
- cmd marker: `@rem installed by travsr — do not edit this line`

Backup logic mirrors for each file type: a foreign `post-commit.cmd` (no Travsr
marker) is renamed to `post-commit.travsr-pre.bak.cmd` and a chain script is
written instead, so the pre-existing hook continues to run.

Linux and macOS are unaffected — the `.cmd` write is inside `#[cfg(windows)]`.

## Alternatives Considered

**Single dispatcher script** — write one `post-commit.cmd` that `call`s
`post-commit`. Rejected: requires `post-commit` to be on PATH or use relative
addressing, which is fragile in repo-relative hook contexts, and couples the two
scripts unnecessarily.

**`shell: true` spawn in the daemon** — rejected by Principal Security Engineer
(R5 in RFC-014): executing user-facing strings through a shell is a command
injection risk.

**No action / document WSL2 workaround** — rejected: forces a significant
environment dependency on Windows-native git users.

## Consequences

- cmd.exe / PowerShell git users get working post-commit hooks after `travsr init`.
- `install_hook` has a small Windows-only branch; complexity is local and tested.
- Two hook files must both be kept in sync when the hook body changes in future.
- Linux/macOS binary is byte-identical to pre-WS3 — no regression risk.
