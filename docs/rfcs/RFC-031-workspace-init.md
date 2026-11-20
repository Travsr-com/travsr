# RFC-031: `travsr init` in a plain folder, and one map for a folder of repos

**Date:** 2026-11-02
**Status:** Draft
**Issue:** #1002
**Related:** ARCH-102 (corpus naming), RFC-026 (AI tool auto-configuration),
#381 (multi-repo-per-tenant serve layout, closed), #670 (cross-repo queries)

## Context

Users trying Travsr after a demo clone all their repos into one folder and run
`travsr init` at the top, expecting one map that covers every repo behind one MCP
server. Today that fails before anything is indexed:

```
not inside a git repository; run `git init` first, then `travsr init`
```

The error comes from one place, `resolve_repo_root` in
`crates/travsr-cli/src/repo.rs`, which every write command (`init`, `fsck`,
`embed`, `daemon`, `hook-run`) goes through. The `git init` workaround is worse
than the error: it makes the parent folder a repo, so one walk indexes every
child into the parent's corpus. Measured on v1.4.0 with a folder `ws/` holding
two git repos, `alpha` (remote `github.com/acme/alpha`) and `beta`:

| | corpus | path of `alpha`'s file |
| --- | --- | --- |
| `travsr init` inside `ws/alpha` | `github.com/acme/alpha` | `main.ts` |
| `git init` in `ws/`, then `travsr init` there | `local/ws` | `alpha/main.ts` |

The same file gets two different VNames depending on where init ran. See "One
map is a federation, not a merge" for why that rules out a merged map.

#1002 asks for three things:

1. `travsr init` in a folder with no VCS, kept fresh without git hooks.
2. No hard assumption of git for change detection.
3. A workspace: one init at a parent folder that finds the child repos, indexes
   them, and serves them through one MCP server.

Most of (3) already exists and is only unreachable:

- `travsr mcp --global` serves every repo in `~/.travsr/registry.json` through
  one stdio server. 11 query tools take an optional `repo` argument and fan out
  across all repos when it is omitted (`collect_global` in
  `crates/travsr-mcp/src/tools.rs`), and `repos_list` lets the agent discover
  names.
- The registry already maps each repo root to its own `graph.db`, and
  `registered_ancestor` already resolves a path inside a registered root without
  a `.git`.

What is missing is the entry point (one init that finds the children), a scope
(`--global` serves the whole machine's registry, not one folder), and the no-git
freshness path.

## Decision

### D1. Init without git ("plain mode")

`resolve_repo_root` falls back to the starting directory when no `.git` is found
**and the command is `init`**. Every other command keeps today's resolution,
which already finds a plain-mode root through the registry once init has
registered it.

Guards, because a root with no VCS has no natural boundary:

- Refuse `$HOME` and the filesystem root, with a message naming the folder to
  run in instead. Indexing a home directory is never what the user meant.
- Apply the same refusal when the `.git` that detection finds is at `$HOME`.
  A home directory tracked as a dotfiles repo is common, and without this
  check `travsr init` in `~/code` resolves to `~` and indexes all of it, which
  is the exact case the first guard exists to stop.
- Print one line saying the folder is in plain mode and how it stays fresh
  (below), so a user who expected git hooks is not misled.

The mode is recorded in the index metadata (`vcs = none`) at init, and the
daemon reads that rather than probing for `.git` at each start. A folder that
later gains a `.git` keeps plain-mode freshness until `travsr init` is run
again, instead of silently switching to a hook path that has no hooks
installed.

Plain mode changes nothing inside a git repo: detection still finds `.git` first.

### D2. Freshness without hooks

A git repo is kept fresh by the post-commit hook and the file watcher, and
caught up after downtime by `reconcile_head_drift`, which compares a stored
`last_commit` against `HEAD`. A plain folder has no commit to compare, so the
catch-up step needs a different trigger and a different file list. Everything
else is reused:

| Concern | Git repo (unchanged) | Plain mode |
| --- | --- | --- |
| Edits while the daemon runs | file watcher | file watcher (same) |
| Catch-up after downtime | `reconcile_head_drift` on `HEAD` mismatch | full-tree reconcile at daemon start |
| File list for the reconcile | `git ls-files` | the init walk (`WalkBuilder`) |
| Cost of an unchanged file | one hash (`reindex_files` is hash-delta gated) | one hash (same) |
| Deletions | prune `db_paths - tracked` | prune `db_paths - walked` (same breaker) |
| Manual refresh | commit, or `travsr init` | `travsr init` |

Concretely, `reconcile_tracked_tree` takes its file list from the caller instead
of calling `tracked_files_from_git` itself, and plain mode passes the walk. The
prune keeps the same mass-delete circuit breaker `fsck --fix` uses.

The reconcile alone is not the whole catch-up. After it, `reconcile_head_drift`
also hands the dirty callers to `enqueue_dirty_callers` and arms the Phase B
rebuild with `phase_b_scheduler.mark_dirty()`. The plain-mode startup trigger
does the same two steps, or the call edges stay stale after a restart even
though every file was rehashed.

**A revision marker stands in for the commit.** `last_commit` is more than a
display value. `run_background_phase_b_inner` returns immediately when it is
empty, the query cache keys on it, and `travsr status` and the MCP freshness
fields read it. A plain folder has no commit, so leaving it empty would leave
the semantic layer permanently stale after every restart. Plain mode stores a
`plain-<n>` marker there instead (`n` is a strictly increasing time-based
number). It is minted when a pass changes the tree and reused when it does not,
so a re-run over an unchanged folder finds Phase B already done instead of
repeating whole-project analysis. Nothing resolves it with git: every reader
that asks git about `last_commit` already treats a revision git cannot resolve
as "unknown", and a git repo's real commit replaces the marker the next time
`travsr init` runs there.

The startup reconcile also rebuilds when the watcher left the semantic layer
flagged degraded (`phase_b_dirty`), not only when file hashes differ. A file
edited while the daemon ran was reindexed by the watcher, so its hash matches
at the next start, but its call edges were dropped; without this a restart
would never repair them. An unchanged tree arms nothing.

**Dropped watcher events.** The daemon's event queues are bounded and drop
events when full (#736). In a git repo that loss is repaired by the periodic
HEAD reconcile and the next commit. Plain mode has neither, so a dropped event
would stay unrepaired until the next daemon restart. When the watcher has
dropped events since the last reconcile, plain mode runs the same full-tree
reconcile on the next GC tick.

**Semantic layer after live edits.** Phase B is gated on `last_commit` differing
from `phase_b_commit`, and a plain folder has no commit to move it. A live edit
sets `phase_b_dirty` with the two markers equal, so nothing would rerun Phase B
until a restart. On each GC tick (every five minutes, not per edit, to keep the
#583 rule that typing does not trigger whole-project analysis) a plain index with
`phase_b_dirty=1` and equal markers mints a new `plain-<n>` revision and arms
the debounced Phase B run.

**Text search.** `find_pattern` runs `git grep --no-index --exclude-standard`
over the working tree in plain mode, with the hard skip directories excluded by
pathspec. It honours the folder's `.gitignore` but not `.travsrignore`.

**Index status fields.** `get_index_status` reports `staleness.is_stale` and
`head_commit` as null in plain mode, since there is no HEAD to compare. After a
later `git init` with no commit yet, `last_commit` keeps its `plain-<n>` marker
until the first commit.

No git hooks are installed in plain mode, and `status` reports the freshness
source as `watcher` rather than `post-commit hook`.

**Ignore rules need one explicit change.** The init walk builds with
`.git_ignore(true)`, and the `ignore` crate applies `.gitignore` files only
inside a git repository unless `require_git(false)` is set. Without it, a plain
folder would index `node_modules/` and every other path its `.gitignore`
excludes. Plain mode sets `require_git(false)` on the `WalkBuilder`s that
enumerate files (the init walk and the reconcile walk).

`require_git(false)` on its own goes too far: with the crate's default
`parents(true)`, it also applies `.gitignore` files in folders *above* the
root. A stray `~/.gitignore`, or one in the workspace folder, would then
silently drop files the plain folder's own rules never mention. The watcher's
`build_ignore_matcher` only reads ignore files under the root, so the walk and
the watcher would also disagree about the same path. Plain mode therefore sets
`parents(false)` on those walkers too, which makes the walk match the watcher
and match what git would do inside a repo rooted at the same folder. Both
settings apply in plain mode only; walkers inside a git repo are unchanged.

### D3. Other VCSs

Plain mode is the answer for any non-git VCS. Mercurial, Subversion, Fossil and
Pijul repos get the watcher plus the startup reconcile, which is correct for
all of them because it compares file contents, not VCS history. Jujutsu in its
default colocated mode has a `.git` and is already a git repo to Travsr.

Native hooks for other VCSs are out of scope. The design leaves room for them:
a VCS adapter only has to supply a file list and a "history moved" signal, the
two inputs D2 abstracts.

### D4. Workspace: one init for a folder of repos

`travsr init` at a folder that is not itself a repo, but has repos below it,
initializes a workspace:

1. **Discover members.** Walk down from the folder, honoring `.travsrignore` and
   the hard skip list, and stop descending at each directory holding a `.git`
   (directory or gitlink file). Each such directory is a member. A member's own
   nested repos are its business, as they are today. A gitlink that points into
   another repo's `worktrees/` is a linked worktree of a repo already in the
   workspace or elsewhere; it is listed but not made a member, since it would
   give the same code a second index and duplicate every fan-out result.
2. **Confirm before writing.** Initializing a member installs its hooks and
   adds the Travsr block to its `.gitignore`, so one command at the root leaves
   an uncommitted change in every member repo. Init prints the members it found
   and what it will change, and asks before continuing. `--yes` skips the
   prompt, and `--dry-run` prints the list and stops.
3. **Initialize each member as an ordinary repo**, exactly as `travsr init`
   inside it would: its own `.travsr/graph.db`, its own corpus, its own hooks,
   its own registry entry. A member that is already initialized is left alone
   (`init` is already idempotent).
4. **Write a workspace manifest** at `<folder>/.travsr/workspace.toml`, listing
   members by relative path. The manifest holds no graph.
5. **Register the workspace root** in the registry as a workspace entry, not a
   repo. Today a folder with no `.git` and no registered graph fails every
   command with "not inside a git repository", so without this `status`,
   `daemon` and `fsck` run at the root would fail right after a successful
   init. At the root, `status` reports each member; commands that act on one
   index (`fsck`, `embed`) ask for a member.
6. **Wire one MCP server** for the folder (`.mcp.json` and the other RFC-026
   targets at the workspace root), running `travsr mcp --workspace <folder>`.

If the folder has no repos below it, init falls through to plain mode (D1). If
it has repos **and** loose source files outside any member, the loose files are
not indexed in v1, and init says so with a count. See Unresolved Q2.

`travsr init` run again at the workspace root re-discovers members, so a newly
cloned repo joins the workspace without editing the manifest.

### D5. One map is a federation, not a merge

The issue asks how a child's own `.travsr/` relates to the top-level map:
merge, link, or replace. **Link.** The other two break node identity.

`NodeId = BLAKE3(corpus ∥ root ∥ path ∥ language ∥ signature)` (ARCH-102), and
the corpus is derived per repo from its remote (`github.com/acme/api`) or
`local/<basename>`. Merging every member into one database under the
workspace's corpus would give each node a different id from the one the
member's own index gives it, so a member opened on its own and the same member
inside the workspace would disagree about every symbol, and the cross-repo
`Exports` joins #670 is building would silently match nothing. Replacing the
member's map with the workspace's has the same problem, and also breaks
`travsr` run inside the member.

So the "one map" is the set of member maps, addressed together. Each member
stays fully usable on its own, and the workspace adds nothing that can go
stale independently.

### D6. MCP: `travsr mcp --workspace <folder>`

Global mode with the registry filtered to the manifest's members. It reuses
`run_global`, `collect_global` and `tools_list_global` unchanged, so every tool
that accepts `repo` in global mode accepts it here, and omitting `repo` fans out
across the workspace's members only, not every repo on the machine.

`repos_list` returns the members, so the agent's discovery step is unchanged.

The server re-reads the manifest when it changes, not only at startup. D4
promises that re-running `travsr init` adds a newly cloned repo without
editing the manifest; if the running server kept its startup member list, the
new member would stay invisible to the agent until the editor restarted the
server.

**Per-query repo selection** is explicit in v1: the agent names a member with
`repo`, or omits it to search all of them. The existing tool descriptions
already tell the agent to call `repos_list` first and to supply `repo`. No
heuristic chooses a member for the agent; a wrong guess would be invisible to
it, and fan-out already returns per-repo prefixed results it can narrow from.

**Tool parity.** Global mode serves 17 tools to single-repo mode's 29. The 12
missing are the four below, the six `synonym_*` tools, `repos_prune` and
`repos_remove`. The gap that matters for a workspace is
`get_architecture_brief`, `get_subsystem_brief`, `check_invariants` and
`repo_languages`. They are added to the global list with a required `repo`
argument (each describes one repo, so a fan-out has no meaning), bringing it to
21. The remaining eight are administrative and stay single-repo.

## Acceptance criteria

Mapped to #1002:

- [ ] `travsr init` succeeds in a folder with no VCS; an edit made while the
      daemon runs is queryable, and an edit made while it is stopped is
      queryable after it restarts. A `.gitignore`d directory is not indexed,
      and a `.gitignore` in a folder above the root has no effect.
- [ ] In plain mode, an edit whose watcher event was dropped is queryable after
      the next GC tick, without a restart.
- [ ] `travsr init` refuses `$HOME` and `/`, including when `$HOME` is itself a
      git repo and init runs in a folder below it.
- [ ] `travsr init` at a folder of N git repos lists the N members and asks
      before changing them, then initializes N members and writes one
      manifest; each member's `NodeId`s equal those from a standalone
      `travsr init` inside it. A linked worktree is not made a member.
- [ ] `travsr status` at the workspace root reports every member.
- [ ] `travsr mcp --workspace` answers a query with `repo` set to one member,
      and fans out across members only when `repo` is omitted. A member added
      by re-running init is visible without restarting the server.
- [ ] Behavior inside an ordinary git repo is unchanged (existing suite green).
- [ ] Docs describe plain mode and the workspace layout.

## Delivery

Three PRs, each independently useful:

1. **Plain mode** (D1, D2, D3): `travsr-cli` root resolution and guards, the
   `vcs = none` marker; `travsr-daemon` reconcile file-list parameter, startup
   and dropped-event triggers, `require_git(false)` with `parents(false)`, no
   hooks in plain mode, `status` freshness source.
2. **Workspace init** (D4): member discovery, confirmation, per-member init,
   manifest, root registration, MCP wiring at the root. Gated on the
   one-daemon-per-member measurement (Consequences), since this is the PR that
   turns one command into N daemons.
3. **`mcp --workspace`** (D6): registry filter, manifest reload, the four tools
   added to the global list.

## Consequences

- One daemon per member. A workspace of 20 repos runs 20 daemons and 20
  watchers, as 20 separate `travsr init`s would today. This needs measuring
  before delivery PR 2 merges (idle RSS per daemon, and inotify watch count
  against `max_user_watches` on Linux, which is low on older kernels and small
  machines). If the numbers are bad, the first step is starting a member's
  daemon on its first query rather than at init. A shared multi-repo daemon is
  the larger fix, and is a separate RFC.
- Plain mode's catch-up hashes every file at daemon start. That is the same cost
  `reconcile_head_drift` already pays on a `HEAD` mismatch, paid on every start
  instead. Measure on a large plain folder.
- Two new on-disk artifacts at the workspace root: `.travsr/workspace.toml` and
  the MCP config files. Both are local and gitignored by the same scaffolding
  init already applies.

## Alternatives considered

- **Tell users to `git init` the parent.** Today's workaround. It indexes every
  child into the parent's corpus (D5), installs hooks that never fire for
  commits made inside the children, and leaves a stray repo in the user's
  folder.
- **One database for the whole workspace.** Fewer files and one daemon, but it
  breaks node identity (D5). Rejected.
- **`--global` as the workspace server.** Already works, but it serves every repo
  ever initialized on the machine, so a query fans out into unrelated projects.
  Workspace mode is global mode with a scope.
- **Auto-select the member per query** (from the launch cwd, or by guessing from
  the symbol). Rejected for v1 (D6): a silent wrong pick is the failure mode
  ARCH-102 exists to prevent, and the explicit `repo` argument already exists.

## Unresolved questions

Each has a proposed answer for review; none is settled until this RFC is
accepted.

1. **Discovery depth.** Should member discovery have a depth limit, or is
   stopping at each `.git` plus `.travsrignore` enough to keep a stray deep scan
   bounded?
   *Proposed:* a default depth of 3, overridable with `--depth`. Stopping at
   `.git` bounds the walk only where there are repos; a deep folder with none
   (a data or download directory) is walked to the bottom.
2. **Loose files at the workspace root.** Index them as an extra plain-mode
   member (corpus `local/<workspace-basename>`, excluding member roots), or
   leave them out? v1 leaves them out and reports the count.
   *Proposed:* leave them out in v1, as above. Adding the extra member later
   breaks nothing, while removing it would drop an index users rely on.
3. **Removed members.** When a member directory disappears, should re-running
   init at the root drop it from the manifest only, or also unregister it
   (`repos --remove`)?
   *Proposed:* drop it from the manifest only, and print a hint to run
   `travsr repos --prune`. A missing directory may have been moved, and
   unregistering it is an action the user should take on purpose.
4. **Plain mode inside a workspace.** Should a non-git child folder (D3's
   Mercurial repo, say) be discoverable as a member? It has no `.git` to stop
   at, so discovery would need a second marker such as `.hg`, or an explicit
   entry in the manifest.
   *Proposed:* an explicit manifest entry only in v1. Marker detection can be
   added later; once shipped, it cannot be removed without dropping members
   users already have.

## Out of scope

- Cross-repo edges between members. That is #670; a workspace makes the set of
  repos to join explicit, but this RFC adds no edges.
- Native hooks for non-git VCSs (D3).
- A shared daemon serving several repos (Consequences).
- Remote or cloud workspaces. Everything here is local.
