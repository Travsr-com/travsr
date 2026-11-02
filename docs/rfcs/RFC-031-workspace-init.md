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
- Print one line saying the folder is in plain mode and how it stays fresh
  (below), so a user who expected git hooks is not misled.

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

No git hooks are installed in plain mode, and `status` reports the freshness
source as `watcher` rather than `post-commit hook`.

**Ignore rules need one explicit change.** The init walk builds with
`.git_ignore(true)`, and the `ignore` crate applies `.gitignore` files only
inside a git repository unless `require_git(false)` is set. Without it, a plain
folder would index `node_modules/` and every other path its `.gitignore`
excludes. Plain mode sets `require_git(false)` on the `WalkBuilder`s that
enumerate files (the init walk and the reconcile walk), so `.gitignore` and
`.travsrignore` mean the same thing with or without git. It is set in plain
mode only: with it off, the crate also reads `.gitignore` files above the root,
which inside a git repo would differ from git. The watcher's matcher needs no
change; `build_ignore_matcher` reads `.gitignore` files directly through
`GitignoreBuilder`, which has no git requirement.

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
   nested repos are its business, as they are today.
2. **Initialize each member as an ordinary repo**, exactly as `travsr init`
   inside it would: its own `.travsr/graph.db`, its own corpus, its own hooks,
   its own registry entry. A member that is already initialized is left alone
   (`init` is already idempotent).
3. **Write a workspace manifest** at `<folder>/.travsr/workspace.toml`, listing
   members by relative path. The manifest holds no graph.
4. **Wire one MCP server** for the folder (`.mcp.json` and the other RFC-026
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

**Per-query repo selection** is explicit in v1: the agent names a member with
`repo`, or omits it to search all of them. The existing tool descriptions
already tell the agent to call `repos_list` first and to supply `repo`. No
heuristic chooses a member for the agent; a wrong guess would be invisible to
it, and fan-out already returns per-repo prefixed results it can narrow from.

**Tool parity.** Global mode serves 21 tools to single-repo mode's 29. The gap
that matters for a workspace is `get_architecture_brief`,
`get_subsystem_brief`, `check_invariants` and `repo_languages`. They are added
to the global list with a required `repo` argument (each describes one repo, so
a fan-out has no meaning). The remaining gap (`synonym_*`, `repos_prune`,
`repos_remove`) is administrative and stays single-repo.

## Acceptance criteria

Mapped to #1002:

- [ ] `travsr init` succeeds in a folder with no VCS; an edit made while the
      daemon runs is queryable, and an edit made while it is stopped is
      queryable after it restarts. A `.gitignore`d directory is not indexed.
- [ ] `travsr init` refuses `$HOME` and `/`.
- [ ] `travsr init` at a folder of N git repos initializes N members and writes
      one manifest; each member's `NodeId`s equal those from a standalone
      `travsr init` inside it.
- [ ] `travsr mcp --workspace` answers a query with `repo` set to one member,
      and fans out across members only when `repo` is omitted.
- [ ] Behavior inside an ordinary git repo is unchanged (existing suite green).
- [ ] Docs describe plain mode and the workspace layout.

## Delivery

Three PRs, each independently useful:

1. **Plain mode** (D1, D2, D3): `travsr-cli` root resolution and guards;
   `travsr-daemon` reconcile file-list parameter, startup trigger,
   `require_git(false)`, no hooks in plain mode, `status` freshness source.
2. **Workspace init** (D4): member discovery, per-member init, manifest, MCP
   wiring at the root.
3. **`mcp --workspace`** (D6): registry filter, the four tools added to the
   global list.

## Consequences

- One daemon per member. A workspace of 20 repos runs 20 daemons and 20
  watchers, as 20 separate `travsr init`s would today. This needs measuring
  before it is called acceptable (idle RSS per daemon, and inotify watch count
  against `max_user_watches` on Linux). A shared multi-repo daemon is the fix if
  it is not, and is a separate RFC.
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

1. **Discovery depth.** Should member discovery have a depth limit, or is
   stopping at each `.git` plus `.travsrignore` enough to keep a stray deep scan
   bounded?
2. **Loose files at the workspace root.** Index them as an extra plain-mode
   member (corpus `local/<workspace-basename>`, excluding member roots), or
   leave them out? v1 leaves them out and reports the count.
3. **Removed members.** When a member directory disappears, should re-running
   init at the root drop it from the manifest only, or also unregister it
   (`repos --remove`)?
4. **Plain mode inside a workspace.** Should a non-git child folder (D3's
   Mercurial repo, say) be discoverable as a member? It has no `.git` to stop
   at, so discovery would need a second marker such as `.hg`, or an explicit
   entry in the manifest.

## Out of scope

- Cross-repo edges between members. That is #670; a workspace makes the set of
  repos to join explicit, but this RFC adds no edges.
- Native hooks for non-git VCSs (D3).
- A shared daemon serving several repos (Consequences).
- Remote or cloud workspaces. Everything here is local.
