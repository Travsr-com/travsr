# LLD 575: AppContainer grant cleanup on repo deregistration

## Problem

On Windows the Phase B sandbox grants an AppContainer profile SID inheritable
allow-ACEs on the user's source tree and on the toolchain caches, and creates a
named AppContainer profile. Nothing ever removes either. After a repo is
deregistered (or travsr is deleted from disk) the grants stay in every file's
security descriptor and the profile stays in the user's registry hive.

## Root cause

The grant is written into persistent OS state under an identity travsr does not
own and cannot keep secret, and no code path ever reverses it.

- `crates/travsr-plugin-host/src/sandbox/windows.rs:24` derives the profile name
  as `travsr-{DefaultHasher(repo_root):016x}`. `DefaultHasher` is SipHash with a
  fixed zero key, so the name is a pure, reproducible function of the repo path,
  identical on every machine.
- `crates/travsr-plugin-host/src/sandbox/windows/ffi.rs:215`
  (`DeriveAppContainerSidFromAppContainerName`) turns that name into a SID with
  no privilege check and no per-user component. Any unprivileged local process
  that can guess the repo path can derive the same SID and launch an
  AppContainer that holds it.
- `ffi.rs:306` `grant_path_access` writes an `(OI)(CI)` allow ACE for that SID
  and, since the #505 fix, deliberately leaves it in place across spawns.
  `ffi.rs:439` `grant_ancestor_traverse` additionally writes `FILE_TRAVERSE`
  ACEs on every ancestor of the repo root up to the volume root.
- There is no `DeleteAppContainerProfile` and no revoke helper in `ffi.rs`, and
  no caller anywhere would invoke one: `travsr repos --remove` / `--prune`
  (`crates/travsr-cli/src/repos.rs:19`) only edit `~/.travsr/registry.json`.

### Where the issue's diagnosis is incomplete

1. **Scratch dir.** The issue lists the scratch dir as residue. It is not: the
   sandbox scratch is a per-invocation `tempfile::TempDir`
   (`crates/travsr-plugin-host/src/transport.rs:153`) deleted on drop, taking
   its security descriptor with it. Nothing to clean.
2. **Ancestor traverse ACEs.** The issue's fix shape covers "repo root, scratch
   dir, toolchain cache paths" and omits `grant_ancestor_traverse`, which is the
   residue landing *outside* the repo, on `C:\Users\<user>`, `C:\Users`, `C:\`.
   Cleanup that skipped these would leave the most visible residue behind.
3. **Profile-name spelling.** The registry key is verbatim-stripped at write
   time (`crates/travsr-store/src/registry.rs:169`) while the `repo_root` the
   sandbox hashes carries the `\\?\` prefix on Windows (documented at
   `sandbox/toolchain.rs:51`). `Path`'s hash of `\\?\C:\r` and of `C:\r` differ,
   so cleanup driven naively off the registry key derives the *wrong* profile
   name and silently revokes nothing.

## Options considered

**A. Revoke at daemon stop / sidecar exit.** Rejected: removing an inheritable
ACE triggers a full inheritance propagation walk over the tree, the exact cost
#505 removed, and it would re-churn the tree on every start/stop cycle.

**B. Add a `travsr uninstall` command and clean up from it.** Rejected as the
*primary* trigger: no uninstall command exists anywhere (not in `travsr-cli`,
not in `install.sh`, not in `packages/travsr-npm`), and a real one must also
reverse `~/.travsr/{bin,share,models,lib,kls}`, the git post-commit hook
(`hook.rs:185` has no inverse), the Windows autostart task, and `install.sh`'s
`~/.local/bin/travsr`. That is a separate feature; bolting ACL cleanup onto a
command that does not exist would ship the code with no caller.

**C. (chosen) Clean up from the existing explicit deregistration path.**
`travsr repos --remove` and `travsr repos --prune` are the only user-initiated
"this repo is done" events, they are explicit and opt-in (which the propagation
walk requires), and they already know the repo root. The cleanup entry point is
public on `travsr-plugin-host`, so a future uninstall command reuses it by
looping over `registry::all_repos()`.

## Chosen design

- `crates/travsr-plugin-host/src/sandbox/cleanup.rs`: cross-platform, no
  `unsafe`, no FFI. Holds `profile_name` (moved out of `windows.rs`, which now
  reuses it) and the pure planner `plan_cleanup(removed_repo, remaining_repos,
  toolchain_paths, travsr_bin)` returning `CleanupPlan { profiles, paths }`:
  - `profiles`: the profile names for *both* spellings of the repo root (the
    registry key and its `\\?\` verbatim form), minus any name a still-registered
    repo also maps to.
  - `paths`: repo root, every ancestor of it, the union of toolchain
    read/write/exec paths over `PHASE_B_CATALOG`, and `~/.travsr/bin`; deduped,
    order preserved.
- `crates/travsr-plugin-host/src/sandbox/windows/ffi.rs`: two new safe wrappers
  (the only new `unsafe`, staying inside this one file per ADR-017 A2).
  `revoke_path_access(path, sid)` uses `SetEntriesInAclW` with `REVOKE_ACCESS`,
  guarded by a DACL pre-check so a path carrying no ACE for the SID skips the
  subtree rewrite; `delete_appcontainer_profile(name)` treats "not found" as
  success.
- `purge_repo_sandbox_grants` is the caller-facing entry point: a no-op returning
  an empty report off Windows, so `repos.rs` needs no `cfg`.
- `UnregisterResult::Removed` carries the registry key it removed, so `repos.rs`
  can hand the repo root to the cleanup without re-implementing the name
  resolution inside `unregister_resolving`.
- Every step is best-effort and idempotent: a missing path, an already-deleted
  profile, or an ACL the user cannot rewrite is counted in the report and never
  aborts the deregistration.

## Why optimal here

It reverses exactly what the grant path creates, at the one moment the user has
said the repo is finished, without adding a second `unsafe` site (ADR-017 A2
invariant 1) and without a periodic cost. All decision logic (which profiles,
which paths, whether the profile may be deleted) is a pure function testable on
any OS; only the two Win32 calls are `cfg(windows)`.

## Security analysis

The residue is not disk clutter, it is a live grant. The ACE names an
AppContainer SID derived by public, unkeyed hash from the profile name, which is
itself an unkeyed hash of the repo path. AppContainer SIDs are not scoped to a
user account and creating an AppContainer needs no privilege, so **any local
process, including one running as a different unprivileged user, can assume that
SID** by creating an AppContainer with the same name and inherit whatever the
leftover ACE grants:

- repo root: `GENERIC_READ` over the whole source tree, or `GENERIC_ALL` for
  Scala repos where `needs_repo_write` is true, so read of private source and,
  for Scala, write access that survives travsr's removal.
- toolchain write caches (`~/.gradle`, `~/.m2`, `~/.nuget`, `GOMODCACHE`):
  `GENERIC_ALL`, a plant-a-dependency primitive.
- `~/.travsr/bin`: read and execute, on a directory whose #507 hardening
  otherwise restricts it to the owner.
- ancestors: `FILE_TRAVERSE` only, which is pass-through and not list-contents,
  so it discloses nothing on its own.

Revoking uses `REVOKE_ACCESS` for that SID alone, so no other principal's ACEs
are touched, and it is skipped entirely when a still-registered repo maps to the
same profile name.

## Test plan

Cross-platform unit tests in `cleanup.rs`, running on macOS and in CI on Linux
and macOS:
- the plan covers the repo root and every ancestor;
- the plan carries both the clean and the `\\?\` profile-name spellings, and
  those two names actually differ (pins the trap in Root cause item 3);
- a profile still mapped by a remaining registered repo is excluded, making the
  plan a no-op;
- paths are deduplicated, and `~/.travsr/bin` plus the toolchain paths are
  present;
- `profile_name` is stable and is the same derivation the spawn path uses.

Windows-only, to be verified by a reviewer on real Windows:
`revoke_path_access` round-trips against `grant_path_access` (grant, assert ACE
present, revoke, assert absent), a revoke with no matching ACE is a cheap no-op,
and `delete_appcontainer_profile` succeeds and is idempotent.

## Risks

- **Not executable on non-Windows CI.** The FFI half compiles only for
  `x86_64-pc-windows-msvc` and cannot run here; a Windows reviewer must confirm
  the ACE round-trip.
- **Partial cleanup.** A plugin binary resolved from outside `~/.travsr/bin`
  (for example a project-local `node_modules/.bin`) received a read+execute grant
  on its own directory that this plan does not enumerate, because the resolved
  program path is not recorded anywhere at deregistration time.
- **One-time propagation walk.** The revoke on the repo root rewrites the subtree
  once, the same cost profile as the original grant, and only on explicit
  deregistration.
- **Toolchain discovery cost.** The languages that actually ran are not
  recorded, so `toolchain_grant_paths` walks the whole Phase B catalog and some
  arms shell out (`go env`, `java -XshowSettings`, the dotnet probes). That is a
  one-shot cost on an explicit deregistration, not on any hot path.
- **Pre-existing residue** is cleaned only for repos still in the registry when
  the user deregisters them. A repo already dropped from the registry keeps its
  ACEs, since the repo path is the only way to name the SID.
