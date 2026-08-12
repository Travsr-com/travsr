use std::time::{Duration, Instant};

use crate::{ControlAddr, ControlMessage, ControlResponse, ControlTransport};

/// `SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION` (#407 M1).
///
/// The pipe name is predictable (`\\.\pipe\travsr-<blake3(repo)[..8]>`), so on
/// a multi-user machine a malicious local user could create the pipe first and
/// wait for a client. Without a QoS cap, that fake server could call
/// `ImpersonateNamedPipeClient` and act with the caller's identity.
/// `SECURITY_IDENTIFICATION` caps impersonation at identify-only: the server
/// can learn who connected but cannot act as them. (The daemon side is already
/// protected by `first_pipe_instance(true)`.)
const SECURITY_QOS_IDENTIFICATION: u32 = 0x0010_0000 | 0x0001_0000;

/// `ERROR_PIPE_BUSY` — every pipe instance is serving another client.
const ERROR_PIPE_BUSY: i32 = 231;

/// How long `connect` keeps retrying a busy pipe before giving up (#407 M2).
/// The daemon creates one pipe instance per accept-loop iteration, so two
/// concurrent CLI calls race for it; the loser sees `ERROR_PIPE_BUSY` for the
/// few milliseconds until the daemon's next `create` call.
const BUSY_RETRY_DEADLINE: Duration = Duration::from_secs(2);
const BUSY_RETRY_INTERVAL: Duration = Duration::from_millis(20);

/// Synchronous Windows Named Pipe client for the daemon control plane.
///
/// Opens `\\.\pipe\travsr-<hex>` as a blocking file handle. Windows allows
/// synchronous client I/O on a named pipe even when the server uses overlapped
/// (async) I/O — the client side does not need FILE_FLAG_OVERLAPPED.
///
/// One request per connection: blocking pipe I/O is bounded by handing the
/// handle to a deadline-guarded worker thread (#407 M2), so the handle is
/// consumed by the first request. That matches the daemon, which answers one
/// message per connection anyway.
pub struct NamedPipeTransport {
    file: Option<std::fs::File>,
}

impl NamedPipeTransport {
    /// Connect to the daemon's named pipe at `addr.pipe_name()`.
    ///
    /// Returns `Err` if no daemon is listening. A busy pipe (all instances
    /// serving other clients) is retried until [`BUSY_RETRY_DEADLINE`] and then
    /// reported as busy — NOT as "daemon not running", which used to push the
    /// CLI onto its slow direct-open fallback for a daemon that was merely
    /// mid-request (#407 M2).
    pub fn connect(addr: &ControlAddr) -> anyhow::Result<Self> {
        use std::os::windows::fs::OpenOptionsExt as _;

        let pipe_name = addr.pipe_name();
        let cutoff = Instant::now() + BUSY_RETRY_DEADLINE;
        loop {
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .security_qos_flags(SECURITY_QOS_IDENTIFICATION)
                .open(&pipe_name)
            {
                Ok(file) => return Ok(Self { file: Some(file) }),
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                    anyhow::ensure!(
                        Instant::now() < cutoff,
                        "daemon is busy ({} had no free pipe instance for {:?}) — \
                         retry shortly",
                        pipe_name,
                        BUSY_RETRY_DEADLINE
                    );
                    std::thread::sleep(BUSY_RETRY_INTERVAL);
                }
                Err(e) => anyhow::bail!("daemon not running ({}): {e}", pipe_name),
            }
        }
    }

    /// The pipe handle, consumed by the first request (see type-level docs).
    fn take_file(&mut self) -> anyhow::Result<std::fs::File> {
        self.file.take().ok_or_else(|| {
            anyhow::anyhow!(
                "named-pipe transport already used — the control plane answers \
                 one request per connection, reconnect for the next one"
            )
        })
    }
}

impl ControlTransport for NamedPipeTransport {
    fn send_request(&mut self, msg: &ControlMessage) -> anyhow::Result<ControlResponse> {
        // #541: shares the wire exchange with the Unix transport (framing,
        // partial-write resume, response cap). #407 M2: named-pipe client I/O
        // here is blocking and never reports WouldBlock, so the shared
        // deadlines alone cannot fire — the exchange runs on a worker thread
        // with a hard overall deadline instead, and a wedged daemon yields a
        // timeout error rather than hanging `status`/`ask`/`daemon stop`.
        let file = self.take_file()?;
        crate::transport::send_request_line_bounded(file, msg)
    }

    fn send_fire_and_forget(&mut self, msg: &ControlMessage) -> anyhow::Result<()> {
        let file = self.take_file()?;
        crate::transport::write_line_bounded(file, msg, crate::transport::FIRE_AND_FORGET_DEADLINE)
    }
}
