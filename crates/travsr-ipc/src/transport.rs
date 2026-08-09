use std::io::{ErrorKind, Read, Write};
use std::time::{Duration, Instant};

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

/// Overall ceiling on getting the request line out (#541).
///
/// This is a deadline across retries, not a per-syscall timeout. The socket's
/// own `SO_SNDTIMEO` is what wakes us up to re-check it, so it stays short
/// while the budget that actually decides success lives here.
pub const WRITE_DEADLINE: Duration = Duration::from_secs(5);

/// Overall ceiling on reading the response line (#541). Matches the pre-#541
/// `SO_RCVTIMEO`, so a daemon busy with an initial scan still gets the same
/// grace it always had — the change is that a would-block now retries within
/// the window instead of failing the whole command on the first one.
pub const READ_DEADLINE: Duration = Duration::from_secs(15);

/// Send `msg` as one JSON line and read one JSON line back, retrying rather
/// than surrendering on a would-block.
///
/// #541: both transports previously used `writeln!` + `BufReader::read_line`
/// directly on a socket carrying `SO_SNDTIMEO` / `SO_RCVTIMEO`. When one of
/// those timeouts fires, the OS reports `EAGAIN` (errno 35 on macOS/BSD,
/// surfaced as [`ErrorKind::WouldBlock`]), and `write_all` treats that as
/// fatal. `travsr daemon stop` then failed with a bare "Resource temporarily
/// unavailable (os error 35)" while the daemon it was supposed to stop kept
/// running and kept answering queries from a stale binary image.
///
/// A would-block on a socket with a send/receive timeout means "not yet", not
/// "never" — so both directions loop until their deadline. Partial writes are
/// resumed from the offset the kernel accepted, which the old `writeln!` could
/// not do: a timeout mid-line used to leave a truncated JSON message in the
/// daemon's buffer.
pub(crate) fn send_request_line<S: Read + Write>(
    stream: &mut S,
    msg: &ControlMessage,
) -> anyhow::Result<ControlResponse> {
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    write_all_before(stream, line.as_bytes(), WRITE_DEADLINE)?;
    stream.flush().or_else(|e| match e.kind() {
        // A would-block on flush is the same "not yet" as one on write, and the
        // bytes are already queued in the kernel by `write_all_before`.
        ErrorKind::WouldBlock | ErrorKind::Interrupted => Ok(()),
        _ => Err(e),
    })?;

    let buf = read_line_before(stream, READ_DEADLINE)?;
    let trimmed = buf.trim();
    anyhow::ensure!(
        !trimmed.is_empty(),
        "daemon closed connection without a response"
    );
    Ok(serde_json::from_str::<ControlResponse>(trimmed)?)
}

/// `write_all`, but a would-block retries until `deadline` instead of failing.
fn write_all_before<S: Write>(
    stream: &mut S,
    mut buf: &[u8],
    deadline: Duration,
) -> anyhow::Result<()> {
    let cutoff = Instant::now() + deadline;
    while !buf.is_empty() {
        match stream.write(buf) {
            // A zero-length write with bytes left is a closed peer, not
            // progress — looping on it would spin until the deadline.
            Ok(0) => anyhow::bail!("daemon closed the control connection mid-request"),
            Ok(n) => buf = &buf[n..],
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                anyhow::ensure!(
                    Instant::now() < cutoff,
                    "timed out after {}s sending the request to the daemon \
                     (it may be busy or wedged; `travsr daemon status` will say \
                     whether it is still running)",
                    deadline.as_secs()
                );
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Read one `\n`-terminated line, retrying a would-block until `deadline`.
///
/// Reads a byte at a time rather than through a `BufReader` because the buffer
/// would have to be rebuilt on every would-block retry, discarding whatever it
/// had already consumed past the line. Control responses are one short JSON
/// line, so the syscall count is not worth optimising against that risk.
fn read_line_before<S: Read>(stream: &mut S, deadline: Duration) -> anyhow::Result<String> {
    let cutoff = Instant::now() + deadline;
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => break, // peer closed — caller reports the empty response
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                out.push(byte[0]);
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                anyhow::ensure!(
                    Instant::now() < cutoff,
                    "timed out after {}s waiting for the daemon's response \
                     (it may be busy or wedged; `travsr daemon status` will say \
                     whether it is still running)",
                    deadline.as_secs()
                );
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stream that returns `WouldBlock` for the first `stalls` calls on each
    /// direction, then behaves normally — the shape of a socket whose
    /// `SO_SNDTIMEO` / `SO_RCVTIMEO` fires while the daemon is busy.
    struct StallingStream {
        write_stalls: usize,
        read_stalls: usize,
        written: Vec<u8>,
        response: Vec<u8>,
        read_pos: usize,
        /// Bytes accepted per successful write, to force partial writes.
        chunk: usize,
    }

    impl Write for StallingStream {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.write_stalls > 0 {
                self.write_stalls -= 1;
                return Err(std::io::Error::from(ErrorKind::WouldBlock));
            }
            let n = self.chunk.min(buf.len());
            self.written.extend_from_slice(&buf[..n]);
            Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Read for StallingStream {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.read_stalls > 0 {
                self.read_stalls -= 1;
                return Err(std::io::Error::from(ErrorKind::WouldBlock));
            }
            if self.read_pos >= self.response.len() {
                return Ok(0);
            }
            buf[0] = self.response[self.read_pos];
            self.read_pos += 1;
            Ok(1)
        }
    }

    fn stream(write_stalls: usize, read_stalls: usize, chunk: usize) -> StallingStream {
        StallingStream {
            write_stalls,
            read_stalls,
            written: Vec::new(),
            response: b"{\"ok\":true,\"message\":null}\n".to_vec(),
            read_pos: 0,
            chunk,
        }
    }

    #[test]
    fn would_block_on_write_is_retried_not_fatal() {
        // #541: this is the exact failure. Before the fix the first WouldBlock
        // surfaced as "Resource temporarily unavailable (os error 35)" and the
        // Shutdown never reached the daemon.
        let mut s = stream(3, 0, usize::MAX);
        let resp = send_request_line(&mut s, &ControlMessage::Shutdown).unwrap();
        assert!(resp.ok);
    }

    #[test]
    fn would_block_on_read_is_retried_not_fatal() {
        let mut s = stream(0, 5, usize::MAX);
        let resp = send_request_line(&mut s, &ControlMessage::Shutdown).unwrap();
        assert!(resp.ok);
    }

    #[test]
    fn a_partial_write_resumes_instead_of_truncating_the_message() {
        // The old `writeln!` path could leave a half-written JSON line in the
        // daemon's buffer when a timeout landed mid-message.
        let mut s = stream(0, 0, 3);
        send_request_line(&mut s, &ControlMessage::Shutdown).unwrap();
        let sent = String::from_utf8(s.written.clone()).unwrap();
        assert!(sent.ends_with('\n'), "message must be newline-terminated");
        serde_json::from_str::<ControlMessage>(sent.trim())
            .expect("the daemon must receive one complete, parseable message");
    }

    #[test]
    fn write_stall_past_the_deadline_reports_a_timeout_not_a_raw_errno() {
        // The user-facing half of #541: whatever happens, the message must say
        // what to do next rather than surfacing errno 35.
        let mut s = stream(usize::MAX, 0, usize::MAX);
        let err = write_all_before(&mut s, b"x", Duration::from_millis(1))
            .unwrap_err()
            .to_string();
        assert!(err.contains("timed out"), "{err}");
        assert!(err.contains("daemon status"), "{err}");
    }

    #[test]
    fn a_closed_peer_mid_write_is_reported_not_spun_on() {
        let mut s = stream(0, 0, 0); // every write accepts 0 bytes
        let err = write_all_before(&mut s, b"x", Duration::from_secs(30))
            .unwrap_err()
            .to_string();
        assert!(err.contains("closed the control connection"), "{err}");
    }
}
