//! Live progress UI for `travsr init` (issue #293).
//!
//! A large repo can take many minutes to index; with no output the command is
//! indistinguishable from a hang. This renders a progress indicator to
//! **stderr** (stdout stays clean for the final summary), adapting to context:
//!
//! - **TTY**: a single-line spinner updated in place via carriage return,
//!   showing `done/total (pct%)`, elapsed, and a rough ETA.
//! - **Non-TTY** (pipe/CI): occasional newline-terminated lines, no control chars.
//! - **`--json`**: one JSON object per (throttled) event on stderr.
//! - **`--quiet`**: nothing.
//!
//! Rendering is dependency-free and cross-platform: only `\r` and an ASCII
//! spinner are used, so it behaves identically on Linux, macOS, and Windows.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use travsr_daemon::InitProgress;

/// ASCII spinner frames — safe on every terminal/code page (no Unicode).
const SPINNER: [char; 4] = ['|', '/', '-', '\\'];
/// Minimum gap between TTY repaints (caps refresh at ~10/s).
const TTY_TICK: Duration = Duration::from_millis(100);
/// Cadence for non-TTY / JSON lines so logs stay readable.
const LINE_TICK: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Tty,
    Plain,
    Json,
    Quiet,
}

/// Renders [`InitProgress`] events. Construct once, call [`update`] per event,
/// then [`finish`] to clear the live line.
///
/// [`update`]: ProgressReporter::update
/// [`finish`]: ProgressReporter::finish
pub struct ProgressReporter {
    mode: Mode,
    start: Instant,
    last_paint: Instant,
    spin: usize,
    last_len: usize,
}

impl ProgressReporter {
    /// Pick a mode from the flags and whether stderr is a terminal.
    /// `--quiet` wins over `--json`.
    pub fn new(quiet: bool, json: bool) -> Self {
        let mode = if quiet {
            Mode::Quiet
        } else if json {
            Mode::Json
        } else if std::io::stderr().is_terminal() {
            Mode::Tty
        } else {
            Mode::Plain
        };
        let now = Instant::now();
        Self {
            mode,
            start: now,
            // Offset so the first non-TTY / JSON event paints immediately.
            last_paint: now - LINE_TICK,
            spin: 0,
            last_len: 0,
        }
    }

    /// Handle one progress event (throttled internally).
    pub fn update(&mut self, p: InitProgress) {
        match self.mode {
            Mode::Quiet => {}
            Mode::Tty => self.render_tty(p),
            Mode::Plain => self.render_line(p, false),
            Mode::Json => self.render_line(p, true),
        }
    }

    /// Clear the in-place TTY line so the caller's stdout summary prints cleanly.
    /// No-op in the other modes.
    pub fn finish(&mut self) {
        if self.mode == Mode::Tty && self.last_len > 0 {
            let mut err = std::io::stderr().lock();
            let _ = write!(err, "\r{}\r", " ".repeat(self.last_len));
            let _ = err.flush();
            self.last_len = 0;
        }
    }

    fn render_tty(&mut self, p: InitProgress) {
        let now = Instant::now();
        if now.duration_since(self.last_paint) < TTY_TICK {
            return;
        }
        self.last_paint = now;
        self.spin = (self.spin + 1) % SPINNER.len();
        let line = format!("{} {}", SPINNER[self.spin], self.describe(p));

        let mut err = std::io::stderr().lock();
        let visible = line.chars().count();
        let pad = self.last_len.saturating_sub(visible);
        let _ = write!(err, "\r{}{}", line, " ".repeat(pad));
        let _ = err.flush();
        self.last_len = visible;
    }

    fn render_line(&mut self, p: InitProgress, json: bool) {
        let now = Instant::now();
        if now.duration_since(self.last_paint) < LINE_TICK {
            return;
        }
        self.last_paint = now;
        let line = if json {
            self.describe_json(p)
        } else {
            format!("travsr: {}", self.describe(p))
        };
        let _ = writeln!(std::io::stderr(), "{line}");
    }

    /// Human-readable one-liner for a progress event (no spinner prefix).
    fn describe(&self, p: InitProgress) -> String {
        let elapsed = fmt_dur(self.start.elapsed());
        match p {
            InitProgress::Scanning { scanned } => {
                format!("scanning {} files  {elapsed}", commas(scanned))
            }
            InitProgress::Indexing { done, total } => {
                let pct = (done * 100).checked_div(total).unwrap_or(0);
                let eta = self
                    .eta(done, total)
                    .map(|e| format!("  eta {}", fmt_dur(e)))
                    .unwrap_or_default();
                format!(
                    "indexing {}/{} ({pct}%)  {elapsed}{eta}",
                    commas(done),
                    commas(total)
                )
            }
            InitProgress::Finalizing => format!("finalizing (semantic pass)  {elapsed}"),
        }
    }

    fn describe_json(&self, p: InitProgress) -> String {
        let secs = self.start.elapsed().as_secs();
        match p {
            InitProgress::Scanning { scanned } => {
                format!(r#"{{"phase":"scanning","scanned":{scanned},"elapsed_s":{secs}}}"#)
            }
            InitProgress::Indexing { done, total } => {
                format!(
                    r#"{{"phase":"indexing","done":{done},"total":{total},"elapsed_s":{secs}}}"#
                )
            }
            InitProgress::Finalizing => {
                format!(r#"{{"phase":"finalizing","elapsed_s":{secs}}}"#)
            }
        }
    }

    /// Rough ETA from average throughput so far. `None` once done or at start.
    fn eta(&self, done: u64, total: u64) -> Option<Duration> {
        if done == 0 || done >= total {
            return None;
        }
        let elapsed = self.start.elapsed().as_secs_f64();
        let rate = done as f64 / elapsed; // files/sec
        if rate <= 0.0 {
            return None;
        }
        Some(Duration::from_secs_f64((total - done) as f64 / rate))
    }
}

/// Group an integer with thousands separators, e.g. `17203` -> `17,203`.
fn commas(n: u64) -> String {
    let digits = n.to_string();
    let len = digits.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, ch) in digits.chars().enumerate() {
        // Insert a separator before every position whose distance from the end
        // is a non-zero multiple of three.
        if i != 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Compact human duration: `45s`, `2m30s`, `1h02m`.
fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commas_groups_thousands() {
        assert_eq!(commas(0), "0");
        assert_eq!(commas(7), "7");
        assert_eq!(commas(42), "42");
        assert_eq!(commas(123), "123");
        assert_eq!(commas(1234), "1,234");
        assert_eq!(commas(17203), "17,203");
        assert_eq!(commas(1234567), "1,234,567");
    }

    #[test]
    fn fmt_dur_scales() {
        assert_eq!(fmt_dur(Duration::from_secs(0)), "0s");
        assert_eq!(fmt_dur(Duration::from_secs(45)), "45s");
        assert_eq!(fmt_dur(Duration::from_secs(150)), "2m30s");
        assert_eq!(fmt_dur(Duration::from_secs(3720)), "1h02m");
    }

    #[test]
    fn eta_none_at_edges() {
        let r = ProgressReporter::new(true, false);
        assert!(r.eta(0, 100).is_none(), "no eta before any progress");
        assert!(r.eta(100, 100).is_none(), "no eta once complete");
        assert!(r.eta(150, 100).is_none(), "no eta past total");
    }

    #[test]
    fn eta_positive_mid_run() {
        let r = ProgressReporter::new(true, false);
        // Some real time must elapse for a finite rate.
        std::thread::sleep(Duration::from_millis(5));
        assert!(r.eta(10, 100).is_some());
    }
}
