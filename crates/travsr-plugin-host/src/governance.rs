//! Embedding reindex resource governance (WS2 of the reindex-governance epic, #419).
//!
//! Resolves the user's CPU knobs — **capacity** (a percentage governor),
//! **max_workers** (an absolute cap), and **priority** (OS scheduling) — from the
//! layered `travsr-config` sources and turns them into the concrete inputs the
//! reindex spawn path consumes: a worker count and a child-process priority.
//!
//! Governance is orthogonal to correctness (epic invariant INV-A): these knobs
//! change reindex *speed only*, never which nodes are embedded or the vectors
//! produced.
//!
//! Precedence (via [`travsr_config::resolve`]): CLI > env > per-repo > global >
//! default. On Unix a lowered priority wraps the sidecar with `nice`; on Windows
//! it sets the process priority class via safe `creation_flags` (this crate is
//! `deny(unsafe_code)`, so no `pre_exec`).

use std::path::Path;
use std::process::Command;

use travsr_config::{resolve, Resolved};

/// OS scheduling priority for the embed sidecar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Priority {
    #[default]
    Normal,
    Low,
    Idle,
}

impl Priority {
    /// Parse the canonical config/CLI spelling. `None` on anything else.
    pub fn parse(s: &str) -> Option<Priority> {
        match s.trim() {
            "normal" => Some(Priority::Normal),
            "low" => Some(Priority::Low),
            "idle" => Some(Priority::Idle),
            _ => None,
        }
    }

    /// Canonical spelling (matches the `travsr-config` validator).
    pub fn as_str(self) -> &'static str {
        match self {
            Priority::Normal => "normal",
            Priority::Low => "low",
            Priority::Idle => "idle",
        }
    }

    /// `nice(1)` increment for this priority (higher = lower scheduling priority).
    #[cfg(unix)]
    fn nice_delta(self) -> i32 {
        match self {
            Priority::Normal => 0,
            Priority::Low => 10,
            Priority::Idle => 19,
        }
    }

    /// Win32 `PROCESS_CREATION_FLAGS` priority class. Hardcoded stable constants
    /// (avoids a windows-sys import here); consumed by safe `creation_flags`.
    #[cfg(windows)]
    fn win_priority_class(self) -> u32 {
        match self {
            Priority::Normal => 0x0000_0020, // NORMAL_PRIORITY_CLASS
            Priority::Low => 0x0000_4000,    // BELOW_NORMAL_PRIORITY_CLASS
            Priority::Idle => 0x0000_0040,   // IDLE_PRIORITY_CLASS
        }
    }

    /// Build the base [`Command`] for spawning the reindex sidecar at this
    /// priority. The caller appends the sidecar's own args afterwards.
    ///
    /// - Unix `low`/`idle`: wraps with `nice -n <delta> <bin>`. `nice` `execvp`s
    ///   the target, so the child PID (used by the shutdown killer) is preserved.
    /// - Windows `low`/`idle`: sets the priority class via safe `creation_flags`.
    /// - `normal` (either OS): spawns the binary directly, unchanged.
    pub fn reindex_command(self, bin_path: &Path) -> Command {
        #[cfg(unix)]
        {
            if self == Priority::Normal {
                Command::new(bin_path)
            } else {
                let mut c = Command::new("nice");
                c.arg("-n").arg(self.nice_delta().to_string()).arg(bin_path);
                c
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            let mut c = Command::new(bin_path);
            if self != Priority::Normal {
                c.creation_flags(self.win_priority_class());
            }
            c
        }
        #[cfg(not(any(unix, windows)))]
        {
            Command::new(bin_path)
        }
    }
}

/// One-shot CLI overrides for a single reindex invocation. All `None` for daemon
/// background passes (which take config + env + default only).
#[derive(Debug, Clone, Copy, Default)]
pub struct EmbedOverrides {
    /// `--capacity <1-100>` percent.
    pub capacity: Option<u8>,
    /// `-j/--jobs <N>` absolute worker count.
    pub max_workers: Option<usize>,
    /// `--priority normal|low|idle`.
    pub priority: Option<Priority>,
}

/// The resolved governance for a reindex: each knob plus the layer that set it
/// (for source-attributed status, epic goal G1).
#[derive(Debug, Clone, Copy)]
pub struct EmbedGovernance {
    /// Percent of derived workers to use (1-100).
    pub capacity: Resolved<u8>,
    /// Absolute worker cap; `None` means "derive from CPU/RAM".
    pub max_workers: Resolved<Option<usize>>,
    /// Sidecar scheduling priority.
    pub priority: Resolved<Priority>,
}

impl EmbedGovernance {
    /// Capacity as a `[0.01, 1.0]` fraction for the worker-count math.
    pub fn capacity_fraction(self) -> f32 {
        (self.capacity.value as f32 / 100.0).clamp(0.01, 1.0)
    }
}

/// Resolve embed governance for `repo_root` (per-repo config layer) combined with
/// the process env, global config, and one-shot CLI `overrides`.
///
/// Reads each layer at the edges and defers precedence to the pure
/// [`travsr_config::resolve`], so the layering is identical to every other key.
pub fn resolve_embed(repo_root: Option<&Path>, overrides: &EmbedOverrides) -> EmbedGovernance {
    let repo_file = repo_root.map(travsr_config::repo_path);
    let global_file = travsr_config::global_path();

    // capacity: u8, default 100.
    let capacity = resolve(
        overrides.capacity,
        env_u8("TRAVSR_EMBED_CAPACITY"),
        file_parse(repo_file.as_deref(), "embed.capacity", parse_u8),
        file_parse(global_file.as_deref(), "embed.capacity", parse_u8),
        100u8,
    );

    // max_workers: Option<usize>, default None ("no cap → derive"). The value
    // type is itself an Option, so each present layer contributes Some(Some(n)).
    let max_workers = resolve(
        overrides.max_workers.map(Some),
        env_usize("TRAVSR_EMBED_WORKERS").map(Some),
        file_parse(repo_file.as_deref(), "embed.max_workers", parse_usize).map(Some),
        file_parse(global_file.as_deref(), "embed.max_workers", parse_usize).map(Some),
        None,
    );

    // priority: Priority, default Normal.
    let priority = resolve(
        overrides.priority,
        std::env::var("TRAVSR_EMBED_PRIORITY")
            .ok()
            .and_then(|s| Priority::parse(&s)),
        file_parse(repo_file.as_deref(), "embed.priority", Priority::parse),
        file_parse(global_file.as_deref(), "embed.priority", Priority::parse),
        Priority::Normal,
    );

    EmbedGovernance {
        capacity,
        max_workers,
        priority,
    }
}

// ── layer readers (I/O at the edges) ─────────────────────────────────────────

fn file_parse<T>(path: Option<&Path>, key: &str, parse: impl Fn(&str) -> Option<T>) -> Option<T> {
    let raw = travsr_config::read_key_file(path?, key)?;
    parse(&raw)
}

fn env_u8(var: &str) -> Option<u8> {
    std::env::var(var).ok().and_then(|s| parse_u8(&s))
}

fn env_usize(var: &str) -> Option<usize> {
    std::env::var(var).ok().and_then(|s| parse_usize(&s))
}

fn parse_u8(s: &str) -> Option<u8> {
    let n: u32 = s.trim().parse().ok()?;
    (1..=100).contains(&n).then_some(n as u8)
}

fn parse_usize(s: &str) -> Option<usize> {
    let n: usize = s.trim().parse().ok()?;
    (n >= 1).then_some(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use travsr_config::Source;

    #[test]
    fn priority_round_trip() {
        for p in [Priority::Normal, Priority::Low, Priority::Idle] {
            assert_eq!(Priority::parse(p.as_str()), Some(p));
        }
        assert_eq!(Priority::parse("high"), None);
    }

    #[test]
    fn capacity_fraction_clamps() {
        let g = EmbedGovernance {
            capacity: Resolved {
                value: 50,
                source: Source::Default,
            },
            max_workers: Resolved {
                value: None,
                source: Source::Default,
            },
            priority: Resolved {
                value: Priority::Normal,
                source: Source::Default,
            },
        };
        assert!((g.capacity_fraction() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn parse_helpers_reject_out_of_range() {
        assert_eq!(parse_u8("50"), Some(50));
        assert_eq!(parse_u8("0"), None);
        assert_eq!(parse_u8("101"), None);
        assert_eq!(parse_usize("4"), Some(4));
        assert_eq!(parse_usize("0"), None);
    }

    #[test]
    fn resolve_default_when_all_unset() {
        // No repo_root, and (in a clean env) no env vars → defaults.
        // Guard against a developer env that sets these.
        if std::env::var("TRAVSR_EMBED_CAPACITY").is_ok()
            || std::env::var("TRAVSR_EMBED_PRIORITY").is_ok()
        {
            return;
        }
        let g = resolve_embed(None, &EmbedOverrides::default());
        assert_eq!(g.capacity.value, 100);
        assert_eq!(g.priority.value, Priority::Normal);
    }

    #[test]
    fn cli_override_wins_over_default() {
        let over = EmbedOverrides {
            capacity: Some(25),
            max_workers: Some(3),
            priority: Some(Priority::Idle),
        };
        let g = resolve_embed(None, &over);
        assert_eq!(g.capacity.value, 25);
        assert_eq!(g.capacity.source, Source::Cli);
        assert_eq!(g.max_workers.value, Some(3));
        assert_eq!(g.priority.value, Priority::Idle);
    }
}
