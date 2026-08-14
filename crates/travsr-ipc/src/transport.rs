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

    /// Send `msg` without reading a response (#407 L2).
    ///
    /// For fire-and-forget callers (the git post-commit hook) that must never
    /// stall the user's workflow: the write is bounded by
    /// [`FIRE_AND_FORGET_DEADLINE`] and no response is awaited — the daemon
    /// processes the message after the caller has already exited. Keeping this
    /// on the trait means the line-framing knowledge lives in one crate instead
    /// of being hand-rolled at each fire-and-forget call site.
    fn send_fire_and_forget(&mut self, msg: &ControlMessage) -> anyhow::Result<()>;
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

/// Budget for a fire-and-forget send (#407 L2). Sized for the git post-commit
/// hook, which must never make a commit feel slow: if the daemon cannot accept
/// one line in this window, the hook gives up and the next commit (or a manual
/// `travsr init`) catches the graph up instead.
pub const FIRE_AND_FORGET_DEADLINE: Duration = Duration::from_millis(50);

/// Ceiling on one response line (#407 L1). A buggy daemon streaming bytes with
/// no terminating newline used to grow the client's buffer without limit; now
/// the read fails at this cap instead of OOMing the CLI.
///
/// Sized for the largest LEGITIMATE payload, not just typical traffic (#685
/// review): a deep `graph <hub-symbol> --format json --budget 0` query applies
/// its budget truncation client-side, after the full payload crosses this
/// transport, so a densely-connected seed on a big repo can produce a
/// well-formed response far above everyday sizes. 256 MiB clears that with
/// room while still bounding a runaway stream; `graph --all` bypasses the
/// daemon entirely and is never subject to this cap.
pub const MAX_RESPONSE_BYTES: usize = 256 * 1024 * 1024;

/// `msg` as its newline-terminated JSON wire line.
fn encode_line(msg: &ControlMessage) -> anyhow::Result<String> {
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    Ok(line)
}

/// Flush, treating a would-block as success: the bytes are already queued in
/// the kernel by `write_all_before`, so "not yet" on flush is not a failure.
fn flush_tolerant<S: Write>(stream: &mut S) -> anyhow::Result<()> {
    stream.flush().or_else(|e| match e.kind() {
        ErrorKind::WouldBlock | ErrorKind::Interrupted => Ok(()),
        _ => Err(e.into()),
    })
}

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
#[cfg_attr(windows, allow(dead_code))] // the Windows transport uses the bounded variant
pub(crate) fn send_request_line<S: Read + Write>(
    stream: &mut S,
    msg: &ControlMessage,
) -> anyhow::Result<ControlResponse> {
    let line = encode_line(msg)?;
    request_on(stream, &line)
}

/// The wire exchange itself: write `line`, read one line back, parse. Split
/// from [`send_request_line`] so the bounded-thread path (#407 M2) can run the
/// same exchange from an owned closure.
fn request_on<S: Read + Write>(stream: &mut S, line: &str) -> anyhow::Result<ControlResponse> {
    write_all_before(stream, line.as_bytes(), WRITE_DEADLINE)?;
    flush_tolerant(stream)?;

    let buf = read_line_before(stream, READ_DEADLINE)?;
    let trimmed = buf.trim();
    anyhow::ensure!(
        !trimmed.is_empty(),
        "daemon closed connection without a response"
    );
    Ok(serde_json::from_str::<ControlResponse>(trimmed)?)
}

/// Write `msg` as one line and return without reading a response (#407 L2).
#[cfg_attr(windows, allow(dead_code))] // the Windows transport uses the bounded variant
pub(crate) fn write_line_before<S: Write>(
    stream: &mut S,
    msg: &ControlMessage,
    deadline: Duration,
) -> anyhow::Result<()> {
    let line = encode_line(msg)?;
    write_all_before(stream, line.as_bytes(), deadline)?;
    flush_tolerant(stream)
}

/// [`send_request_line`], but executed on a worker thread with a hard overall
/// deadline (#407 M2).
///
/// The Windows named-pipe client uses blocking `File` I/O, which never returns
/// [`ErrorKind::WouldBlock`] — so the in-line deadlines in `write_all_before` /
/// `read_line_before` are never re-checked and a wedged daemon used to hang
/// `travsr status` / `ask` / `daemon stop` forever. Running the exchange on a
/// worker thread bounds it: on timeout the thread is abandoned, still parked on
/// the blocked syscall, and exits with the (short-lived) CLI process. Takes the
/// stream by value because the abandoned thread keeps it — which is also why
/// the named-pipe transport is one request per connection, matching the
/// daemon's one-response-per-connection protocol.
#[cfg_attr(unix, allow(dead_code))] // only the Windows transport needs the bounding
pub(crate) fn send_request_line_bounded<S>(
    mut stream: S,
    msg: &ControlMessage,
) -> anyhow::Result<ControlResponse>
where
    S: Read + Write + Send + 'static,
{
    let line = encode_line(msg)?;
    run_with_deadline(WRITE_DEADLINE + READ_DEADLINE, move || {
        request_on(&mut stream, &line)
    })
}

/// [`write_line_before`], on a worker thread with a hard deadline (#407 M2) —
/// the fire-and-forget analogue of [`send_request_line_bounded`].
#[cfg_attr(unix, allow(dead_code))]
pub(crate) fn write_line_bounded<S>(
    mut stream: S,
    msg: &ControlMessage,
    deadline: Duration,
) -> anyhow::Result<()>
where
    S: Write + Send + 'static,
{
    let line = encode_line(msg)?;
    run_with_deadline(deadline, move || {
        write_all_before(&mut stream, line.as_bytes(), deadline)?;
        flush_tolerant(&mut stream)
    })
}

/// Run `work` on a named worker thread; give up after `deadline`.
///
/// An abandoned worker is deliberately leaked (it is parked on a blocking
/// syscall that cannot be cancelled without unsafe FFI, which this crate
/// forbids); it dies with the process, and every caller of this crate is a
/// short-lived CLI invocation.
#[cfg_attr(unix, allow(dead_code))]
fn run_with_deadline<T: Send + 'static>(
    deadline: Duration,
    work: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("travsr-ipc-io".into())
        .spawn(move || {
            let _ = tx.send(work());
        })?;
    match rx.recv_timeout(deadline) {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => anyhow::bail!(
            "timed out after {:?} talking to the daemon over the named pipe \
             (it may be busy or wedged; `travsr daemon status` will say \
             whether it is still running)",
            deadline
        ),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            anyhow::bail!("the daemon I/O worker terminated unexpectedly")
        }
    }
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

/// Chunk size for [`read_line_before`]. Matches the order of magnitude
/// `BufReader` used before this function existed, so a large response costs a
/// handful of syscalls rather than one per byte.
const READ_CHUNK_BYTES: usize = 4096;

/// Read one `\n`-terminated line, retrying a would-block until `deadline`.
///
/// Reads in chunks rather than through a `BufReader` because the reader would
/// have to be rebuilt on every would-block retry. The accumulator survives
/// those retries instead, which gives bulk reads and would-block resilience at
/// once.
///
/// Anything the final chunk holds past the newline is dropped: this transport
/// carries exactly one response per connection, so there is no next message to
/// lose. That is what makes chunking safe here.
///
/// Sized deliberately, not by the control plane's own traffic: `send_request`
/// is shared with the daemon-served query fast-path (#318 O1), where responses
/// are `get_context` payloads and `ask` tables running to multiple KB. Reading
/// those a byte at a time would cost thousands of syscalls on the exact path
/// that exists to be fast.
fn read_line_before<S: Read>(stream: &mut S, deadline: Duration) -> anyhow::Result<String> {
    let cutoff = Instant::now() + deadline;
    let mut out = Vec::new();
    let mut chunk = [0u8; READ_CHUNK_BYTES];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break, // peer closed — caller reports the empty response
            Ok(n) => {
                if let Some(nl) = chunk[..n].iter().position(|&b| b == b'\n') {
                    out.extend_from_slice(&chunk[..nl]);
                    break;
                }
                out.extend_from_slice(&chunk[..n]);
                // #407 L1: a daemon streaming bytes with no newline must fail
                // the read, not OOM the client accumulating them forever.
                anyhow::ensure!(
                    out.len() <= MAX_RESPONSE_BYTES,
                    "daemon response exceeded the {} MiB cap; either the daemon \
                     is misbehaving or the query result is enormous (for very \
                     large graph queries, `travsr graph --all` bypasses the \
                     daemon and this cap)",
                    MAX_RESPONSE_BYTES / (1024 * 1024)
                );
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
        /// Bytes returned per successful read, to force short reads.
        read_chunk: usize,
        /// Number of `read` calls made, so a test can assert syscall shape.
        reads: std::cell::Cell<usize>,
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
            self.reads.set(self.reads.get() + 1);
            if self.read_stalls > 0 {
                self.read_stalls -= 1;
                return Err(std::io::Error::from(ErrorKind::WouldBlock));
            }
            let remaining = &self.response[self.read_pos.min(self.response.len())..];
            if remaining.is_empty() {
                return Ok(0);
            }
            // Honour `buf.len()` rather than assuming a one-byte reader, so the
            // test cannot quietly bless a byte-at-a-time implementation.
            let n = remaining.len().min(buf.len()).min(self.read_chunk);
            buf[..n].copy_from_slice(&remaining[..n]);
            self.read_pos += n;
            Ok(n)
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
            read_chunk: usize::MAX,
            reads: std::cell::Cell::new(0),
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

    /// `send_request` is shared with the daemon-served query fast-path (#318
    /// O1), so this reader carries multi-KB `get_context` and `ask` payloads,
    /// not just short control acks. Reading those a byte at a time would cost
    /// one syscall per byte on the path that exists to be fast.
    #[test]
    fn a_large_response_is_read_in_bulk_not_byte_at_a_time() {
        let payload = "x".repeat(30_000);
        let mut s = stream(0, 0, usize::MAX);
        s.response = format!("{{\"ok\":true,\"message\":\"{payload}\"}}\n").into_bytes();
        let total = s.response.len();

        let resp = send_request_line(&mut s, &ControlMessage::Status).unwrap();
        assert_eq!(resp.message.as_deref().map(str::len), Some(payload.len()));

        // Bound stated in absolute terms, not derived from READ_CHUNK_BYTES:
        // a bound computed from that constant scales with it, so shrinking the
        // chunk back to 1 would still "pass". At least 1 KB per read is the
        // property worth pinning, whatever the chunk size happens to be.
        let reads = s.reads.get();
        assert!(
            reads <= total / 1024,
            "a {total}-byte response took {reads} reads; expected bulk reads, not one per byte"
        );
    }

    #[test]
    fn a_response_split_across_short_reads_is_reassembled() {
        // A real socket returns whatever has arrived, not a full buffer. The
        // accumulator has to survive that, and a would-block landing mid-line.
        let mut s = stream(0, 0, usize::MAX);
        s.response = b"{\"ok\":true,\"message\":\"abcdefghij\"}\n".to_vec();
        s.read_chunk = 3;
        s.read_stalls = 2;

        let resp = send_request_line(&mut s, &ControlMessage::Status).unwrap();
        assert_eq!(resp.message.as_deref(), Some("abcdefghij"));
    }

    #[test]
    fn a_closed_peer_mid_write_is_reported_not_spun_on() {
        let mut s = stream(0, 0, 0); // every write accepts 0 bytes
        let err = write_all_before(&mut s, b"x", Duration::from_secs(30))
            .unwrap_err()
            .to_string();
        assert!(err.contains("closed the control connection"), "{err}");
    }

    /// A reader that streams bytes forever without ever emitting a newline —
    /// the shape of the buggy daemon #407 L1 guards against.
    struct EndlessStream;

    impl Read for EndlessStream {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            buf.fill(b'x');
            Ok(buf.len())
        }
    }

    #[test]
    fn a_response_with_no_newline_fails_at_the_cap_instead_of_growing_forever() {
        // #407 L1: before the cap this accumulated until the process OOMed.
        let err = read_line_before(&mut EndlessStream, Duration::from_secs(30))
            .unwrap_err()
            .to_string();
        assert!(err.contains("exceeded"), "{err}");
        assert!(err.contains("cap"), "{err}");
    }

    #[test]
    fn fire_and_forget_writes_one_complete_line_and_reads_nothing() {
        // #407 L2: the hook's fire-and-forget path shares the same framing as
        // the request path — one newline-terminated JSON line — and never
        // touches the read side.
        let mut s = stream(0, 0, 3); // partial writes must still resume
        s.read_stalls = usize::MAX; // any read attempt would hang the test
        write_line_before(
            &mut s,
            &ControlMessage::ReindexCommit {
                sha: "abc123".into(),
            },
            FIRE_AND_FORGET_DEADLINE,
        )
        .unwrap();
        let sent = String::from_utf8(s.written.clone()).unwrap();
        assert!(sent.ends_with('\n'), "message must be newline-terminated");
        serde_json::from_str::<ControlMessage>(sent.trim())
            .expect("the daemon must receive one complete, parseable message");
        assert_eq!(s.reads.get(), 0, "fire-and-forget must never read");
    }
}
