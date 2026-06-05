use crate::{ControlAddr, ControlMessage, ControlResponse, ControlTransport};

/// Windows Named Pipe client — stub pending RFC-013 implementation.
///
/// All methods return `Err` until the WS4 PR lands the actual
/// `tokio::net::windows::named_pipe` implementation.
pub struct NamedPipeTransport {
    _addr: ControlAddr,
}

impl NamedPipeTransport {
    pub fn connect(addr: ControlAddr) -> anyhow::Result<Self> {
        let _ = addr.pipe_name(); // ensure it compiles
        anyhow::bail!("Windows Named Pipe transport not yet implemented (RFC-013 pending)")
    }
}

impl ControlTransport for NamedPipeTransport {
    fn send_request(&mut self, _msg: &ControlMessage) -> anyhow::Result<ControlResponse> {
        anyhow::bail!("Windows Named Pipe transport not yet implemented (RFC-013 pending)")
    }
}
