use thiserror::Error;

/// ADR-018 Rule 1 — normative resource block applied to every plugin process
/// and to the verification probe. Enforcement mechanism (Rule 2):
/// - `PARSE_DEADLINE` / probe deadline: host-side timer in the transport /
///   probe, `SIGKILL` on breach (works identically on every platform).
/// - `OUTPUT_MAX`: frame cap on the decoded `ParseResponse` (rejected before
///   allocation — T16).
/// - `MEMORY_MAX` (kill) / `MEMORY_HIGH` (recycle trigger): supervisor RSS
///   watchdog sampling `/proc/<pid>/statm` on Linux and `ps -o rss=` on macOS
///   (the ADR's documented sampled-enforcement path; a sub-interval spike can
///   transiently overshoot — accepted).
/// - `RECYCLE_AFTER_FILES` / `IDLE_TTL`: the LazySidecar transport drains and
///   respawns at the boundary; respawn reuses the cached verdict (no
///   re-probe, ADR-018 Rule 3).
/// - cgroup `memory.max` / `RLIMIT_AS` hard caps and cgroup `cpu.max` are a
///   deferred hardening layer on top of the watchdog — the process boundary
///   already insulates the daemon.
pub mod resource_limits {
    use std::time::Duration;

    /// Per plugin process RSS: kill threshold.
    pub const MEMORY_MAX_BYTES: u64 = 1024 * 1024 * 1024;
    /// Per plugin process RSS: graceful recycle threshold (drain + respawn).
    pub const MEMORY_HIGH_BYTES: u64 = 768 * 1024 * 1024;
    /// Wall-clock per ParseRequest (host-side).
    pub const PARSE_DEADLINE: Duration = Duration::from_secs(30);
    /// Cap on a decoded ParseResponse frame.
    pub const OUTPUT_MAX_BYTES: usize = 64 * 1024 * 1024;
    /// Recycle after this many files even without memory pressure.
    pub const RECYCLE_AFTER_FILES: u32 = 2000;
    /// No in-flight files for this long → spin the process down.
    pub const IDLE_TTL: Duration = Duration::from_secs(120);
    /// RSS/idle sampling interval of the supervisor watchdog.
    pub const WATCHDOG_INTERVAL: Duration = Duration::from_secs(1);
    /// Consecutive parse crashes before the language is disabled for the run.
    pub const MAX_CONSECUTIVE_CRASHES: u32 = 3;
}

/// ADR-017 normative sandbox policy enum.
#[derive(Debug, Clone)]
pub enum SandboxPolicy {
    /// Default — applied to every Sidecar spawn.
    Standard,
    /// For tools that require POSIX IPC (shm_open/mq_open) but not network access.
    /// On macOS, sandbox-exec Seatbelt has no valid operation to permit mq_open, so
    /// this policy skips sandbox-exec entirely and relies on ulimit resource caps.
    /// On Linux, bwrap is used with --unshare-ipc omitted. No PSE approval required.
    NativeIpc,
    /// Exception variant — requires PSE sign-off before implementation PR merges.
    Elevated {
        /// Explicit allowlist of hosts. No wildcards. No CIDR ranges.
        permitted_hosts: Vec<String>,
        /// Required one-sentence justification. Empty string rejected at parse time.
        reason: String,
        /// PSE GitHub handle who approved. Self-approval forbidden.
        approved_by: String,
        /// ISO-8601 date. Re-review required after 12 months.
        approved_date: String,
    },
}

impl SandboxPolicy {
    pub fn validate(&self) -> Result<(), SandboxUnavailable> {
        if let Self::NativeIpc = self {
            return Ok(());
        }
        if let Self::Elevated {
            permitted_hosts,
            reason,
            approved_by,
            approved_date,
        } = self
        {
            if reason.is_empty() {
                return Err(SandboxUnavailable(
                    "Elevated.reason must not be empty".into(),
                ));
            }
            if approved_by.is_empty() {
                return Err(SandboxUnavailable(
                    "Elevated.approved_by must not be empty".into(),
                ));
            }
            if approved_date.is_empty() {
                return Err(SandboxUnavailable(
                    "Elevated.approved_date must not be empty (ISO-8601; re-review after 12 months)"
                        .into(),
                ));
            }
            if permitted_hosts.is_empty() {
                return Err(SandboxUnavailable(
                    "Elevated.permitted_hosts must list at least one explicit host".into(),
                ));
            }
            for host in permitted_hosts {
                validate_permitted_host(host).map_err(SandboxUnavailable)?;
            }
        }
        Ok(())
    }
}

/// Validate a single `permitted_hosts` entry per ADR-017 Rule 1:
/// "explicit allowlist, no wildcards, no CIDR". Rejects empty strings,
/// wildcards (`*`), CIDR/path slashes (`/`), URL schemes or ports (`:`),
/// and embedded whitespace. A full network-disable (`Elevated` cannot widen
/// to all hosts) is therefore impossible to express here — escalate to CTO.
pub fn validate_permitted_host(host: &str) -> Result<(), String> {
    let h = host.trim();
    if h.is_empty() {
        return Err("permitted host must not be empty".into());
    }
    if h != host {
        return Err(format!(
            "permitted host '{host}' must not have surrounding whitespace"
        ));
    }
    if h.contains('*') {
        return Err(format!(
            "permitted host '{host}' must not contain a wildcard '*'"
        ));
    }
    if h.contains('/') {
        return Err(format!(
            "permitted host '{host}' must be a bare hostname — no CIDR ranges, paths, or schemes"
        ));
    }
    if h.contains(':') {
        return Err(format!(
            "permitted host '{host}' must not include a scheme or port"
        ));
    }
    if h.chars().any(char::is_whitespace) {
        return Err(format!(
            "permitted host '{host}' must not contain whitespace"
        ));
    }
    Ok(())
}

/// ADR-017 Rule 2: no unsandboxed fallback. If the sandbox cannot be applied,
/// the plugin is DISABLED. This error surfaces that condition.
#[derive(Debug, Error)]
#[error("sandbox unavailable (plugin disabled per ADR-017 Rule 2): {0}")]
pub struct SandboxUnavailable(pub String);
