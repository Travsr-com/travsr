// Bounded ring buffer that drains a child's stderr on a reader thread so a
// corrupt-model / OOM message (which the sidecar prints then exits) is captured
// and can be surfaced via tracing on non-zero exit (FT-M2). Bounded so a chatty
// sidecar cannot grow memory without limit.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::process::ChildStderr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

const MAX_LINES: usize = 64;
// A JVM analyzer (Gradle, sbt, KLS) emits hundreds of stderr lines, and the line
// that explains the failure ("FAILURE:", "* What went wrong:", the sidecar's own
// startup warning) appears EARLY. Evicting from the front dropped exactly that
// line and kept only trailing noise, so the first HEAD_LINES are pinned and the
// middle is discarded instead. Same MAX_LINES memory bound.
const HEAD_LINES: usize = 16;

pub(crate) struct StderrRing {
    buf: Arc<Mutex<VecDeque<String>>>,
    elided: Arc<AtomicUsize>,
    handle: Option<JoinHandle<()>>,
}

impl StderrRing {
    /// Create a ring with no backing reader (used when stderr is not piped).
    pub(crate) fn spawn_empty() -> Self {
        Self {
            buf: Arc::new(Mutex::new(VecDeque::new())),
            elided: Arc::new(AtomicUsize::new(0)),
            handle: None,
        }
    }

    /// Take the child's piped stderr and start draining it. Caller must have
    /// spawned with `.stderr(Stdio::piped())`.
    pub(crate) fn spawn(stderr: ChildStderr) -> Self {
        let buf = Arc::new(Mutex::new(VecDeque::with_capacity(MAX_LINES)));
        let elided = Arc::new(AtomicUsize::new(0));
        let buf_w = Arc::clone(&buf);
        let elided_w = Arc::clone(&elided);
        let handle = std::thread::Builder::new()
            .name("sidecar-stderr".into())
            .spawn(move || {
                let rdr = BufReader::new(stderr);
                for line in rdr.lines().map_while(Result::ok) {
                    let mut b = buf_w.lock().unwrap_or_else(|e| e.into_inner());
                    if b.len() == MAX_LINES {
                        b.remove(HEAD_LINES);
                        elided_w.fetch_add(1, Ordering::Relaxed);
                    }
                    b.push_back(line);
                }
            })
            .ok();
        Self {
            buf,
            elided,
            handle,
        }
    }

    /// Snapshot the captured lines, oldest first, joined by newline: the pinned
    /// opening lines, an elision marker once the middle was dropped, then the
    /// most recent lines.
    pub(crate) fn tail(&self) -> String {
        let b = self.buf.lock().unwrap_or_else(|e| e.into_inner());
        let elided = self.elided.load(Ordering::Relaxed);
        let mut out: Vec<String> = b.iter().take(HEAD_LINES).cloned().collect();
        if elided > 0 {
            out.push(format!("... {elided} lines elided ..."));
        }
        out.extend(b.iter().skip(HEAD_LINES).cloned());
        out.join("\n")
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

    /// Run `script`, drain its stderr into a ring, and return the ring.
    fn ring_for(script: &str) -> StderrRing {
        let mut child = Command::new("sh")
            .args(["-c", script])
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn sh");
        let stderr = child.stderr.take().expect("piped stderr");
        let ring = StderrRing::spawn(stderr);
        let _ = child.wait();
        // Give the reader thread time to drain.
        std::thread::sleep(std::time::Duration::from_millis(200));
        ring
    }

    #[test]
    fn stderr_ring_captures_lines() {
        let tail = ring_for("echo one 1>&2; echo two 1>&2").tail();
        assert!(tail.contains("one"), "expected 'one' in: {tail}");
        assert!(tail.contains("two"), "expected 'two' in: {tail}");
    }

    #[test]
    fn stderr_ring_bounds_to_max_lines() {
        // Write MAX_LINES+10 lines; only MAX_LINES must be kept.
        let ring = ring_for(&format!(
            "for i in $(seq 1 {}); do echo \"line$i\" 1>&2; done",
            MAX_LINES + 10
        ));
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

    // The reason a JVM analyzer failed is printed in its FIRST lines. Evicting
    // from the front dropped them; head + tail must keep both ends.
    #[test]
    fn stderr_ring_keeps_head_and_tail() {
        let total = MAX_LINES * 4;
        let rendered = ring_for(&format!(
            "echo 'FAILURE: Build failed with an exception.' 1>&2; \
             for i in $(seq 2 {total}); do echo \"noise$i\" 1>&2; done"
        ))
        .tail();
        assert!(
            rendered.contains("FAILURE: Build failed"),
            "the first line is the cause and must survive: {rendered}"
        );
        assert!(
            rendered.contains(&format!("noise{total}")),
            "the last line must survive: {rendered}"
        );
        assert!(
            rendered.contains("lines elided"),
            "the discarded middle must be marked: {rendered}"
        );
    }

    // Under the bound nothing is elided and the output is verbatim.
    #[test]
    fn stderr_ring_no_marker_when_under_bound() {
        let rendered = ring_for("for i in $(seq 1 5); do echo \"line$i\" 1>&2; done").tail();
        assert_eq!(rendered, "line1\nline2\nline3\nline4\nline5");
    }
}
