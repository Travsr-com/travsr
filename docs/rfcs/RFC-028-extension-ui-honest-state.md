# RFC-028: Extension UI, Honest State and the Health Panel

**Date:** 2026-09-02
**Status:** Proposed. Parts 1 and 3 are implemented against this branch; part 2
(`viewsWelcome` and the context keys) is not yet.

**Implemented so far:** the rename, the verdict banner and its actions, the
Index and daemon section reading `get_index_status`, the state-carrying Last
indexed tile, the de-duplicated diagnostic card, Run and Copy on a diagnostic's
command, the shared Travsr terminal, and the graph webview's label level of
detail. 447 tests pass, 21 of them new.

## Context

The VS Code extension has grown a lot of surface: a Cytoscape graph webview, a
Context Explorer, Languages and Stats panels, a code lens, a hover card, a
multi-repo picker, and twenty-one commands. The individual pieces work. What is
missing is that **the extension frequently reports a state that is not true**,
and that when it does report a problem it offers no way to resolve it.

Three concrete observations drove this RFC.

**The state model lies.** `src/status.ts` declares five states and renders all
five, but `state = "stale"` is never assigned anywhere in the file: the
assignments are `fresh` (L132), `error` (L136, L144), `connecting` (L161) and
`indexing` (L172, L193). The contributed colour `travsr.staleForeground` and the
`timeAgo` label are unreachable code. A graph six commits behind the checkout
renders green and says `fresh`.

**A dead daemon is indistinguishable from an empty result.** `StdioMcpClient`
resolves every failure to `""` and never rejects (`src/mcp.ts`), and the tree
providers catch and return `[]` (`src/tree.ts:179-181, 207-209`). So a stopped
daemon renders as "No dependencies found", the hover returns `undefined`, and
the code lens is omitted. Worse, `hover.ts:93-98` and `codelens.ts:127-130`
**cache the failure as an empty result**, so the wrong answer persists until the
next save.

**Nothing is actionable.** A real session, captured while writing this RFC,
showed the Graph Stats panel rendering five green metric tiles and the line
"1 warning" while the daemon had exited four minutes earlier and the graph was
nine hours stale. The one warning it did show, a Java analyzer that resolved
zero symbols, printed its entire message twice, once truncated mid-sentence into
the heading, and ended with a command to retype in a terminal.

The panel that ought to answer "is Travsr working" is named after one of its
metric groups, and its remedies are all text to copy.

## Decision

Three changes, in this order.

### 1. Model the state honestly

Introduce `src/uiState.ts` as the single owner of three context keys:

| Key | Type | Meaning |
|---|---|---|
| `travsr.daemonState` | `connecting`, `fresh`, `indexing`, `stale`, `error` | what the daemon is doing |
| `travsr.repoIndexed` | boolean | whether a graph exists for this repo |
| `travsr.binaryFound` | boolean | whether a spawnable binary resolved |

Setters dedupe on value and only then call `setContext`, so a 30 second poll
does not churn the context service. Every consumer accepts an injected instance
so tests do not leak keys across suites.

`stale` becomes reachable by reading the daemon's own drift report rather than
re-deriving it in TypeScript. `get_index_status` already returns
`staleness.{behind_by, is_stale, working_tree_dirty}` alongside `indexed_commit`
and `head_commit` (`crates/travsr-mcp/src/observability.rs:654-812`). `is_stale`
is deliberately tri-state and already handles short-SHA comparison and linked
worktrees (#636). Reimplementing that against the Git extension's HEAD would
duplicate a fix we have already paid for.

Unavailable is distinguished from empty without changing the "never rejects"
contract. Every list tool passes through `sanitize_for_mcp`, which always
returns at least the empty envelope `<travsr-data></travsr-data>`
(`crates/travsr-mcp/src/sanitize.rs:48,138-144`). Therefore `raw === ""` means
the call did not answer and a stripped-empty envelope means a genuine empty
result. Neither hover nor code lens caches the former.

### 2. Offer the action beside the problem

Contribute `viewsWelcome` for both trees, keyed on the context keys above, so
each unhealthy state names itself and carries a button: Download for a missing
binary, Restart and Show Output for a dead daemon, Index This Repo for an
unindexed repo. This requires both providers to return `[]` at the root in those
states, since VS Code renders welcome content only for an empty tree.

Two new commands back the buttons: `travsr.restartDaemon` and
`travsr.showOutput`, both of which already exist as inline handlers inside the
status quick pick and are simply lifted out.

The daemon-offline toast stops being one-shot. Today `wireDisconnectHandler`
calls `sub.dispose()` on first fire (`src/extension.ts:846`), so dismissing it
means never being told again. That one-shot is not what prevents double-firing
on an explicit restart: `StdioMcpClient.dispose()` sets `connected = false`
before killing the process, so `onExit` does not notify listeners on a swap
(`src/mcp.ts:58-67, 174-180`). The listener becomes permanent and a rate limiter
keeps it to one toast per disconnect episode.

### 3. Rename Graph Stats to Travsr: Health, and make it a health page

The panel already carries the daemon log, a rotation-aware file picker, an
auto-refresh interval and severity filters. It gains a verdict, the rest of the
state, and an action per row:

- A **verdict banner** naming the overall state in a word plus an icon, with the
  primary action beside it.
- **Metric tiles** whose values carry state. Last-indexed at nine hours reads
  amber with the word "stale", not green like the rest.
- **Daemon**: process, version against the extension's expectation, transport
  latency, log file. Actions: Start, Restart, Stop, Open log.
- **Index freshness**: indexed commit against checkout HEAD, phase A and phase B
  state, and whether the post-commit hook is installed. Actions: Reindex, Full
  rebuild, Install hook.
- **Sidecars**, **Languages**, **Agent connections**, **Repositories**,
  **Storage and integrity**, each with the action that resolves it.
- Diagnostics render a short claim as the heading and the explanation once, plus
  the checks the analyzer actually performed. Any command is a Run button beside
  a Copy button, not a block to retype.

Destructive actions (full rebuild, prune, repair) confirm first and say what
they will remove. Every action reports the real exit status of what it ran
rather than a fixed timer's guess, which is the bug fixed in 0.11.0 for language
installs and which must not be reintroduced here.

**The rename keeps the command id.** `travsr.showGraphStats` and the
`travsrStats` viewType are API: they appear in user keybindings, in
`menus.view/title`, and in the extension's own tests. Only the display strings
change, so an existing keybinding keeps working.

## Graph webview: labels

Shipped alongside this RFC, and recorded here because it changes how the graph
reads. Cytoscape has no label collision avoidance, and every node asked for its
label at every zoom. On this repository's own `crates/travsr-core/src/exec.rs`
that is 157 names drawn over each other.

Ordinary nodes now carry a `.nolbl` class and give up their label until either
the view is zoomed past `LABEL_ZOOM`, or the pointer is on their neighbourhood.
The seed node and the ten busiest hubs keep their names at every zoom, so the
picture always has landmarks, and the count of labels cannot grow with the
graph. Anything the user has singled out (selection, a search hit, the blast
rings) keeps its name regardless. Package tiles, ghosts and compound parents are
exempt: for those the label is the content, not an annotation on a dot.

## Alternatives Considered

**Derive staleness in the extension from the Git extension's HEAD.** Rejected:
it duplicates `is_stale`, including the short-SHA and linked-worktree handling
from #636, and it would drift from the daemon's own answer. The cost of the
chosen approach is three `git` spawns per 30 second poll on the daemon side; if
that shows contention with in-flight queries, call it on alternate polls.

**Add a `callToolResult()` that returns `{ok, text}`.** Rejected as unnecessary.
The envelope guarantee already distinguishes unavailable from empty, and
changing the client interface would break every test stub for no gain.

**Rename the command id to `travsr.showHealth`.** Rejected. It buys naming
consistency and costs every user's existing keybinding. If the old name becomes
confusing in the source, a doc comment is cheaper than a breaking change.

**Put repository diagnostics in the Problems panel.** Rejected, and the existing
rationale at `src/commands.ts:1443-1454` still holds: none of them belong to a
file, so they would be entries without a location.

## Consequences

- `stale` becomes reachable, so users will see amber where they saw green. This
  is the point, and it will look like a regression to anyone who assumed green
  meant "working". The tooltip names both commits so the claim is checkable.
- A local commit briefly shows stale until the daemon reconciles. Honest, and
  the alternative is a green light that lags reality.
- Hover and code lens re-query while the daemon is slow rather than serving a
  cached wrong answer. Bounded by user activity.
- The health panel grows from three sections to eight, so it becomes a page that
  scrolls. Sections collapse and the verdict is the part that must be readable
  without scrolling.
- Renaming the panel changes a string users search for. The command palette
  entry moves from "Travsr: Graph Stats" to "Travsr: Health"; the CHANGELOG must
  say so plainly.

## Out of Scope

- Panel state persistence across refresh (#767). Every managed panel still
  rebuilds `webview.html` wholesale, discarding filter text, chips, toggles and
  scroll position. It is the largest remaining UI debt and needs its own change.
- The native provider work (call hierarchy, references, change impact keyed to
  the working set). Higher value than most of this RFC, and larger.
- Determinate progress for long operations, and a warming-up state for the
  embedding sidecar.
- Any CLI change. `travsr status` still has no `--json` and no verdict line;
  that belongs in a CLI RFC.

## Verification

- `npm run compile`, `npm run lint`, `npm test` in `packages/travsr-vscode`.
- New suites: `uiState.test.ts` (dedupe, event, `publishAll`),
  `offlineNotifier.test.ts` (one toast per episode, re-arm, rate limit),
  `repoFileTree.test.ts`, and `contributions.test.ts` asserting that every
  command referenced by a menu or a `viewsWelcome` entry is declared, that every
  `travsr.*` token in a `when` clause is a known context key, and that no
  em dash reaches a user-facing string.
- Extended: `status.test.ts` gains per-state render assertions (text, colour id,
  tooltip command links), the stale transition, disconnect re-arm across two
  episodes, and the connecting watchdog; `tree.test.ts` gains unavailable versus
  empty.
- Manual, in the Extension Development Host: point `travsr.binaryPath` at a
  bogus path and confirm both trees show the binary welcome; open a repo with no
  `.travsr` and confirm the index welcome; kill the daemon and confirm the toast
  fires, then kill it again after a restart and confirm it fires a second time;
  check out an older commit and confirm the status bar goes amber within one
  poll.
