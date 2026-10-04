# 454 - repos listing cannot distinguish "never indexed" from "index deleted"

## Problem

`travsr repos` (JSON + table) and the MCP `repos_list` tool report a single
boolean derived from `graph.db_path.exists()`. A registered repo whose db file is
gone reports the same `false` / `0` whether the index was never built or was
built and later deleted. Callers (the VS Code webview, and agents calling
`repos_list` in global mode) cannot tell the two apart without inspecting the
filesystem themselves.

## Root cause

The registry persists nothing but a path. `~/.travsr/registry.json` is written by
`write_registry_atomic` as `{"repos": {<repo root>: "<db path>"}}`
(`crates/travsr-store/src/registry.rs`, pre-change `write_registry_atomic` /
`read_registry`), and `register` is the only writer
(`crates/travsr-daemon/src/lib.rs:1249`). Nothing anywhere records that an index
run ever completed:

- `crates/travsr-cli/src/repos.rs:76` - `"exists": db_path.exists()`
- `crates/travsr-mcp/src/tools.rs:3815` - `let exists = if db_path.exists() { "1" } else { "0" };`

The issue's diagnosis ("emit a clearer status string") is incomplete: no amount of
relabelling the boolean can produce a tri-state, because one `stat` of a file that
is not there is the *only* input either call site has. The two states differ only
in history, and no history is recorded. The fix has to establish that evidence
first.

A second consequence of the same gap: `register` runs at the *start* of
`init_repo`, before indexing, so even the positive case is a proxy - the db file
exists from the moment `SqliteStore::open` creates it
(`crates/travsr-daemon/src/lib.rs:1127`), not from the moment an index succeeds.

## Options considered

1. **Record the completion inside `graph.db`** (a `meta` row, e.g. `indexed_at`).
   Rejected: the evidence would live inside the very artifact whose absence we
   are trying to explain. Delete `graph.db` and the record goes with it, so
   "never indexed" and "index deleted" stay indistinguishable - the bug, reworded.
2. **A marker file next to `graph.db`** (`.travsr/indexed`). Rejected for the same
   reason: users and scripts delete `.travsr/` wholesale, and it adds a second
   file to keep in sync with the registry.
3. **Infer from the node count / db size.** Rejected: requires opening every
   repo's db from a listing command, is not durable when the db is deleted, and
   answers a different question (is the graph non-empty) than the one asked.
4. **Chosen: record the first successful index in the registry itself.** The
   registry is global, lives outside the repo, and already survives the deletion
   of `.travsr/`. It is exactly the store that outlives the artifact.

## Chosen design

- Registry values become either the legacy bare string (unchanged rows) or
  `{"db_path": "...", "indexed_at": <unix secs>}`. `read_registry` accepts both;
  `write_registry_atomic` writes back whichever shape the row's history warrants,
  so a rewrite by `prune` / `unregister` never invents history for a legacy row.
- `registry::mark_indexed(repo_root)` stamps `indexed_at` and is called from
  `init_repo_with_progress` only on the success path, under the existing
  `TRAVSR_DISABLE_REGISTRY` guard. `register` preserves any `indexed_at` already
  present on upsert.
- `RepoEntry::index_status()` is the single derivation, shared by CLI and MCP:

  | db file | recorded history | status |
  |---|---|---|
  | present | any | `indexed` |
  | absent | indexed at T | `index_missing` |
  | absent | tracked, never indexed | `not_indexed` |
  | absent | untracked (legacy row) | `unknown` |

- CLI JSON keeps `exists` (now defined as `status == indexed`) and adds `status`.
  The table's `Exists` column keeps its `yes` / `no (...)` idiom:
  `yes`, `no (index deleted)`, `no (never indexed)`, `no (unknown)`.
- MCP `repos_list` TSV appends a fourth column, `status`, and leaves the `{0|1}`
  third column untouched, so `parseReposList` in the VS Code extension
  (`packages/travsr-vscode/src/commands.ts`) keeps working against older and
  newer binaries alike. That parser now carries the status through when the
  binary sends it, and the repos webview badge names the case (`index deleted` /
  `never indexed`), falling back to the old `stale` wording when it does not.

De-duplicating the primary repo (the issue's secondary "also") is already handled
at write time by the UX-018 verbatim-prefix collapse in `register`, so it is out
of scope here.

## Why optimal here

It puts the new fact in the only place that outlives the thing whose absence is
being explained, costs one `stat` and no db opens, needs no schema migration
(the registry is JSON, already read defensively), and leaves both output shapes
backwards compatible for existing consumers.

## What pre-existing rows report and why that is honest

A row written by an older binary is a bare string with no `indexed_at`. When its
db file is gone, that row reports `unknown` - not `not_indexed`. Claiming "never
indexed" for it would be a guess: such a repo was very likely indexed at some
point and the registry simply did not record it. `unknown` is the literal truth
about what the registry knows. Legacy rows whose db file is present still report
`indexed`, matching the file-existence evidence and today's behaviour. The next
`travsr init` for such a repo rewrites its row in the new shape and stamps
`indexed_at`, so `unknown` is self-healing.

## Test plan

- `travsr-store` unit tests: a registry round-trip for each shape; `register`
  preserving `indexed_at` across an upsert; `mark_indexed` promoting a row;
  `prune` / `unregister` rewrites preserving a legacy row's string shape; and
  `index_status` over all four cases.
- `travsr-cli` integration test (`tests/repos.rs`): a crafted registry with a
  never-indexed row, an indexed-and-present row and an indexed-then-deleted row;
  assert `repos --json` statuses and the table labels.
- `travsr-mcp` unit test: `repos_list` emits the fourth column with the same
  three statuses, and keeps the `{0|1}` column.
- `travsr-daemon` unit test: a real `init_repo` leaves a row that reports
  `indexed`, and `index_missing` once its graph.db is deleted. This is the test
  that pins the stamp to the success path.
- VS Code unit test: `parseReposList` carries a fourth column through and leaves
  `status` unset for a three-column (older binary) row.

## Risks

- Two writers of `registry.json` are serialized by the existing `registry.lock`
  flock; `mark_indexed` takes the same lock, so no new race.
- An old binary reading a new-shape registry sees a non-string value and drops
  the row from its listing (its `read_registry` filters on `as_str`). Downgrades
  are the only exposure; the row is not lost from the file and a re-`init`
  restores visibility.
- Anything reading `registry.json` directly now has to accept both row shapes.
  The only such reader in the repo is the `registry_key_for` helper in
  `crates/travsr-mcp/tests/observability_e2e.rs`, updated here.
- `indexed_at` reveals nothing beyond what the registry already stores, and the
  file keeps its 0600 permissions.
