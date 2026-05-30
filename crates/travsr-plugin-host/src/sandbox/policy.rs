use thiserror::Error;

/// ADR-017 normative sandbox policy enum.
#[derive(Debug, Clone)]
pub enum SandboxPolicy {
    /// Default — applied to every Sidecar spawn.
    Standard,
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
        if let Self::Elevated {
            reason,
            approved_by,
            ..
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
        }
        Ok(())
    }
}

/// ADR-017 Rule 2: no unsandboxed fallback. If the sandbox cannot be applied,
/// the plugin is DISABLED. This error surfaces that condition.
#[derive(Debug, Error)]
#[error("sandbox unavailable (plugin disabled per ADR-017 Rule 2): {0}")]
pub struct SandboxUnavailable(pub String);
