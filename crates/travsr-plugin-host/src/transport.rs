use std::sync::Arc;
use travsr_error::IndexError;
use travsr_plugin_protocol::{Plugin, ParseRequest, ParseResponse, InvokeRequest, InvokeResponse};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginHealth {
    Ok,
    Degraded(String),
    Disabled(String),
}

pub trait Transport: Send + Sync {
    fn parse(&self, req: ParseRequest) -> Result<ParseResponse, IndexError>;
    /// InProcess: MUST return Err(IndexError::PhaseNotSupported).
    /// Sidecar: sends InvokeRequest over the wire.
    fn invoke_phase_b(&self, req: InvokeRequest) -> Result<InvokeResponse, IndexError>;
    fn health(&self) -> PluginHealth;
}

/// Zero-IPC. Calls plugin directly in the daemon's address space.
/// PERMITTED ONLY for first-party, pinned, fuzzed, fixture-gated Phase A grammars.
/// NEVER for Phase B. NEVER for --command community plugins. (ADR-017 Rule 4)
pub struct InProcess {
    plugin: Arc<dyn Plugin>,
}

impl InProcess {
    pub fn new(plugin: impl Plugin + 'static) -> Self {
        Self { plugin: Arc::new(plugin) }
    }
}

impl Transport for InProcess {
    fn parse(&self, req: ParseRequest) -> Result<ParseResponse, IndexError> {
        Ok(self.plugin.parse(&req))
    }

    /// Normative (RFC-011 §2): MUST return PhaseNotSupported.
    /// Returning Ok would silently mask an InProcess/PhaseB misconfiguration.
    fn invoke_phase_b(&self, _req: InvokeRequest) -> Result<InvokeResponse, IndexError> {
        Err(IndexError::PhaseNotSupported)
    }

    fn health(&self) -> PluginHealth { PluginHealth::Ok }
}

/// Subprocess transport. Spawns under ADR-017 SandboxPolicy::Standard.
/// P5-S1: skeleton only — real subprocess spawn + IPC lands in P5-S3.
pub struct Sidecar {
    #[allow(dead_code)] // language tag reserved for P5-S3 IPC handshake
    language: String,
    health: PluginHealth,
}

impl Sidecar {
    pub fn stub(language: impl Into<String>) -> Self {
        Self {
            language: language.into(),
            health: PluginHealth::Disabled("Sidecar spawn not yet implemented (P5-S3)".into()),
        }
    }
}

impl Transport for Sidecar {
    fn parse(&self, _req: ParseRequest) -> Result<ParseResponse, IndexError> {
        Err(IndexError::PhaseNotSupported)
    }
    fn invoke_phase_b(&self, _req: InvokeRequest) -> Result<InvokeResponse, IndexError> {
        Err(IndexError::PhaseNotSupported)
    }
    fn health(&self) -> PluginHealth { self.health.clone() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use travsr_plugin_protocol::{ParseResponse, InvokeResponse};

    struct NoOpPlugin;
    impl Plugin for NoOpPlugin {
        fn language(&self) -> travsr_core::Language { travsr_core::Language::TypeScript }
        fn extensions(&self) -> &[&str] { &["ts"] }
        fn supports_phase_b(&self) -> bool { false }
        fn parse(&self, _req: &travsr_plugin_protocol::ParseRequest) -> ParseResponse {
            ParseResponse::default()
        }
        fn invoke_phase_b(&self, _req: &travsr_plugin_protocol::InvokeRequest) -> InvokeResponse {
            InvokeResponse::default()
        }
    }

    #[test]
    fn in_process_invoke_phase_b_returns_phase_not_supported() {
        let t = InProcess::new(NoOpPlugin);
        let req = InvokeRequest { root: std::path::PathBuf::from(".") };
        assert!(matches!(t.invoke_phase_b(req), Err(IndexError::PhaseNotSupported)));
    }

    #[test]
    fn sidecar_stub_is_disabled() {
        let s = Sidecar::stub("kotlin");
        assert!(matches!(s.health(), PluginHealth::Disabled(_)));
    }
}
