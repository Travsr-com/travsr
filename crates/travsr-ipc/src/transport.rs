use crate::{ControlMessage, ControlResponse};

/// Sync client transport for the daemon control plane.
///
/// Implementors: [`crate::unix::UnixTransport`] (Unix) and
/// [`crate::windows::NamedPipeTransport`] (Windows).
/// Business logic in the daemon CLI always speaks `ControlTransport` — no
/// `#[cfg]` appears outside the implementing modules.
pub trait ControlTransport {
    fn send_request(&mut self, msg: &ControlMessage) -> anyhow::Result<ControlResponse>;
}
