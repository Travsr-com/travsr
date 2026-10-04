# LLD 525-3: two untrusted strings that become filesystem paths

Covers items 3 and 4 of #525. Different crates, one shared root cause.

## Problem

1. `reclaimable_hnsw_paths` (`crates/travsr-cli/src/embed.rs:2011`) builds
   `dir.join(format!("{id}.hnsw.usearch"))` from `node_embeddings.model_id` and
   hands the result to `remove_file`.
2. `EmbedOpLock::try_acquire`
   (`crates/travsr-plugin-host/src/embed_catalog.rs:407`) writes the lock's
   diagnostics sidecar with `std::fs::write`, which truncates through a
   symlink.

## Root cause

Both treat a name that came from outside the process as if the process had
chosen it. `model_id` is a column in `embed.db`, not a catalog constant, so
`gc` uses it as a filename component without ever asking whether it is one; a
value containing `..` escapes `.travsr/` and `remove_file` deletes there.
`.travsr/` is repo-local and a clone can ship anything in it, including a
symlink at `embed.lock.info`, and `std::fs::write` resolves it before
truncating, so a daemon-triggered reindex writes `<pid>\t<op>` over the
symlink's target.

The stale-info half of item 4 is already fixed: `Drop` removes the file
before releasing the lock (`embed_catalog.rs:549`, via #735), and
`try_acquire` logs recovered residue (`:396`). Only the write remains.

## Options considered

**Item 3**

1. **The issue's suggestion, `lookup_embed_backend(id).is_some()`.** Rejected.
   It answers a different question. An id that has left the catalog (a backend
   dropped in a later release) is a perfectly real model with real index files
   on disk, and a catalog check makes those permanently unreclaimable while
   `gc` still deletes their rows, which is a worse outcome than the bug. What
   makes the id dangerous is that it is not a filename, not that it is not in
   the catalog.
2. **Reject ids containing `/`, `\` or `..` by substring.** Rejected: a
   hand-rolled blocklist over a platform-specific grammar. `Path::components`
   already decides this, and gets absolute paths and Windows prefixes right.
3. **Canonicalise and check containment.** Rejected: `canonicalize` needs the
   file to exist and resolves symlinks, neither of which this needs, and it
   would change the meaning of an ordinary reclaim.

**Item 4**

1. **`OpenOptions::custom_flags(O_NOFOLLOW)`.** Works on unix, needs `libc`
   and a `#[cfg]` split with a Windows arm. This crate has no `libc`
   dependency and the fix does not need a platform split.
2. **Tempfile plus rename.** Atomic, but rename over a symlink still leaves
   the question of what the link was, and it adds a temp file to a directory
   the daemon watches for no benefit here.
3. **Refuse to write when the path is a symlink.** Rejected: it fails an
   acquire that is otherwise legitimate and leaves the repo permanently
   unusable for reindexing until someone deletes a file by hand.

## Chosen design

Item 3: `names_a_file_in_this_dir(id)` accepts only a single
`Component::Normal`, applied as a `filter` before the paths are built, so both
callers (`embed gc` and the `Reclaimable` line in `embed status`) inherit it.

Item 4: `write_info_without_following_a_symlink` unlinks the path, then
`File::create_new`. Unlinking removes the link and never its target, and
`create_new` (`O_CREAT|O_EXCL`) refuses anything that reappears at the path,
so a lost race fails the acquire rather than writing somewhere unintended.
Unlinking first is safe precisely because the exclusive lock is already held:
no other holder can legitimately own that file, which is the same argument
`Drop` already relies on.

## Why this is optimal here

Each fix is a few lines at the exact point where an outside string becomes a
path, with no new dependency, no platform split, and no change to the
behaviour either function has for ordinary inputs.

## Test plan

- `a_model_id_that_escapes_the_index_dir_reclaims_nothing`: `../outside` yields
  no deletion target and the file above the index dir survives.
- `an_ordinary_model_id_still_reclaims_its_index_files`: the real path is
  unchanged.
- `a_symlinked_info_file_is_replaced_rather_than_written_through` (unix): the
  target keeps its content, the acquire still succeeds, and the holder's own
  metadata is recorded at the path.
- `cargo test -p travsr-cli -p travsr-mcp -p travsr-plugin-host`, clippy, fmt,
  and `update-plugin-hashes.sh` for the plugin-host source change.

## Risks

- Item 3: an id that is not a plain filename is now skipped silently, so its
  index file (if one somehow exists) is not reclaimed while its rows are. That
  is strictly better than deleting an unrelated file, and no writer in this
  codebase produces such an id.
- Item 4: on Windows, `create_new` fails if another handle holds the file
  open. The lock is exclusive at that point, so this is the same contention
  the acquire already reports, and it surfaces as an error rather than as a
  silent overwrite.
- Neither changes any output a user sees for a normal repo.
