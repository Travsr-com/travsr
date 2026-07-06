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

/// The capacity governor: either an explicit percentage or the adaptive `auto`
/// sentinel (WS5). `auto` is resolved to a load-adjusted fraction at each spawn
/// via [`auto_capacity_fraction`]; `Percent(p)` is the static `p/100` fraction.
///
/// Governance is orthogonal to correctness (INV-A): `auto` and any percent embed
/// the exact same node set and vectors — only the worker count (speed) differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capacity {
    /// Load-adaptive: leave idle-core headroom for interactive work.
    Auto,
    /// Fixed percent of the derived worker count (1-100).
    Percent(u8),
}

impl Default for Capacity {
    fn default() -> Self {
        Capacity::Percent(100)
    }
}

impl Capacity {
    /// Parse the canonical config/CLI spelling: `auto` or an integer 1-100.
    /// `None` on anything else (mirrors the `travsr-config` validator).
    pub fn parse(s: &str) -> Option<Capacity> {
        let t = s.trim();
        if t.eq_ignore_ascii_case("auto") {
            return Some(Capacity::Auto);
        }
        let n: u32 = t.parse().ok()?;
        (1..=100).contains(&n).then_some(Capacity::Percent(n as u8))
    }

    /// Canonical stored/CLI spelling (`auto` or the bare percent).
    pub fn to_config_value(self) -> String {
        match self {
            Capacity::Auto => "auto".to_string(),
            Capacity::Percent(p) => p.to_string(),
        }
    }

    /// Short human label for status output (`auto` or `NN%`).
    pub fn label(self) -> String {
        match self {
            Capacity::Auto => "auto".to_string(),
            Capacity::Percent(p) => format!("{p}%"),
        }
    }
}

/// Best-effort `[0.25, 1.0]` fraction of the derived worker count for `auto`,
/// scaled by current CPU idleness so a busy machine yields fewer workers and an
/// idle one yields the full derived count (WS5). RAM headroom is already enforced
/// downstream in `derive_num_workers_inner`, so this governs the CPU dimension.
///
/// Sampled fresh at each spawn — the daemon spawns per phase (Phase1/Phase2/
/// background), so adaptivity happens at those natural boundaries with no mid-run
/// thrash. Unsafe-free and portable: Linux `/proc/loadavg`, macOS `sysctl
/// vm.loadavg`; where load is unknown (Windows / sandbox) it falls back to a
/// conservative half.
pub fn auto_capacity_fraction() -> f32 {
    const FLOOR: f32 = 0.25;
    const UNKNOWN: f32 = 0.5;
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .max(1) as f32;
    match one_minute_load() {
        Some(load1) => {
            // Idle cores as a fraction of the machine; never below the floor so
            // auto always makes progress, never above 1.0 (governor reduce-only).
            (((cores - load1) / cores).clamp(FLOOR, 1.0)).min(1.0)
        }
        None => UNKNOWN,
    }
}

/// Best-effort 1-minute load average. `None` where the platform has no cheap,
/// unsafe-free way to read it (Windows, restricted sandboxes).
fn one_minute_load() -> Option<f32> {
    // A `#[cfg]`-gated `let` (not a bare block) so each arm is a tail expression
    // without a `return` — bare `#[cfg]` blocks in statement position evaluate to
    // `()` and would drop the value.
    #[cfg(target_os = "linux")]
    let load = {
        let text = std::fs::read_to_string("/proc/loadavg").ok()?;
        text.split_whitespace().next()?.parse().ok()
    };
    #[cfg(target_os = "macos")]
    let load = {
        // `sysctl -n vm.loadavg` → "{ 1.85 2.01 2.10 }".
        let out = Command::new("sysctl")
            .args(["-n", "vm.loadavg"])
            .output()
            .ok()?;
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout);
            text.split_whitespace()
                .find(|t| t.parse::<f32>().is_ok())
                .and_then(|t| t.parse().ok())
        } else {
            None
        }
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let load: Option<f32> = None;
    load
}

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
    /// `--capacity <auto|1-100>`.
    pub capacity: Option<Capacity>,
    /// `-j/--jobs <N>` absolute worker count.
    pub max_workers: Option<usize>,
    /// `--priority normal|low|idle`.
    pub priority: Option<Priority>,
}

/// The resolved governance for a reindex: each knob plus the layer that set it
/// (for source-attributed status, epic goal G1).
#[derive(Debug, Clone, Copy)]
pub struct EmbedGovernance {
    /// Worker budget: `auto` (load-adaptive) or an explicit percent.
    pub capacity: Resolved<Capacity>,
    /// Absolute worker cap; `None` means "derive from CPU/RAM".
    pub max_workers: Resolved<Option<usize>>,
    /// Sidecar scheduling priority.
    pub priority: Resolved<Priority>,
}

impl EmbedGovernance {
    /// Capacity as a `[0.01, 1.0]` fraction for the worker-count math.
    ///
    /// For [`Capacity::Auto`] this samples current CPU load, so it is evaluated
    /// once per spawn (not a pure/const value like the percent case).
    pub fn capacity_fraction(self) -> f32 {
        match self.capacity.value {
            Capacity::Auto => auto_capacity_fraction(),
            Capacity::Percent(p) => (p as f32 / 100.0).clamp(0.01, 1.0),
        }
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

    // capacity: Capacity, default Percent(100).
    let capacity = resolve(
        overrides.capacity,
        env_capacity("TRAVSR_EMBED_CAPACITY"),
        file_parse(repo_file.as_deref(), "embed.capacity", Capacity::parse),
        file_parse(global_file.as_deref(), "embed.capacity", Capacity::parse),
        Capacity::default(),
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

fn env_capacity(var: &str) -> Option<Capacity> {
    std::env::var(var).ok().and_then(|s| Capacity::parse(&s))
}

fn env_usize(var: &str) -> Option<usize> {
    std::env::var(var).ok().and_then(|s| parse_usize(&s))
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

    fn gov(capacity: Capacity) -> EmbedGovernance {
        EmbedGovernance {
            capacity: Resolved {
                value: capacity,
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
        }
    }

    #[test]
    fn capacity_fraction_clamps() {
        assert!((gov(Capacity::Percent(50)).capacity_fraction() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn capacity_parse_auto_and_percent() {
        assert_eq!(Capacity::parse("auto"), Some(Capacity::Auto));
        assert_eq!(Capacity::parse("AUTO"), Some(Capacity::Auto));
        assert_eq!(Capacity::parse("50"), Some(Capacity::Percent(50)));
        assert_eq!(Capacity::parse("100"), Some(Capacity::Percent(100)));
        assert_eq!(Capacity::parse("0"), None);
        assert_eq!(Capacity::parse("101"), None);
        assert_eq!(Capacity::parse("high"), None);
        assert_eq!(Capacity::Auto.to_config_value(), "auto");
        assert_eq!(Capacity::Percent(25).to_config_value(), "25");
        assert_eq!(Capacity::Percent(25).label(), "25%");
    }

    #[test]
    fn auto_capacity_fraction_stays_in_band() {
        // Whatever the machine load, auto never oversubscribes and never starves.
        let f = gov(Capacity::Auto).capacity_fraction();
        assert!((0.25..=1.0).contains(&f), "auto fraction {f} out of band");
    }

    #[test]
    fn parse_helpers_reject_out_of_range() {
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
        // Skip if the global config file exists — it may set embed.* keys that
        // shadow the defaults under test. CI runs in a clean home dir so this
        // guard never fires there; on developer machines it prevents false failures.
        if travsr_config::global_path().is_some_and(|p| p.exists()) {
            return;
        }
        let g = resolve_embed(None, &EmbedOverrides::default());
        assert_eq!(g.capacity.value, Capacity::Percent(100));
        assert_eq!(g.priority.value, Priority::Normal);
    }

    #[test]
    fn cli_override_wins_over_default() {
        let over = EmbedOverrides {
            capacity: Some(Capacity::Percent(25)),
            max_workers: Some(3),
            priority: Some(Priority::Idle),
        };
        let g = resolve_embed(None, &over);
        assert_eq!(g.capacity.value, Capacity::Percent(25));
        assert_eq!(g.capacity.source, Source::Cli);
        assert_eq!(g.max_workers.value, Some(3));
        assert_eq!(g.priority.value, Priority::Idle);
    }
}
