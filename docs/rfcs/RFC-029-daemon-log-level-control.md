# RFC-029: Daemon Log Level as a Setting, and a Log That Describes Itself

**Date:** 2026-09-13
**Status:** Proposed. Parts 1 to 5 are implemented against this branch; part 6
(human-readable rendering) is proposed only and nothing in it is built.

**Implemented so far:** the `log.level` config key and its precedence, the
daemon reading it at startup, the session line becoming exempt from the level it
reports, the Health panel's Write control and its pending-restart note, and the
Repositories expanders. 72 extension tests pass, 11 of them new; 18
`travsr-config` tests pass, 5 of them new; 246 in `travsr-daemon`, 3 of them new.

## Context

The daemon writes `~/.travsr/daemon.log.<DATE>` (or the repo's `.travsr/`), and
until now the only way to change how much it wrote was `RUST_LOG`.

That is an environment variable on a process **nobody launches by hand**. The
daemon is spawned in the background by `travsr init`, by the VS Code extension,
or by the Task Scheduler autostart. So the level it ran at was whatever happened
to be exported in the shell that triggered the spawn, and it held for the whole
life of the process. Three consequences, all observed:

**The setting could not be found.** There was no `travsr config` key, nothing in
the Health panel, and no flag except `daemon start --verbose`. A user who wanted
debug output had to know that `RUST_LOG` reaches a process they never start.

**The setting could not be turned off.** The CLI's own troubleshooting messages
print `RUST_LOG=travsr_plugin_host=debug travsr init --semantic --force`. That
command auto-starts a daemon, which then inherits `debug` and keeps writing at
debug indefinitely. Nothing in `travsr status` or the log said why the file had
grown.

**The file could not say what it was.** This is the subtle one and it is what
made the first implementation of this RFC wrong. `daemon.session.start` is
emitted at INFO, so a daemon running at `error` or `warn` suppressed the single
line that identifies the session and its level. Setting `error` and restarting
therefore wrote **nothing**, the previous session's `log_level=info` line was
still in the file, and every reader (human or panel) took it as describing the
current daemon. A log that cannot describe itself is worse than a quiet one: it
is confidently wrong.

## Decision

### 1. `log.level` is a registered config key

Added to `travsr-config`'s registry, so `get`, `set`, `unset` and `list` all work
with no new CLI code:

| | |
|---|---|
| Key | `log.level` |
| Values | `error` \| `warn` \| `info` \| `debug` \| `trace` |
| Default | `info` |
| Env | `TRAVSR_LOG_LEVEL` |

Validation rejects anything else. This matters more than it looks: `EnvFilter`
reads an unrecognised bare word as a **target name** with no level, so
`log.level = "verbose"` would not fail, it would silently change what is
recorded. A value this build does not know (hand-edited file, or written by a
newer travsr) falls back to `info` rather than reaching the filter.

Deliberately **not** a full `RUST_LOG` directive string. This key exists to be
driven by a dropdown and by `config set`; accepting `travsr_daemon=debug,hyper=off`
would put a filter grammar behind a five-item control, with per-target typos
discarded at parse time. Anyone who needs that grammar has `RUST_LOG`.

### 2. Precedence, with `RUST_LOG` still on top

`travsr_config::resolve_log_filter` returns the directive **and the layer that
chose it**:

1. `RUST_LOG`, verbatim, directives and all.
2. `log.level`: `TRAVSR_LOG_LEVEL`, then repo `config.toml`, then global.
3. `info`.

`RUST_LOG` wins because it is strictly more expressive and because every
troubleshooting message the CLI prints tells people to use it. A key that
silently overrode it would break the documented path.

An empty or all-whitespace `RUST_LOG` counts as unset. `RUST_LOG= travsr daemon
start` is how a shell clears it for one command, and taking it literally would
hand `EnvFilter` an empty directive and silence the file entirely, which is the
opposite of what was meant.

The precedence itself is a pure function of two inputs, separate from the code
that reads the environment and the config files, because the real function reads
process-global state that a parallel test cannot own.

### 3. The session lifecycle lines are exempt from the filter they report

`daemon.session.start` and `daemon.session.exit` both carry a dedicated target,
`travsr_daemon::session` (`SESSION_LOG_TARGET`), and the subscriber appends a
directive admitting that target unconditionally:

```
error  ->  error,travsr_daemon::session=trace
```

Two lines per daemon lifetime is a price worth paying at any level for a file
that says what it is and why it stopped. The extension's `shortTarget` splits on
`::`, so entries still render under `daemon` and the log view is unchanged.

A high severity is not a substitute for the exemption, which is how the exit
line was got wrong at first. ERROR passes any bare level, so under the
`log.level` path it was recorded, but a targeted directive with no bare level
leaves `EnvFilter`'s unmatched default OFF and drops it. That is precisely the
form the CLI's troubleshooting text prints, and it auto-starts a daemon:

```
RUST_LOG=travsr_plugin_host=debug travsr init --semantic --force
```

so a user following the documented workflow whose `graph.db` then vanished got
the silent mid-session stop this event exists to remove. `filter_directive_for`
admits the target, not a severity; anything that must outlive the filter has to
ride it.

Appended whatever chose the directive, `RUST_LOG` included. The first version
exempted the escape hatch on the grounds that readers tell a `RUST_LOG` run
apart by `log_level_from`, which is true but only if a line is written at all:
under `RUST_LOG=warn` the daemon wrote no session line, a reader landing on the
same day's file found a *previous* session's line, believed it, and asked for a
restart that could never clear, because every restart under that `RUST_LOG`
writes no line either. That is the exact bug this section exists to prevent,
reintroduced through the one path that opted out of it. Everything else in an
explicit `RUST_LOG` is still honoured as written; one line per process start is
a small enough imposition for a file that always says what it is.

The panel reads the active level only when `log_level_from` is `log.level` or
`default`. Judging by the shape of the value instead was wrong for
`RUST_LOG=debug`, which is what `daemon start --verbose` sets itself.

The line carries two new fields:

```json
"log_level": "debug", "log_level_from": "log.level"
```

so "there are no debug lines in here" and "debug is off" stop being
indistinguishable from the file alone, which is the only artifact left once the
process is gone.

### 4. The Health panel writes it, and says when it is not live yet

A **Write** control sits on the log toolbar beside File, Lines and Since. It
posts `setLogLevel`, which runs `travsr config set log.level <v> --repo` and
re-renders. `--repo` on purpose: the level is a debugging choice about one
repository's daemon, and writing the global file from a panel would change it
for every other repo on the machine.

Three rules the panel has to follow, each of which was got wrong first:

**Never render a guessed selection.** When `config list --json` has no
`log.level` row the binary predates the key, and a dropdown defaulted to `info`
would be a control that silently does nothing. It renders "Write: needs a newer
travsr (this one is `<version>`)" with a **Change binary** link, following the
Languages section's contract-skew precedent: report which binary is behind
rather than withholding silently.

**Read the newest session, not the first.** `readDaemonLogFile` returns the tail
in **file order, oldest-first**. Taking the first `daemon.session.start` meant a
file holding a restart reported the level the daemon started the *day* with.
`activeLogLevel` scans backwards.

**Only describe a daemon that exists.** The active level comes from a file that
outlives the process that wrote it, so the pending-restart note is gated on
`daemonRunning`. Without that gate it claimed "the running daemon is still
writing at info" about a stopped daemon and offered a restart that would change
nothing.

The level applies at daemon startup, so a stored level the running daemon has
not picked up renders a note naming what is live now, with a Restart button.
The panel *offers* the restart rather than performing it: a log-level change
must not silently cancel an in-flight index.

### 5. Levels that match what the line is worth (implemented for `query.served`)

A level control is only worth having if the levels mean something. Measured
against this repository's own `daemon.log`, they mostly do, with one clear
exception.

Message frequency across the checked-in log, by level:

```
  28  INFO  query served
  13  INFO  embed_text updated
  11  INFO  daemon starting
  10  WARN  write_phase_b_batch: skipped edges with a missing endpoint
   7  WARN  skipping reindex: this index was built with an older version
   5  WARN  rust-analyzer skipped, no OS sandbox available
```

`query.served` was INFO on every query, making it the most frequent line in the
file by a wide margin, most occurrences being `elapsed_ms=0` cache hits. It
broke the rule this file's own module docs state: a line earns its place where
something happened that a reader would count, alert on or chart, not for the
running commentary in between. It was also never added to the documented `event`
key list, which is how it grew to that volume unnoticed.

Dropping it to DEBUG outright would have cost the thing it was added for, since
"which query was slow" has to stay answerable without restarting the daemon. So
the level follows the content: INFO at or past `SLOW_QUERY_MS` (200 ms, four
times the 50 ms p95 the bench gate enforces), DEBUG below it, same key and same
fields either way. The same split is already the house pattern:
`sidecar.version.checked` is DEBUG so healthy spawns do not flood, while
`below_floor` is WARN.

Measured on a six-query session: 6 lines at INFO before, 0 after, and all six
still present under `log.level=debug`.

`query.served` and `query.failed` are now in the documented key list, with the
level rule recorded beside them. A consumer counting served queries has to read
at debug; one watching for slow ones can stay at the default.

**Deliberately left alone,** having checked each against its call site rather
than its frequency:

- `write_phase_b_batch: skipped edges` (WARN) reports real data loss and is
  already aggregated to one line per batch with a count.
- `rust-analyzer skipped, no OS sandbox` (WARN) carries a UX-002 comment arguing
  the case: a degradation the run recovers from, with the recovery folded into
  the one line.
- The `live:` and `live_resolve` failures at DEBUG are per-file speculative
  enrichment on the editor path. Raising them would flood on every keystroke,
  which is the same argument that keeps the editor plane to two lifecycle lines.
- `skipping reindex: this index was built with an older version` (WARN) repeats,
  but once per genuine reindex attempt that did not happen, which is a fact each
  time rather than a repeated complaint.

### 6. Human-readable logs (proposed, not built)

The stored format is JSON lines and **should stay that way**. It is a contract:
`travsr daemon logs` renders it, the Health panel renders it, `--json` hands the
raw line to `jq`, Loki and Datadog, and `logfile.rs` documents an `event` key set
that queries are built on. Rewording a message must not break a dashboard, which
is exactly what a prose format cannot promise.

What is missing is not the format but the path to a readable view. Three gaps,
in order of how often they bite:

**The raw file is what people find.** `.travsr/daemon.log.2026-09-13` is visible
in the file tree and in the panel's own File control, and opening it in an
editor gives this:

```json
{"timestamp":"2026-09-13T09:47:44.649124Z","level":"INFO","fields":{"message":"daemon starting","event":"daemon.session.start","version":"1.0.0","pid":3728,"repo":"C:\\Users\\...\\repo","log_level":"info",...},"target":"travsr_daemon::session"}
```

Nothing anywhere tells that reader `travsr daemon logs` exists. README line 281
lists the command among thirty others. **Proposal:** `daemon start` and
`daemon restart` print the log path and the command that renders it, once, on
the success line.

**Machine keys are shown to humans.** The rendered view repeats in prose what the
message already said:

```
15:23:45         daemon   daemon starting event=daemon.session.start log_level=warn log_level_from=log.level pid=17364 pruned_logs=0 version=1.0.0
```

`event=daemon.session.start` is a selector for `jq`, not information for a
person reading "daemon starting". **Proposal:** suppress `event` in the human
renderer (it stays in `--json` and in the file), and order the remaining fields
by relevance rather than alphabetically, so `log_level` does not sort between
`error` and `pid`.

**A level filter cannot recover what was never written.** `daemon logs --level
debug` on a file written at `info` returns nothing, correctly, and looks like
"no debug activity". The CLI already warns about this for `--verbose`; with
`log.level` now settable the same warning should name the setting.

Explicitly **not** proposed: a second human-readable file alongside the JSON one.
It doubles the write path and the rotation budget to serve a reader who is one
command away from a better view.

One thing to keep as it is: the human renderer leaves INFO blank on purpose.
It is the level of nine lines in ten, so printing it is four characters of
"nothing unusual happened" per line and it buries the two lines that do say
something. WARN and ERROR stand out from a column of blanks. That reads as a
missing column at first glance and is not one.

## Consequences

`travsr config set log.level debug` now does what `RUST_LOG=debug` did, except
that it persists, is per-repo, is discoverable in `config list`, and can be
turned off from the same place it was turned on.

The level still applies at daemon startup, not live. Making it live needs a
`tracing_subscriber::reload` handle and either a control-socket message or a
config re-read on the existing scheduler tick. That is a real improvement and
deliberately out of scope here: it adds protocol surface to a change whose point
was disclosure.

`travsr mcp --global` writes the same `daemon.log.*` files in the global home
and follows the same key, resolved at global scope, or one setting would be true
of one file and not the other.

`travsr mcp --global`'s file follows the setting but deliberately not
`RUST_LOG`, unlike the daemon's. That process is spawned by an editor, so its
environment is not one a person chose for it, and honouring `RUST_LOG` there let
an inherited value empty the durable log. `RUST_LOG` still governs its stderr.

At `error` the only line a healthy daemon writes is its own session line. That
is the point (the file says who wrote it and that it will stay quiet), but it
does mean the Recent activity feed and the panel's log view are empty by design
at that level, not broken.

## Alternatives considered

**A `--log-level` flag on `daemon start`.** Does not survive a restart, and the
daemon is usually started by something other than a person, which is the whole
problem.

**Making the session line INFO and accepting the gap.** What the first
implementation did. It produced a panel that demanded a restart that had already
happened, indefinitely, for every level below INFO.

**Having the panel ask the running daemon over the control socket.** Correct and
never stale, and it would survive a level that suppresses the session line. It
needs a new `ControlMessage` variant plus a `OnceLock` holding the resolved
directive. Worth doing if the session-target exemption ever proves too clever;
the exemption was chosen because it also fixes the file for a human reader, which
a control-socket answer does not.
