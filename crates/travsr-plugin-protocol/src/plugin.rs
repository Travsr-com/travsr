use travsr_core::Language;
use crate::types::{ParseRequest, ParseResponse, InvokeRequest, InvokeResponse};

/// Implemented once per language. Stateless and Send+Sync so a single instance
/// is shared across walker threads (in-process) or drives one child process (sidecar).
pub trait Plugin: Send + Sync {
    fn language(&self) -> Language;
    fn extensions(&self) -> &[&str];
    fn supports_phase_b(&self) -> bool { false }
    fn parse(&self, req: &ParseRequest) -> ParseResponse;
    fn invoke_phase_b(&self, _req: &InvokeRequest) -> InvokeResponse {
        InvokeResponse::unsupported()
    }
}
