use std::io::{BufRead as _, BufReader, Write as _};
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
        stream.set_write_timeout(Some(Duration::from_millis(500)))?;
        stream.set_read_timeout(Some(Duration::from_secs(15)))?;
        Ok(Self { stream })
    }
}

impl ControlTransport for UnixTransport {
    fn send_request(&mut self, msg: &ControlMessage) -> anyhow::Result<ControlResponse> {
        let line = serde_json::to_string(msg)?;
        writeln!(self.stream, "{line}")?;
        // Read response via a BufReader over a reference — &UnixStream: Read.
        let mut reader = BufReader::new(&self.stream);
        let mut buf = String::new();
        reader.read_line(&mut buf)?;
        let trimmed = buf.trim();
        anyhow::ensure!(
            !trimmed.is_empty(),
            "daemon closed connection without a response"
        );
        Ok(serde_json::from_str::<ControlResponse>(trimmed)?)
    }
}
