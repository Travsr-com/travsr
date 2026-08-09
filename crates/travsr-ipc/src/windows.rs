use crate::{ControlAddr, ControlMessage, ControlResponse, ControlTransport};

/// Synchronous Windows Named Pipe client for the daemon control plane.
///
/// Opens `\\.\pipe\travsr-<hex>` as a blocking file handle. Windows allows
/// synchronous client I/O on a named pipe even when the server uses overlapped
/// (async) I/O — the client side does not need FILE_FLAG_OVERLAPPED.
pub struct NamedPipeTransport {
    file: std::fs::File,
}

impl NamedPipeTransport {
    /// Connect to the daemon's named pipe at `addr.pipe_name()`.
    /// Returns `Err` if no daemon is listening.
    pub fn connect(addr: &ControlAddr) -> anyhow::Result<Self> {
        let pipe_name = addr.pipe_name();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&pipe_name)
            .map_err(|e| anyhow::anyhow!("daemon not running ({}): {e}", pipe_name))?;
        Ok(Self { file })
    }
}

impl ControlTransport for NamedPipeTransport {
    fn send_request(&mut self, msg: &ControlMessage) -> anyhow::Result<ControlResponse> {
        // #541: shares the retrying helper with the Unix transport. Named-pipe
        // client I/O here is blocking, so the would-block retries should not
        // fire — but the partial-write resume and the deadline-shaped error
        // message apply to both, and the daemon-lifecycle races tracked for
        // Windows in #500/#503 are the same class of bug.
        crate::transport::send_request_line(&mut self.file, msg)
    }
}
