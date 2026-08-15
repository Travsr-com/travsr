// Bounded ring buffer that drains a child's stderr on a reader thread so a
// corrupt-model / OOM message (which the sidecar prints then exits) is captured
// and can be surfaced via tracing on non-zero exit (FT-M2). Bounded so a chatty
// sidecar cannot grow memory without limit.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::process::ChildStderr;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

const MAX_LINES: usize = 64;

pub(crate) struct StderrRing {
    buf: Arc<Mutex<VecDeque<String>>>,
    handle: Option<JoinHandle<()>>,
}

impl StderrRing {
    /// Create a ring with no backing reader (used when stderr is not piped).
    pub(crate) fn spawn_empty() -> Self {
        Self {
            buf: Arc::new(Mutex::new(VecDeque::new())),
            handle: None,
        }
    }

    /// Take the child's piped stderr and start draining it. Caller must have
    /// spawned with `.stderr(Stdio::piped())`.
    pub(crate) fn spawn(stderr: ChildStderr) -> Self {
        let buf = Arc::new(Mutex::new(VecDeque::with_capacity(MAX_LINES)));
        let buf_w = Arc::clone(&buf);
        let handle = std::thread::Builder::new()
            .name("sidecar-stderr".into())
            .spawn(move || {
                let rdr = BufReader::new(stderr);
                for line in rdr.lines().map_while(Result::ok) {
                    let mut b = buf_w.lock().unwrap_or_else(|e| e.into_inner());
                    if b.len() == MAX_LINES {
                        b.pop_front();
                    }
                    b.push_back(line);
                }
            })
            .ok();
        Self { buf, handle }
    }

    /// Snapshot the last captured lines, newest last, joined by newline.
    pub(crate) fn tail(&self) -> String {
        self.buf
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl Drop for StderrRing {
    fn drop(&mut self) {
        // The reader thread ends on stderr EOF (child death closes the write end).
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(unix)]
#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    #[test]
    fn stderr_ring_captures_lines() {
        let mut child = Command::new("sh")
            .args(["-c", "echo one 1>&2; echo two 1>&2"])
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn sh");
        let stderr = child.stderr.take().expect("piped stderr");
        let ring = StderrRing::spawn(stderr);
        let _ = child.wait();
        // Give the reader thread time to drain.
        std::thread::sleep(std::time::Duration::from_millis(100));
        let tail = ring.tail();
        assert!(tail.contains("one"), "expected 'one' in: {tail}");
        assert!(tail.contains("two"), "expected 'two' in: {tail}");
    }

    #[test]
    fn stderr_ring_bounds_to_max_lines() {
        // Write MAX_LINES+10 lines; only the last MAX_LINES must be kept.
        let script = format!(
            "for i in $(seq 1 {}); do echo \"line$i\" 1>&2; done",
            MAX_LINES + 10
        );
        let mut child = Command::new("sh")
            .args(["-c", &script])
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn sh");
        let stderr = child.stderr.take().expect("piped stderr");
        let ring = StderrRing::spawn(stderr);
        let _ = child.wait();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let buf = ring.buf.lock().unwrap();
        assert!(
            buf.len() <= MAX_LINES,
            "ring must not exceed MAX_LINES, got {}",
            buf.len()
        );
        // The last line must be the highest-numbered one.
        let last = buf.back().expect("must have lines");
        assert!(
            last.contains(&(MAX_LINES + 10).to_string()),
            "last line should be line{}, got: {last}",
            MAX_LINES + 10
        );
    }
}
