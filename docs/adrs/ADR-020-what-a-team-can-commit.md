# ADR-020: What a Team Can Commit From `.travsr/`

**Date:** 2026-11-02
**Status:** Accepted
**Issue:** #1000
**Phase:** N/A (repository hygiene contract)
**Author:** Solution Architect
**Related:** #893 (`.travsr/` ignored by `travsr init`), RFC-002 (signature format), RFC-026 (project-scoped MCP config trust), #747 (repo root resolution)

---

## Context

`travsr init` used to write one rule to `.gitignore`, `/.travsr/`, which ignores the
whole directory. Teams asked whether any of it can be shared, and two questions kept
coming back: "can we commit `graph.db` so a clone starts with a map?" and "can we commit
our settings?". The blanket rule answered both with "no" by accident. This ADR answers
them on purpose, file by file.

### Inventory

| File | Shareable | Ignored by the managed block |
|------|-----------|------------------------------|
| `graph.db` | Portable, but not live-shareable (see the clone experiment) | yes |
| `graph.db-wal`, `graph.db-shm` | No, runtime state | yes |
| `config.toml` | Yes. Team settings (`docs.*`, `guard.mode`). Per-machine settings (`embed.*`, `log.level`) belong in `~/.travsr/config.toml` | **no** |
| `init.lock`, `daemon.lock`, `daemon-restart.lock`, `daemon-start.err`, `daemon-<hex>.sock`, `daemon.log.YYYY-MM-DD` | No | yes |
| `embed.db` (+ wal/shm), `<model>.hnsw.usearch`, `<model>-docs.hnsw.usearch`, `embed.toml`, `embed.lock`, `embed.lock.info`, `embed.cancel` | No. Row ids are coupled to `graph.db` and the backend is machine specific | yes |
| `guard-sessions.json` (+ `.json.tmp.<pid>`) | No | yes |
| `.travsrignore` (repo root) | Yes, and a team committing a map should commit it. `travsr init` creates it if missing, and its defaults (`vendor/`, `build/`, `**/generated/`, `**/testdata/`, ...) can change which files get indexed | n/a, never ignored |
| `/.mcp.json` and other AI tool configs (repo root) | No, see below | separate `connect` block, unchanged |

## Decision

1. **Allow-list, not deny-list.** `travsr init` manages a delimited block in
   `.gitignore`:

   ```
   # travsr-local:begin (travsr runtime files, managed by `travsr init`)
   /.travsr/*
   !/.travsr/config.toml
   # travsr-local:end
   ```

   Everything under `.travsr/` is ignored except `config.toml`. A deny-list would have to
   name every runtime file, and the directory keeps growing (embed sidecars, guard
   sessions, daemon logs). New runtime files are safe by default under an allow-list.
   The pattern is `/.travsr/*` (contents) and not `/.travsr/` because git never descends
   into an ignored directory, so the `!` rule could not re-include `config.toml`.

2. **`graph.db` stays ignored by default.** Live sharing is not supported (clone
   experiment below). Teams that want it anyway can opt in with
   `git add -f .travsr/graph.db`, with the tradeoffs listed below.

3. **`/.mcp.json` handling is unchanged.** It is documented here, not altered.

4. **Managed block rules.** One balanced block is rewritten only if its body differs.
   Duplicated, unbalanced or out-of-order markers leave the file untouched and log a
   warning. The exact legacy pair written by older versions (the comment line followed by
   `/.travsr/`) is replaced in place. Any other user rule, including a bare `/.travsr/`
   or `.travsr/` without the legacy comment, is never edited; if the user's rules already
   ignore the runtime files, no block is appended. User lines are preserved byte for byte,
   and CRLF files stay CRLF. The markers deliberately avoid the substrings
   `# travsr:begin` and `# travsr:end`, which the `connect` block counts.

## The clone experiment

A repository with `graph.db` committed was cloned. The committed database answers queries
before any `travsr init` runs, so the artifact is portable. The first `travsr init` on the
clone then does real work in some cases (see the rebuild triggers), so a clone does not
always start free.

The problem is the next step. `graph.db` is a SQLite file in WAL mode, and `travsr init`
alone (like any reindex or WAL write, including a plain query) leaves it modified in
`git status`. We saw ` M .travsr/graph.db` right after `init` on a fresh clone, with no
commit hook or watcher involved. After upstream commits a refreshed map, `git pull` aborts
with "Your local changes ... would be overwritten". This is similar to the deadlock #893
fixed for `git revert` and `git merge`. A map committed by one person is therefore a
conflict generator for everyone else, and binary SQLite files cannot be merged.

## Rebuild triggers for a cloned map

A cloned `graph.db` is not always reused. Work happens when:

- the corpus differs (the normalized origin URL; with no remote, the directory name);
- the binaries disagree on signature format (RFC-002);
- `core.autocrlf` changes file hashes;
- Phase B, which is keyed to the commit hash, re-runs because committing the map moves
  `HEAD`. Verified: after committing the map, the stored `phase_b_commit` and
  `last_commit` were the parent commit while `HEAD` was the map commit; the first `init`
  on the clone re-ran Phase B and set both to `HEAD`, and a second `init` did not re-run
  it (`files_indexed 0`). One clone in an earlier experiment reported `files_indexed 0`
  and `phase_b` partial on the first `init`, and its cause was not isolated;
- `.travsrignore` differs. `travsr init` creates the file when it is missing, with
  defaults that exclude `vendor/`, `build/`, `**/generated/`, `**/testdata/` and others.
  Verified: a source repo with a custom `.travsrignore` that kept `testdata/t.ts` indexed
  (4 nodes) committed its map without that file. After the clone, `init` generated the
  default file (it appeared untracked), dropped the `testdata` file and finished with 2
  nodes. This is the likely explanation for the earlier 8 to 7 node drop with an untracked
  `.travsrignore`, though that exact repo was not re-run. Commit `.travsrignore` next to
  the map; with it committed the node count matched the source;
- embeddings are recomputed locally, since `embed.db` is never shared.

`meta.repo_root` stores an absolute path but is harmless: `resolve_repo_root` prefers the
location of the database (#747).

## Why `/.mcp.json` is not committed by default

A project-scoped MCP config is executed by the AI tool on clone, which is the RCE surface
RFC-026 closes by keeping it opt-in. It may also contain an absolute path to the travsr
binary (`McpCommand::resolve`, `connect.rs`) that is wrong on every other machine.
Committing it is available with `travsr connect --commit`.

## Opting in to a committed `graph.db`

```
git add -f .travsr/graph.db
```

Tradeoffs the team accepts: a binary blob in history that grows with every committed
refresh; a permanently dirty tree after any reindex until the file is restored or
re-committed; `git pull`, `git merge` and `git revert` aborting when the incoming commit
touches the file; and the rebuild triggers above. Commit `.travsrignore` with it so the
clone indexes the same files. Undo the opt-in with `git rm --cached -- .travsr/graph.db`.

## Consequences

- Teams can commit `.travsr/config.toml` with no manual `.gitignore` edit.
- Existing repos with the legacy pair migrate to the block on the next `travsr init`.
- Repos that already track runtime files under `.travsr/` (WAL/SHM, locks, sockets, logs,
  embed files, guard sessions) get a warning naming those files and a
  `git rm --cached -- <files>` command. A tracked `graph.db` alone is treated as the
  documented opt-in and gets a one-line note instead (the committed map shows as modified
  after each reindex, pointing at the FAQ entry), with no untrack command. When `graph.db` is tracked
  together with accidental files, the warning names the accidental files only.
  `travsr init` never edits the index.
- Sharing a frozen, read-only snapshot of the map (for example a release artifact that is
  not rewritten by reindexing) would avoid the pull deadlock but needs its own RFC.
