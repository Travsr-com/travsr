//! `travsr guard`: the Claude Code `PreToolUse` handler (#916).
//!
//! `travsr init` wires the MCP server and, optionally, a rules file. Both are
//! advice. The agent still reaches for `Grep`, `Read` and `bash: rg` because
//! that is what it has always done, and instructions alone do not hold (#252).
//! This is the enforcement half: a hook that runs at the exact moment the agent
//! reaches for text search, and either names the Travsr call that replaces it
//! (advisory) or refuses and names it (strict).
//!
//! Shipped inside the binary the user already installed, rather than as a
//! script dropped into `.claude/hooks/`. There is nothing to keep in sync with
//! the CLI, nothing to make executable, and it behaves identically on Windows,
//! where the hand-written Python hook of #252 never ran at all.
//!
//! # Fail-open
//!
//! The guard blocks nothing it cannot replace, and approves nothing at all. A
//! `deny` is the only `permissionDecision` it ever emits; everything else is
//! either silence or context, and both leave the host's own permission flow
//! exactly as it was. See [`payload::HookOutput`] for why `allow` is not on
//! that list: it is the host's *auto-approve*, and the guard's claim to know
//! which reads the graph can replace says nothing about which paths a user is
//! willing to have read.
//!
//! # Deadline
//!
//! The decision runs on a worker thread and the main thread stops waiting after
//! [`GUARD_DEADLINE`]. Enforced rather than assumed: the work touches SQLite and
//! the filesystem, and `open_read_only` alone carries a 5 second busy timeout
//! for a database the daemon is mid-write on. On the deadline the guard emits
//! Neutral and exits, and the worker dies with the process.

mod payload;
mod policy;
mod session;
mod shell;

use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use payload::HookInput;

/// How long the guard may take before the tool call is let through regardless.
///
/// 200 ms is the budget the issue sets, and it is a ceiling on pathology rather
/// than a target: the measured work is a handful of indexed SQLite lookups and
/// two small file reads. What it actually bounds is the tail: a database the
/// daemon holds mid-checkpoint, a cold page cache, a network filesystem.
pub const GUARD_DEADLINE: Duration = Duration::from_millis(200);

/// Overrides [`GUARD_DEADLINE`], in milliseconds.
///
/// A diagnostic rather than a setting, which is why it is an environment
/// variable and not a `guard.mode`-style config key: it exists so the deadline
/// can be driven from both ends (set it to 1 and every decision must come back
/// `allow`), and so "the graph cannot answer this" can be told apart from "the
/// guard ran out of time" without reading the source. Raising it can only make
/// the guard slower, never more permissive.
///
/// [`DEADLINE_CEILING`] is generous for the same reason. Opening SQLite is
/// normally single-digit milliseconds, and is seconds in an unoptimised build,
/// on a network filesystem, or on a machine whose endpoint security scans every
/// file a process touches. A ceiling that could not reach those cases would
/// make the diagnostic useless in exactly the situations someone would reach
/// for it.
const DEADLINE_ENV: &str = "TRAVSR_GUARD_DEADLINE_MS";

/// Upper bound on the override. The bound itself is never removable: the whole
/// point of this timer is that the agent always gets an answer.
const DEADLINE_CEILING: Duration = Duration::from_secs(60);

/// The deadline in force.
fn deadline() -> Duration {
    decide_deadline(std::env::var(DEADLINE_ENV).ok().as_deref())
}

/// The clamp itself, with the raw value passed in.
///
/// Split out from [`deadline`] for the same reason `decide_log_filter` is split
/// out of `resolve_log_filter`: the real function reads a process-global
/// environment variable, which a parallel test cannot own.
///
/// Clamped so a typo cannot disable the bound. The whole point of this timer is
/// that there is always one, so `0` becomes one millisecond rather than "no
/// wait", and an unparseable value falls back to the documented budget.
fn decide_deadline(raw: Option<&str>) -> Duration {
    raw.and_then(|v| v.trim().parse::<u64>().ok())
        .map(|ms| Duration::from_millis(ms).clamp(Duration::from_millis(1), DEADLINE_CEILING))
        .unwrap_or(GUARD_DEADLINE)
}

/// Ceiling on the payload the guard will read. A `PreToolUse` payload for the
/// matched tools is a few hundred bytes; anything past this is not one, and
/// reading it into memory to discover that is the wrong trade.
const MAX_PAYLOAD_BYTES: u64 = 1024 * 1024;

/// How hard the guard pushes back, persisted as `guard.mode` in
/// `.travsr/config.toml` (or globally, or via `TRAVSR_GUARD`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum GuardMode {
    /// Installed but inert. The default, and what `TRAVSR_GUARD=off` selects.
    Off,
    /// Never blocks; attaches the Travsr call the agent should have made.
    Advisory,
    /// Denies a read the graph can answer, naming the replacement call.
    Strict,
}

impl GuardMode {
    pub fn as_str(self) -> &'static str {
        match self {
            GuardMode::Off => "off",
            GuardMode::Advisory => "advisory",
            GuardMode::Strict => "strict",
        }
    }

    /// Parse a stored or environment value. Anything unrecognised is [`Off`]:
    /// a mistyped `guard.mode` must not leave the guard blocking on a policy
    /// nobody can read back.
    ///
    /// [`Off`]: GuardMode::Off
    fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "advisory" => GuardMode::Advisory,
            "strict" => GuardMode::Strict,
            _ => GuardMode::Off,
        }
    }
}

/// The active mode for `repo_root`, through the normal config layering:
/// `TRAVSR_GUARD` > the repo's `config.toml` > the global one > off.
pub fn resolve_mode(repo_root: Option<&std::path::Path>) -> GuardMode {
    travsr_config::effective("guard.mode", repo_root)
        .map(|v| GuardMode::parse(&v))
        .unwrap_or(GuardMode::Off)
}

/// Run the hook: read a payload on stdin, write a decision on stdout.
///
/// Always `Ok`. The caller exits 0, because a non-zero exit from a
/// `PreToolUse` hook is a signal in its own right (2 blocks the call outright)
/// and the guard must never block by accident.
///
/// Blocks the runtime worker it is called on, which is the right trade here: a
/// one-shot CLI on a current-thread runtime with nothing else scheduled, and a
/// deadline that bounds the wait at 200 ms. Making it async would buy a thread
/// back from a process that is about to exit.
pub fn run(explain: bool) -> anyhow::Result<()> {
    // The CLI installs a process-wide panic hook that prints and calls
    // `process::exit(1)`, which would take the whole process down before the
    // decision reached stdout. Replace it for this subcommand so a panic
    // unwinds the worker thread instead: the channel then disconnects and the
    // main thread emits Neutral, which is the fail-open answer.
    std::panic::set_hook(Box::new(|info| {
        eprintln!("travsr guard: internal error, allowing the tool call ({info})");
    }));

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        // `catch_unwind` as well as the hook above: the hook decides what a
        // panic prints, this decides that the panic does not escape the thread
        // before the value is sent. Without it a panic in `decide` still
        // disconnects the channel, so the outcome is the same, but only by
        // way of a path that is harder to reason about than an explicit one.
        let outcome =
            std::panic::catch_unwind(decide).unwrap_or_else(|_| policy::Decision::internal_error());
        let _ = tx.send(outcome);
    });

    let decision = match rx.recv_timeout(deadline()) {
        Ok(d) => d,
        // Timed out, or the worker panicked and dropped the sender. Both are
        // "the guard could not answer", which is never a reason to block.
        Err(_) => policy::Decision::timed_out(),
    };

    if explain {
        // stderr, always: stdout is the decision channel, and a hook that
        // prints anything else there is a hook whose JSON does not parse.
        eprintln!("travsr guard: {}", decision.why);
    }
    let rendered = decision.output.render();
    if !rendered.is_empty() {
        // Written rather than `println!`ed, and the result dropped. `println!`
        // panics when stdout is a closed pipe, and the panic hook installed
        // above deliberately does not exit the way the CLI's own hook does, so
        // that panic would unwind out of here and take the process down with a
        // non-zero status. A host that has stopped reading has already decided
        // it does not want the decision; losing it is not worth an abort.
        let _ = std::io::stdout().write_all(rendered.as_bytes());
        let _ = std::io::stdout().write_all(b"\n");
    }
    Ok(())
}

/// Read the payload and decide. Runs on the worker thread, under the deadline.
fn decide() -> policy::Decision {
    let Some(input) = read_payload() else {
        return policy::Decision::unreadable_payload();
    };
    let repo_root = repo_root_for(&input);
    let mode = resolve_mode(repo_root.as_deref());
    policy::decide(&input, mode, repo_root.as_deref())
}

/// Parse stdin, or `None` for an empty, oversized or malformed payload.
fn read_payload() -> Option<HookInput> {
    let mut text = String::new();
    // `take` bounds the read, so a host that never closes the pipe costs the
    // deadline rather than the machine's memory.
    std::io::stdin()
        .lock()
        .take(MAX_PAYLOAD_BYTES)
        .read_to_string(&mut text)
        .ok()?;
    serde_json::from_str(&text).ok()
}

/// The repository the payload's `cwd` belongs to.
///
/// Falls back to the guard's own working directory when the payload carries no
/// `cwd`, and to `None` when neither is inside a repository, which is a
/// fail-open condition, since a repo with no `.travsr` has no graph to redirect
/// to either.
fn repo_root_for(input: &HookInput) -> Option<PathBuf> {
    // The host documents `cwd` as absolute. Anchoring a relative one to the
    // guard's own working directory rather than trusting it is what keeps a
    // relative payload from resolving a repo root that no file path can then
    // be made relative to.
    let start = match input.cwd.as_deref().map(PathBuf::from) {
        Some(p) if p.is_absolute() => p,
        Some(p) => std::env::current_dir().ok()?.join(p),
        None => std::env::current_dir().ok()?,
    };
    // The read resolver: a linked worktree indexed by its own checkout keeps
    // its own graph, and one that is not falls back to the main worktree's,
    // which is the index the agent's MCP session is reading from too.
    crate::repo::find_git_root(&start).ok()
}

#[cfg(test)]
mod tests {
    use super::payload::HookOutput;
    use super::*;

    #[test]
    fn modes_round_trip_through_their_stored_spelling() {
        for m in [GuardMode::Off, GuardMode::Advisory, GuardMode::Strict] {
            assert_eq!(GuardMode::parse(m.as_str()), m);
        }
    }

    #[test]
    fn an_unrecognised_stored_mode_falls_back_to_off() {
        for raw in ["", "  ", "on", "yes", "STRICTER", "1", "blocking"] {
            assert_eq!(
                GuardMode::parse(raw),
                GuardMode::Off,
                "'{raw}' must not be read as an enforcing mode"
            );
        }
    }

    #[test]
    fn stored_modes_are_case_and_whitespace_insensitive() {
        assert_eq!(GuardMode::parse("  Strict "), GuardMode::Strict);
        assert_eq!(GuardMode::parse("ADVISORY"), GuardMode::Advisory);
    }

    /// The three spellings the config key validates and the three this parses
    /// have to be the same set, or a value `travsr config set` accepts would
    /// read back as `off` and the guard would be silently inert.
    #[test]
    fn every_validated_config_value_parses_to_a_real_mode() {
        for v in travsr_config::GUARD_MODES {
            let parsed = GuardMode::parse(v);
            assert_eq!(
                parsed.as_str(),
                *v,
                "config accepts '{v}' but the guard reads it as {parsed:?}"
            );
        }
        assert_eq!(
            travsr_config::DEFAULT_GUARD_MODE,
            GuardMode::Off.as_str(),
            "the guard is opt-in; the registry default has to say so too"
        );
    }

    /// The deadline has to be enforced, not assumed. Asserted on the mechanism
    /// rather than on a slow database, because a test that manufactures real
    /// SQLite contention is a race.
    #[test]
    fn the_deadline_is_enforced_by_the_receiver_not_by_hope() {
        let (tx, rx) = mpsc::channel::<HookOutput>();
        let started = std::time::Instant::now();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(30));
            let _ = tx.send(HookOutput::deny("far too late"));
        });
        let decision = match rx.recv_timeout(GUARD_DEADLINE) {
            Ok(d) => d,
            Err(_) => HookOutput::Neutral,
        };
        assert!(!decision.blocks(), "a missed deadline must never block");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the guard waited {:?}, well past its {GUARD_DEADLINE:?} budget",
            started.elapsed()
        );
    }

    /// The override exists so the bound can be driven from both ends, and it is
    /// clamped so that driving it cannot remove the bound.
    #[test]
    fn the_deadline_override_is_clamped_in_both_directions() {
        let check = |raw: Option<&str>, want: Duration, why: &str| {
            assert_eq!(decide_deadline(raw), want, "{why}");
        };
        check(None, GUARD_DEADLINE, "unset means the documented budget");
        check(
            Some("500"),
            Duration::from_millis(500),
            "a value is honoured",
        );
        check(Some("  500 "), Duration::from_millis(500), "and trimmed");
        check(
            Some("0"),
            Duration::from_millis(1),
            "zero cannot disable it",
        );
        check(
            Some("999999999"),
            DEADLINE_CEILING,
            "and neither can a very large value",
        );
        check(
            Some("soon"),
            GUARD_DEADLINE,
            "a typo falls back, it does not panic",
        );
        check(Some(""), GUARD_DEADLINE, "and so does an empty value");
    }

    /// A panic inside the decision must reach the main thread as "no answer",
    /// never as a block and never as a process that dies before it prints.
    #[test]
    fn a_panicking_decision_is_not_a_block() {
        let (tx, rx) = mpsc::channel::<HookOutput>();
        std::thread::spawn(move || {
            let prior = std::panic::take_hook();
            std::panic::set_hook(Box::new(|_| {}));
            let outcome = std::panic::catch_unwind(|| -> HookOutput {
                panic!("graph exploded");
            })
            .unwrap_or(HookOutput::Neutral);
            std::panic::set_hook(prior);
            let _ = tx.send(outcome);
        });
        let decision = rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or(HookOutput::Neutral);
        assert_eq!(decision, HookOutput::Neutral);
        assert!(!decision.blocks());
    }
}
