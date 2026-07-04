use crate::types::{GoldenFixture, InvokeRequest, InvokeResponse, ParseRequest, ParseResponse};
use travsr_core::LanguageId;

/// Implemented once per language. Stateless and Send+Sync so a single instance
/// is shared across walker threads (in-process) or drives one child process (sidecar).
pub trait Plugin: Send + Sync {
    /// The language this plugin provides (RFC-013 D-1: open identity).
    ///
    /// Returns a validated [`LanguageId`], NOT the closed `Language` enum —
    /// this is the load-bearing boundary change that lets a plugin declare a
    /// language core never enumerated. Builtin plugins convert via
    /// `Language::X.into()`.
    fn language(&self) -> LanguageId;
    fn extensions(&self) -> &[&str];
    fn supports_phase_b(&self) -> bool {
        false
    }
    /// Golden fixture for the host's conformance probe (ADR-019 Rule 2).
    ///
    /// The plugin ships a small representative source file; the host asserts
    /// structural validity + determinism over it — never content. `None` means
    /// the probe falls back to an empty file (weaker: only trivial output is
    /// verified).
    fn golden_fixture(&self) -> Option<GoldenFixture> {
        None
    }
    fn parse(&self, req: &ParseRequest) -> ParseResponse;
    fn invoke_phase_b(&self, _req: &InvokeRequest) -> InvokeResponse {
        InvokeResponse::unsupported()
    }
}
