# RFC-030: The `PreToolUse` guard

**Date:** 2026-09-23
**Status:** Implemented (#916)
**Extends:** RFC-026 (AI tool auto-configuration)
**Related:** #252 (push-based context injection), #655 (staleness detection)

## Context

Everything RFC-026 installs is advice.

- `.mcp.json` makes the Travsr tools *reachable*.
- The MCP schema descriptions describe each tool and hope the model picks it.
- `markdown_rules()` is the only thing that says "query the graph before grep",
  and it is opt-in and off by default (`--rules`).

So a default `travsr init` leaves nothing in the repository that prefers the
graph over `grep`. Agents have a very strong prior toward `Grep`, `Glob`, `Read`
and `bash: rg/find`, and they fall back to it constantly even with the rules file
on. #252 documented exactly this while dogfooding, and the workaround at the time
was a hand-written `PreToolUse` hook dropped into `.claude/hooks/` by hand. It was
never productised.

The result is that the value of the index is a coin flip per turn. The user paid
for indexing, the daemon keeps it fresh, and the agent still burns tokens
grepping the tree.

## Decision

Add `travsr guard`, a hook handler shipped inside the `travsr` binary, and wire
it into `.claude/settings.json` as an opt-in `PreToolUse` hook with two
enforcement levels.

### Why it lives in the binary

A script in `.claude/hooks/` has to be kept in sync with the CLI by hand, made
executable, and written twice: once for a POSIX shell and once for Windows,
where #252's Python hook never ran at all. A subcommand of the binary the user
already installed has none of those problems, and it can open the graph
directly rather than shelling out to the CLI it is part of.

### Why the policy is config, not hook config

The hook entry in `.claude/settings.json` names a binary and a matcher. Nothing
else. The enforcement level is `guard.mode` in `.travsr/config.toml`, through
the same layered registry every other setting uses.

That separation is what makes `travsr guard` and the installed hook unable to
disagree. It also means `travsr config set guard.mode strict --repo` is a
complete way to change the policy, `TRAVSR_GUARD=off` is the env layer of the
same key rather than a second code path, and a repository can carry an
enforcement level before Claude Code is ever installed in it.

### Two levels

| Level | Matched call the graph can answer | Everything else |
|---|---|---|
| `advisory` | the replacement call as `additionalContext`, no decision | a one-line nudge, no decision |
| `strict` | `deny` + the replacement call | no decision |

`advisory` is the default when the guard is enabled, because the nudge is worth
more at the moment the agent reaches for grep than it is once per turn in a
rules file it has learned to skim. `strict` is what makes the graph
non-optional.

Neither is installed by a plain `travsr init`. A silent `init` must not start
denying an agent's tool calls in a repository where nobody asked for it.

## The guard never approves anything

`permissionDecision: "allow"` is the host's **auto-approve**, not "do not
block": it lifts the user's own permission rules for that call. Exit 0 with no
JSON is the host's documented "no decision; normal permission flow applies",
and that is what "do not block" actually is.

So a refusal is the only decision the guard emits. Its claim is that it knows
which reads the graph can replace, and that says nothing whatever about which
paths a user is willing to have read. There are three outputs and only one of
them carries a `permissionDecision`:

| Output | On the wire | When |
|---|---|---|
| Neutral | nothing | outside the match set, or any fail-open condition |
| Context | `additionalContext`, no decision | every advisory output, and every strict output the valve released |
| Deny | `permissionDecision: "deny"` + the replacement | strict, graph-answerable, not released |

The first draft of this got that wrong in advisory mode, and the review caught
it. Advisory emitted `allow` for every matched call, including the
`redirect_for` `None` arm, which is exactly the set the guard had just declined
to vouch for: a `Read` of `~/.ssh/id_rsa` or `.env` or anything outside the
repository was auto-approved, lifting the user's `Read` gating on the very
paths the guard deliberately excludes. That is the same failure as
auto-approving `grep foo && rm -rf build` on the strength of its first word,
on a different tool. Dropping `allow` entirely closes the class rather than
the instance, and costs nothing: advisory never wanted to approve, it wanted to
teach.

The shell parser's strictness remains for the same underlying reason. It
refuses anything but a single simple command with no operator, substitution or
redirect, so the guard never forms an opinion about a command it cannot read.

## Fail-open

The guard refuses a read only when the graph can genuinely replace it. Every
other path allows:

| Condition | Where |
|---|---|
| `guard.mode = off`, including `TRAVSR_GUARD=off` | `policy::decide` |
| tool outside the match set, or a compound/unparseable shell command | `policy::classify`, `guard::shell` |
| `cwd` outside a git repository | `guard::repo_root_for` |
| no `.travsr/graph.db`; locked, corrupt, or schema-skewed | `policy::open_index` |
| index empty (Phase A pending) | `policy::open_index` |
| index does not describe `HEAD`, or `HEAD` cannot be resolved | `policy::open_index` |
| path untracked, ignored, vendored, binary, a lockfile, non-code | `policy::excluded_extension`, `vendored`, `file_indexed` |
| symbol unknown to the graph, or the term is a regex not a name | `policy::as_symbol`, `redirect_for` |
| file discovery (`Glob`, `find`, `ls -R`) | `policy::redirect_for`, unconditionally |
| ranged `Read` (`offset`/`limit`) | `policy::classify` |
| malformed or oversized payload | `guard::read_payload` |
| panic, internal error | panic hook + `catch_unwind` in `guard::run` |
| decision exceeds the deadline | `recv_timeout` in `guard::run` |

The daemon is deliberately not in that table: the guard opens the store itself
and never needs the daemon, so a daemon that is down is not a condition it has
to handle.

The open is read-only first, writable as a fallback, the order
`daemon_client::open_read_store` already uses, and for the same reason. SQLite
cannot open a WAL database read-only unless the `-shm` file exists, and after
the last writer closes it does not, so the usual state of an idle repo is one
where the read-only open fails outright. Preferring it is still right, because
it is the open that cannot migrate or checkpoint the user's index as a side
effect of deciding whether to allow a `grep`.

The deadline is enforced, not assumed. The decision runs on a worker thread and
the main thread stops waiting at `GUARD_DEADLINE`; the worker dies with the
process. This matters because `SqliteStore::open_read_only` carries a five
second busy timeout for a database the daemon is mid-write on, and that is the
agent's latency budget, not ours.

`TRAVSR_GUARD_DEADLINE_MS` overrides the budget, clamped to 1 ms to 60 s so the
bound can be moved but never removed. It is a diagnostic, for telling "the
graph cannot answer this" apart from "the guard ran out of time", and for
driving the timeout path from a test, not a setting, which is why it is an
environment variable rather than a `guard.*` config key. Raising it can only
make the guard slower, never more permissive.

The ceiling is generous on purpose. Opening SQLite is single-digit milliseconds
in a release build and can be *seconds* in an unoptimised one, on a network
filesystem, or on a machine whose endpoint security scans every file a process
touches; measured at ~15 s for a 180 KB index in a debug build on Windows with
EDR. A ceiling that could not reach those cases would make the diagnostic
useless in exactly the situations someone reaches for it. It is also why the
integration suite passes an explicit deadline: what the guard *decides* and how
long the machine takes to let it are separate questions, and only the two
deadline tests are about the second one.

`travsr guard --explain` prints the one-line reason behind a decision on
stderr, never stdout. "The guard is installed and nothing is blocked" has a
dozen causes that are indistinguishable from outside, and the alternative to
printing the reason is reading this file.

`HEAD` is read out of `.git` directly rather than through `git rev-parse`.
Spawning a process is the one operation on this path with an unbounded worst
case (a credential helper, an antivirus scanning the executable), and reading
two small files has no such tail.

## The strict-mode release valve

Strict mode denies a search the graph can answer. That is right until the graph
comes back empty, at which point plain text search is the correct move and a
second refusal is a dead end.

`PreToolUse` fires *before* a call, so the guard can see that the agent asked
Travsr about a symbol but not what came back. Two releases, both scoped to one
`session_id`:

1. **The agent already asked.** The installed matcher covers the travsr MCP
   tools as well as the search tools, so the guard observes a
   `get_callers(symbol="X")` go past and records it. A later search for `X` in
   that session is released. Releasing on the question rather than on an empty
   answer costs a redirect that was not needed; the other error costs the agent
   its fallback.
2. **The guard already denied this term once.** A second identical search in
   the same session is released whatever happened in between. This is the
   backstop that bounds the deny by construction: at most one refusal per
   `(session, term)`, so the guard cannot hold an agent in a loop.

State is one JSON file under `.travsr/`, which `init` already git-ignores. It
is pruned on every write: a 24 hour TTL, 64 sessions, 256 terms per session.
Every read and write fails open: no state means no release, which falls back
to (2).

## Consequences

- Claude Code is the only host this reaches. Cursor, Copilot, Gemini CLI,
  Antigravity, Codex, Windsurf and Zed have no pre-tool contract to hook, and
  the README says so rather than implying otherwise. #252's resource injection
  is the portable half.
- Advisory mode has no effect on whether a call is permitted. It attaches
  context and nothing else, so a repository that turns the guard on does not
  thereby change what its agent is allowed to read.
- A host that does not recognise a `hookSpecificOutput` carrying
  `additionalContext` with no `permissionDecision` ignores it. That degrades
  the nudge to silence, which is still correct: the fallback for every output
  but `deny` is "the host decides as it would have anyway".
- File discovery is never blocked, only nudged. The graph indexes code files, so
  it cannot enumerate the untracked, ignored, generated and non-code files a
  glob legitimately finds. Answering "here is the subset I know about" to a
  question about the whole tree would be wrong in a way the agent could not
  detect.
- `.claude/settings.json` joins the set of shared, committed, user-owned files
  travsr merges into rather than owns. It is not git-ignored: it carries no
  server definition, so the RCE-on-clone reason that ignores an `.mcp.json`
  does not apply.

## Alternatives considered

**A `PostToolUse` hook that corrects after the fact.** The tokens are already
spent by then; the whole point is to intervene before the search runs.

**`permissionDecision: "ask"` instead of `deny` in strict mode.** Puts a prompt
in front of the user on every grep and teaches the agent nothing either way. The
variant stays in the wire types but nothing constructs it.

**Blocking file discovery in strict mode.** Considered and rejected above: the
graph cannot answer the question a glob asks.

**Reading the transcript to learn whether a Travsr query came back empty.**
Accurate, and far too expensive for a 200 ms budget on every tool call. The
release valve approximates it and says so.
