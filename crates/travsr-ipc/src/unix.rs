use std::path::Path;
use std::time::Duration;

use crate::{ControlAddr, ControlMessage, ControlResponse, ControlTransport};

/// Sync Unix domain socket client for the daemon control plane.
pub struct UnixTransport {
    stream: std::os::unix::net::UnixStream,
}

impl UnixTransport {
    /// Connect to the daemon socket at `addr.socket_path(travsr_dir)`.
    /// Returns `Err` if no daemon is listening.
    pub fn connect(addr: &ControlAddr, travsr_dir: &Path) -> anyhow::Result<Self> {
        let sock_path = addr.socket_path(travsr_dir);
        let stream = std::os::unix::net::UnixStream::connect(&sock_path)
            .map_err(|e| anyhow::anyhow!("daemon not running ({}): {e}", sock_path.display()))?;
        // #541: these are now the retry granularity, not the budget. When one
        // fires the OS reports EAGAIN (errno 35 on macOS/BSD) and
        // `send_request_line` re-checks its own deadline and tries again,
        // instead of the whole command dying on the first would-block. Kept
        // short so that re-check is responsive.
        stream.set_write_timeout(Some(Duration::from_millis(500)))?;
        stream.set_read_timeout(Some(Duration::from_millis(500)))?;
        Ok(Self { stream })
    }
}

impl ControlTransport for UnixTransport {
    fn send_request(&mut self, msg: &ControlMessage) -> anyhow::Result<ControlResponse> {
        // `&UnixStream` implements both Read and Write, so the shared helper
        // drives the socket without needing ownership.
        crate::transport::send_request_line(&mut &self.stream, msg)
    }
}
