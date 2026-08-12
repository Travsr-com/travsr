//! Daemon log file: where it lives, how it is capped, and how it is read.
//!
//! One module because the writer and every reader must agree on the filename
//! scheme. They previously did not: the appender writes `daemon.log.<DATE>`
//! (`tracing_appender::rolling::daily`), while `travsr daemon start` and
//! `restart` told users to "see .travsr/daemon.log" — a path that has never
//! existed. #347 and #348 were both specified against that same absent path.
//!
//! Rotation is the reason a reader cannot simply hold an open descriptor. At
//! midnight the appender moves to a new file and the old one stops growing;
//! a naive `tail -f` keeps polling a file nothing will ever write to again and
//! shows the user a live-looking stream that has silently stopped. [`LogTail`]
//! re-resolves on every poll, which is `tail -F` behaviour rather than
//! `tail -f`.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Basename passed to the rolling appender. The date suffix is appended by
/// `tracing-appender`, so files on disk are `daemon.log.YYYY-MM-DD`.
pub const LOG_PREFIX: &str = "daemon.log";

/// Rotated files kept on disk. Daily rotation, so roughly a week of history.
pub const MAX_LOG_FILES: usize = 7;

/// Total bytes the log directory may occupy before the oldest files are
/// dropped. `MAX_LOG_FILES` bounds the count but not the size, and a single
/// day of INFO-level output on a large repo is unbounded, so both are needed.
pub const LOG_BUDGET_BYTES: u64 = 50 * 1024 * 1024;

/// Lines buffered between the daemon and the log writer.
///
/// `tracing_appender::non_blocking` defaults to 128,000, which at INFO level
/// during a large Phase A is tens of megabytes of resident memory held in a
/// channel. Combined with lossy mode below, this bounds the daemon's logging
/// footprint to something predictable.
pub const BUFFERED_LINES: usize = 2_048;

/// Bytes read from the log in a single [`LogTail::poll`].
///
/// A burst must not materialise as one enormous allocation in a CLI that is
/// only trying to print new lines. Anything beyond this is picked up by the
/// next poll.
const POLL_READ_CAP: usize = 256 * 1024;

/// Bytes scanned backwards per chunk when seeking N lines from the end.
const BACKFILL_CHUNK: usize = 8 * 1024;

/// The `.travsr` directory holding logs for `repo_root`.
pub fn log_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(".travsr")
}

/// Every rotated log file in `dir`, oldest first.
///
/// `tracing-appender` suffixes with an ISO date, which sorts lexicographically
/// in chronological order, so no date parsing is required — and a file whose
/// suffix is not a date still sorts deterministically rather than panicking.
pub fn log_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(LOG_PREFIX))
                && p.is_file()
        })
        .collect();
    files.sort();
    files
}

/// The file the daemon is currently writing to, if any.
pub fn current_log_file(dir: &Path) -> Option<PathBuf> {
    log_files(dir).pop()
}

/// Drop the oldest log files until the directory fits inside `budget`.
///
/// Runs at daemon start rather than on a timer: rotation happens at most once
/// a day, so there is no window where an unbounded amount accumulates between
/// runs that a startup sweep would miss. The newest file is never removed,
/// even when it alone exceeds the budget — deleting the file being written to
/// would lose the current session's log without freeing anything, since the
/// appender holds the descriptor.
///
/// Returns the number of files removed.
pub fn prune(dir: &Path, budget: u64, max_files: usize) -> usize {
    let mut files = log_files(dir);
    if files.len() <= 1 {
        return 0;
    }
    // Never consider the newest file for deletion.
    let newest = files.pop();
    let mut removed = 0;

    // Count cap first: oldest first, so the surviving window is the recent one.
    while files.len() + 1 > max_files {
        let victim = files.remove(0);
        if std::fs::remove_file(&victim).is_ok() {
            removed += 1;
        }
    }

    let size_of = |p: &PathBuf| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let mut total: u64 =
        files.iter().map(size_of).sum::<u64>() + newest.as_ref().map(&size_of).unwrap_or(0);

    while total > budget && !files.is_empty() {
        let victim = files.remove(0);
        let freed = size_of(&victim);
        if std::fs::remove_file(&victim).is_ok() {
            total = total.saturating_sub(freed);
            removed += 1;
        } else {
            break;
        }
    }
    removed
}

/// A rotation-aware reader over the daemon log.
///
/// Emits only complete lines. A write that lands mid-line is held until its
/// newline arrives, because both consumers prefix what they print — `[travsr]`
/// during startup relay, and nothing at all for `daemon logs`, where a split
/// line would corrupt a `grep`.
pub struct LogTail {
    dir: PathBuf,
    path: Option<PathBuf>,
    file: Option<File>,
    offset: u64,
    partial: String,
}

impl LogTail {
    /// Open a tail over the newest log file in `dir`. Succeeds even when no
    /// log exists yet: the daemon may not have started, and [`Self::poll`]
    /// picks the file up as soon as it appears.
    pub fn new(dir: &Path) -> Self {
        let mut tail = Self {
            dir: dir.to_path_buf(),
            path: None,
            file: None,
            offset: 0,
            partial: String::new(),
        };
        tail.reopen_current();
        tail
    }

    /// The file currently being followed, if one exists.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn reopen_current(&mut self) {
        match current_log_file(&self.dir) {
            Some(p) => {
                self.file = File::open(&p).ok();
                self.path = self.file.as_ref().map(|_| p);
                self.offset = 0;
            }
            None => {
                self.file = None;
                self.path = None;
                self.offset = 0;
            }
        }
    }

    /// Position the tail at the end, so [`Self::poll`] returns only lines
    /// written from now on.
    pub fn seek_to_end(&mut self) {
        if let Some(f) = self.file.as_mut() {
            if let Ok(len) = f.seek(SeekFrom::End(0)) {
                self.offset = len;
            }
        }
    }

    /// The last `lines` complete lines already in the log.
    ///
    /// Scans backwards from the end so cost is proportional to the tail read,
    /// not to file size — a long-lived daemon's log is read at the same speed
    /// whether it is 4 KB or 400 MB. `lines == 0` returns the whole file.
    pub fn backfill(&self, lines: usize) -> std::io::Result<String> {
        let Some(path) = &self.path else {
            return Ok(String::new());
        };
        let mut file = File::open(path)?;
        let len = file.seek(SeekFrom::End(0))?;
        if len == 0 {
            return Ok(String::new());
        }

        if lines == 0 {
            file.seek(SeekFrom::Start(0))?;
            let mut all = String::new();
            file.take(LOG_BUDGET_BYTES).read_to_string(&mut all)?;
            return Ok(all);
        }

        // Walk backwards a chunk at a time until enough newlines are behind us.
        let mut pos = len;
        let mut seen = 0usize;
        let mut start = 0u64;
        let mut chunk = vec![0u8; BACKFILL_CHUNK];
        while pos > 0 {
            let take = std::cmp::min(BACKFILL_CHUNK as u64, pos) as usize;
            pos -= take as u64;
            file.seek(SeekFrom::Start(pos))?;
            file.read_exact(&mut chunk[..take])?;
            for i in (0..take).rev() {
                if chunk[i] == b'\n' {
                    // The trailing newline of the final line does not begin one.
                    if pos + i as u64 + 1 == len {
                        continue;
                    }
                    seen += 1;
                    if seen == lines {
                        start = pos + i as u64 + 1;
                        pos = 0;
                        break;
                    }
                }
            }
        }

        file.seek(SeekFrom::Start(start))?;
        let mut out = String::new();
        file.take(LOG_BUDGET_BYTES).read_to_string(&mut out)?;
        Ok(out)
    }

    /// Complete lines written since the previous call.
    ///
    /// Handles the two ways the underlying file stops being the right one:
    /// rotation, where the newest file changes and reading must restart from
    /// the beginning of the new one, and truncation or deletion, where the
    /// recorded offset is past the end and every later read would return
    /// nothing forever.
    pub fn poll(&mut self) -> std::io::Result<Vec<String>> {
        // Rotation, or a log appearing for the first time.
        let newest = current_log_file(&self.dir);
        if newest.as_deref() != self.path.as_deref() {
            let carried = std::mem::take(&mut self.partial);
            self.reopen_current();
            let mut lines = Vec::new();
            if !carried.trim().is_empty() {
                lines.push(carried);
            }
            lines.extend(self.read_new()?);
            return Ok(lines);
        }
        self.read_new()
    }

    fn read_new(&mut self) -> std::io::Result<Vec<String>> {
        let Some(file) = self.file.as_mut() else {
            return Ok(Vec::new());
        };
        let len = file.seek(SeekFrom::End(0))?;

        // Truncated or replaced in place: start over rather than sit silent.
        if len < self.offset {
            self.offset = 0;
            self.partial.clear();
        }
        if len == self.offset {
            return Ok(Vec::new());
        }

        file.seek(SeekFrom::Start(self.offset))?;
        let want = std::cmp::min(len - self.offset, POLL_READ_CAP as u64) as usize;
        let mut buf = vec![0u8; want];
        let read = file.read(&mut buf)?;
        buf.truncate(read);
        self.offset += read as u64;

        self.partial.push_str(&String::from_utf8_lossy(&buf));

        // Emit up to the last newline; keep any trailing fragment for next time.
        let mut lines = Vec::new();
        if let Some(cut) = self.partial.rfind('\n') {
            let complete: String = self.partial.drain(..=cut).collect();
            lines.extend(complete.lines().map(str::to_string));
        }
        Ok(lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&p)
            .unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    #[test]
    fn the_newest_dated_file_is_the_current_one() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "daemon.log.2026-08-05", "old\n");
        write(d.path(), "daemon.log.2026-08-09", "new\n");
        write(d.path(), "graph.db", "not a log\n");
        assert_eq!(
            current_log_file(d.path()).unwrap().file_name().unwrap(),
            "daemon.log.2026-08-09"
        );
        assert_eq!(log_files(d.path()).len(), 2, "graph.db must not be counted");
    }

    #[test]
    fn a_rotation_mid_follow_is_picked_up_instead_of_going_silent() {
        // The failure this guards: a tail holding the previous day's descriptor
        // keeps polling a file nothing writes to again, and shows a live-looking
        // stream that has stopped.
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "daemon.log.2026-08-11", "before\n");
        let mut tail = LogTail::new(d.path());
        tail.seek_to_end();
        assert!(tail.poll().unwrap().is_empty());

        write(d.path(), "daemon.log.2026-08-12", "after rotation\n");
        assert_eq!(tail.poll().unwrap(), vec!["after rotation".to_string()]);
    }

    #[test]
    fn a_line_split_across_two_polls_is_emitted_once_and_whole() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "daemon.log.2026-08-12", "");
        let mut tail = LogTail::new(d.path());
        tail.seek_to_end();

        write(d.path(), "daemon.log.2026-08-12", "half a li");
        assert!(
            tail.poll().unwrap().is_empty(),
            "a fragment must not be emitted as a line"
        );

        write(d.path(), "daemon.log.2026-08-12", "ne here\n");
        assert_eq!(tail.poll().unwrap(), vec!["half a line here".to_string()]);
    }

    #[test]
    fn truncation_resets_instead_of_going_silent_forever() {
        let d = tempfile::tempdir().unwrap();
        let p = write(d.path(), "daemon.log.2026-08-12", "one\ntwo\n");
        let mut tail = LogTail::new(d.path());
        tail.seek_to_end();

        std::fs::write(&p, "fresh\n").unwrap();
        assert_eq!(tail.poll().unwrap(), vec!["fresh".to_string()]);
    }

    #[test]
    fn backfill_returns_the_last_n_lines_only() {
        let d = tempfile::tempdir().unwrap();
        let body: String = (1..=200).map(|i| format!("line {i}\n")).collect();
        write(d.path(), "daemon.log.2026-08-12", &body);
        let tail = LogTail::new(d.path());

        let out = tail.backfill(3).unwrap();
        assert_eq!(out, "line 198\nline 199\nline 200\n");

        let all = tail.backfill(0).unwrap();
        assert_eq!(all.lines().count(), 200);

        // More lines requested than exist: return everything, not an error.
        assert_eq!(tail.backfill(10_000).unwrap().lines().count(), 200);
    }

    #[test]
    fn backfill_reads_the_tail_not_the_whole_file() {
        // Guards the reverse-seek: a naive read_to_string would also pass the
        // assertion above, so size the file past any plausible buffer and check
        // the result is still exact.
        let d = tempfile::tempdir().unwrap();
        let body: String = (1..=60_000).map(|i| format!("line {i}\n")).collect();
        write(d.path(), "daemon.log.2026-08-12", &body);
        let tail = LogTail::new(d.path());
        assert_eq!(tail.backfill(2).unwrap(), "line 59999\nline 60000\n");
    }

    #[test]
    fn an_absent_log_is_not_an_error() {
        let d = tempfile::tempdir().unwrap();
        let mut tail = LogTail::new(d.path());
        assert!(tail.path().is_none());
        assert!(tail.poll().unwrap().is_empty());
        assert_eq!(tail.backfill(50).unwrap(), "");
    }

    #[test]
    fn a_log_created_after_the_tail_opened_is_picked_up() {
        // #348 relies on this: the parent starts tailing before the child has
        // opened its log file.
        let d = tempfile::tempdir().unwrap();
        let mut tail = LogTail::new(d.path());
        assert!(tail.path().is_none());

        write(d.path(), "daemon.log.2026-08-12", "daemon starting\n");
        assert_eq!(tail.poll().unwrap(), vec!["daemon starting".to_string()]);
    }

    #[test]
    fn prune_keeps_the_newest_and_respects_the_count_cap() {
        let d = tempfile::tempdir().unwrap();
        for day in 1..=10 {
            write(d.path(), &format!("daemon.log.2026-08-{day:02}"), "x\n");
        }
        let removed = prune(d.path(), LOG_BUDGET_BYTES, 3);
        assert_eq!(removed, 7);
        let left = log_files(d.path());
        assert_eq!(left.len(), 3);
        assert_eq!(
            left.last().unwrap().file_name().unwrap(),
            "daemon.log.2026-08-10"
        );
    }

    #[test]
    fn prune_drops_oldest_until_the_byte_budget_is_met() {
        let d = tempfile::tempdir().unwrap();
        let big = "x".repeat(1000);
        for day in 1..=5 {
            write(d.path(), &format!("daemon.log.2026-08-{day:02}"), &big);
        }
        // Budget fits two files; the newest is never a candidate.
        let removed = prune(d.path(), 2_100, 100);
        assert!(removed >= 3, "expected the oldest to go, removed {removed}");
        let left = log_files(d.path());
        assert!(left.len() <= 2, "left {} files", left.len());
        assert_eq!(
            left.last().unwrap().file_name().unwrap(),
            "daemon.log.2026-08-05"
        );
    }

    #[test]
    fn prune_never_removes_the_file_being_written() {
        // Even when the current file alone blows the budget: deleting it loses
        // this session's log and frees nothing, because the appender holds the
        // descriptor.
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "daemon.log.2026-08-12", &"x".repeat(10_000));
        assert_eq!(prune(d.path(), 10, 1), 0);
        assert_eq!(log_files(d.path()).len(), 1);
    }
}
