use std::io::{BufRead as _, BufReader, Write as _};

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
        let line = serde_json::to_string(msg)?;
        writeln!(self.file, "{line}")?;
        let mut reader = BufReader::new(&self.file);
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
