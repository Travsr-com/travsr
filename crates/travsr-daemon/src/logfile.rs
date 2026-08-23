//! Daemon log file: where it lives, how it is capped, and how it is read.
//!
//! One module because the writer and every reader must agree on the filename
//! scheme. They previously did not: the appender writes `daemon.log.<DATE>`
//! (`tracing_appender::rolling::daily`), while `travsr daemon start` and
//! `restart` told users to "see .travsr/daemon.log" — a path that has never
//! existed. #347 and #348 were both specified against that same absent path.
//!
//! ## `event` keys
//!
//! The file is JSON lines, and a machine reading it needs something stable to
//! select on. `fields.message` is prose: rewording it for clarity silently
//! breaks every query and dashboard built on it, which is not a property a
//! contract should have. So each lifecycle and outcome event carries an `event`
//! key, dotted and lowercase, and *that* is the thing to match:
//!
//! ```text
//! jq 'select(.fields.event == "phase_b.complete")'   # stable
//! jq 'select(.fields.message == "background phase B refresh complete")'   # not
//! ```
//!
//! The keys, which are the part callers may rely on:
//!
//! ```text
//! daemon.session.start     daemon.ready          daemon.socket.bound
//! daemon.session.stop      daemon.log_pruned
//! head.drift.detected      tree.reconcile.pruned
//! head.reconcile.complete  phase_b.start         phase_b.indexed
//! phase_b.complete         kcore.updated         embed.text.updated
//! embed.text.fts_backfill  store.fts_words.backfill
//! lsif.spawn               lsif.complete         fuzz_target.missing
//! sidecar.version.checked  sidecar.version.below_floor
//! sidecar.version.stale    sidecar.version.unreadable
//! sidecar.version.probe_timeout
//! editor.attached          editor.detached
//! ```
//!
//! The `sidecar.version.*` keys (RFC-025) report the external-binary version
//! contract. They are emitted from `travsr-plugin-host` on every resolve/spawn,
//! so they surface here whenever the daemon spawns a sidecar. `checked` is DEBUG
//! (healthy spawns do not flood at info). `below_floor` (WARN), `stale` (INFO),
//! `unreadable` (WARN) and `probe_timeout` (WARN) are all transition-only — at
//! most one line per distinct value, never one per ~5s scheduler tick;
//! `below_floor`/`stale` key on the version(s), the two version-less events
//! (`unreadable`, `probe_timeout`) key on the install name. `probe_timeout`
//! fires when a bounded `--version` probe is killed for exceeding its deadline
//! and the sidecar degrades to usable rather than being refused. The daemon
//! emits `stale` from a local cache only and never fetches (local-first).
//!
//! `editor.attached` / `editor.detached` (#688) are the editor plane's only
//! log entries, and the reason it has exactly two is worth stating: an editor
//! arriving or leaving is a lifecycle fact, the same kind as a daemon starting.
//! What it reports in between is a value that changes on every keystroke,
//! which belongs in the plane the reports build (`travsr daemon lsp`) and not
//! in a narrative log. Writing each report here buried the daemon's own story
//! under someone's typing, which is how the rule above — worth adding where
//! something happened that a reader would count or chart, not to the running
//! commentary in between — reads when applied to an external producer.
//!
//! `daemon.log_pruned` reports a rotation sweep that actually dropped files, so
//! a log directory that shrank says why it shrank. Silent when nothing went,
//! which is every tick except the first one after a day roll.
//!
//! The keys keep the codebase's own vocabulary (`phase_b`, `lsif`, `kcore`)
//! because they are identifiers, matched exactly by whatever reads them, and a
//! machine gains nothing from a friendlier spelling. The *messages* beside them
//! are written for people, so they say "semantic call and reference indexing"
//! where the key says `phase_b`. Renaming a key is a breaking change for every
//! query built on it; rewording a message is not, which is the whole reason the
//! keys exist.
//!
//! Deliberately not every log site. A key is a promise to keep it spelled the
//! same way, so it is worth adding where something *happened* that a reader
//! would count, alert on, or chart, and not worth adding to the running
//! commentary in between. When adding a lifecycle event, add its key here too:
//! this list is the only place the set is visible at once.
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

/// Byte offset where the `lines`-th line from the end of `file` begins, or 0
/// when the file holds fewer than that many lines.
fn line_start_from_end(file: &mut File, len: u64, lines: usize) -> std::io::Result<u64> {
    let mut pos = len;
    let mut seen = 0usize;
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
                    return Ok(pos + i as u64 + 1);
                }
            }
        }
    }
    Ok(0)
}

/// Append the last `lines` complete lines of `path` to `out` (`0` for all).
///
/// Decodes lossily on purpose. `read_to_string` fails the entire read on one
/// invalid UTF-8 byte, so a single torn write — the daemon being killed
/// mid-line is the ordinary way to get one — made `travsr daemon logs` refuse
/// to show any of the log, at exactly the moment someone wanted to read it.
/// A replacement character in one line costs nothing by comparison.
///
/// Always leaves `out` newline-terminated, so concatenating one file's tail
/// onto another cannot glue two entries into a single line.
fn append_file_tail(path: &Path, lines: usize, out: &mut String) -> std::io::Result<()> {
    let mut file = File::open(path)?;
    let len = file.seek(SeekFrom::End(0))?;
    if len == 0 {
        return Ok(());
    }
    let start = if lines == 0 {
        0
    } else {
        line_start_from_end(&mut file, len, lines)?
    };
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(LOG_BUDGET_BYTES).read_to_end(&mut bytes)?;
    out.push_str(&String::from_utf8_lossy(&bytes));
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(())
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

/// Prune when the log has rotated since the last check.
///
/// `prune` at daemon start is not enough on its own. `rolling::daily` opens a
/// new file a day and deletes nothing, so for as long as the daemon stays up
/// neither `MAX_LOG_FILES` nor `LOG_BUDGET_BYTES` is applied: a month of uptime
/// is a month of files, and the startup sweep the caps depend on never gets a
/// turn. Rotation is the moment there is something to sweep, so the daemon's
/// tick looks for it here.
///
/// `last` carries the newest file seen on the previous call and is updated in
/// place. Seeding it from `current_log_file` after the startup prune makes the
/// first tick a no-op instead of a redundant second sweep. A directory with no
/// log file yet is left alone rather than counted as a rotation.
///
/// Cheap enough to run inline on the tick: the common case is one `read_dir`
/// that finds the same newest name and returns, and a real rotation adds at
/// most `MAX_LOG_FILES` `metadata` calls on top.
///
/// Returns the number of files removed.
pub fn prune_if_rotated(
    dir: &Path,
    last: &mut Option<PathBuf>,
    budget: u64,
    max_files: usize,
) -> usize {
    let current = current_log_file(dir);
    if current.is_none() || current == *last {
        return 0;
    }
    *last = current;
    prune(dir, budget, max_files)
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

    /// The last `lines` complete lines already in the log, across rotations.
    ///
    /// Rotation means "the last 200 lines" is rarely 200 lines of one file: at
    /// 00:01 today's file holds a handful and the rest of the answer sits in
    /// yesterday's. Reading only the newest file returns short without saying
    /// so, which reads as "the daemon logged almost nothing" when the truth is
    /// "you are looking at one file out of seven". So older files are walked,
    /// newest first, until the request is satisfied or history runs out.
    ///
    /// Each file is scanned backwards, so cost is proportional to the tail read
    /// rather than to file size: a 400 MB log tails as fast as a 4 KB one.
    /// `lines == 0` returns the whole retained history, oldest first, which
    /// [`prune`] already bounds to [`LOG_BUDGET_BYTES`].
    pub fn backfill(&self, lines: usize) -> std::io::Result<String> {
        let files = log_files(&self.dir);
        if files.is_empty() {
            return Ok(String::new());
        }

        if lines == 0 {
            let mut all = String::new();
            for path in &files {
                append_file_tail(path, 0, &mut all)?;
            }
            return Ok(all);
        }

        // Newest first, each older file supplying only what the newer ones
        // could not.
        let mut chunks: Vec<String> = Vec::new();
        let mut needed = lines;
        for path in files.iter().rev() {
            if needed == 0 {
                break;
            }
            let mut text = String::new();
            append_file_tail(path, needed, &mut text)?;
            needed = needed.saturating_sub(text.lines().count());
            chunks.push(text);
        }
        chunks.reverse();
        Ok(chunks.concat())
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

    /// The rotation gap: shortly after midnight the newest file holds a handful
    /// of lines and the rest of the answer is in yesterday's. Reading only the
    /// newest file returned short without saying so, which reads as "the daemon
    /// logged almost nothing".
    #[test]
    fn backfill_spans_rotated_files_instead_of_stopping_at_the_newest() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "daemon.log.2026-08-10", "a1\na2\na3\na4\n");
        write(d.path(), "daemon.log.2026-08-11", "b1\nb2\n");
        write(d.path(), "daemon.log.2026-08-12", "c1\n");
        let tail = LogTail::new(d.path());

        // 1 from today, 2 from yesterday, 1 from the day before: chronological.
        assert_eq!(tail.backfill(4).unwrap(), "a4\nb1\nb2\nc1\n");
        // Satisfied entirely by the newest file: older ones are never opened.
        assert_eq!(tail.backfill(1).unwrap(), "c1\n");
        // The whole retained history, oldest first.
        assert_eq!(
            tail.backfill(0).unwrap(),
            "a1\na2\na3\na4\nb1\nb2\nc1\n",
            "`--lines 0` means the whole log, not the whole newest file"
        );
        // More than exists across every file: everything, not an error.
        assert_eq!(tail.backfill(10_000).unwrap().lines().count(), 7);
    }

    /// A rotated file whose last write was torn leaves no trailing newline.
    /// Concatenating the next file straight onto it would glue two entries into
    /// one line, which silently corrupts whatever the reader greps for.
    #[test]
    fn backfill_does_not_glue_entries_across_a_file_missing_its_newline() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "daemon.log.2026-08-11",
            "first\nsecond-no-newline",
        );
        write(d.path(), "daemon.log.2026-08-12", "third\n");
        let tail = LogTail::new(d.path());

        let out = tail.backfill(0).unwrap();
        assert_eq!(out, "first\nsecond-no-newline\nthird\n");
        assert_eq!(out.lines().count(), 3, "three entries stay three lines");
    }

    /// One invalid byte used to fail the whole read, so a daemon killed
    /// mid-line made `travsr daemon logs` refuse to print any of the log.
    #[test]
    fn backfill_survives_a_non_utf8_byte_instead_of_failing_the_read() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("daemon.log.2026-08-12");
        std::fs::write(&p, b"good line\ntorn \xFF line\nlast line\n").unwrap();
        let tail = LogTail::new(d.path());

        let out = tail.backfill(0).unwrap();
        assert_eq!(out.lines().count(), 3, "no line is lost to one bad byte");
        assert!(out.starts_with("good line\n"));
        assert!(out.ends_with("last line\n"));
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

    #[test]
    fn prune_runs_again_when_the_day_rolls_under_a_running_daemon() {
        // The gap: prune ran at start and nowhere else, so a daemon left up for
        // a month held a month of files and neither cap had been applied since
        // boot. Rotation is what makes them enforceable again.
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "daemon.log.2026-08-10", "a\n");
        write(d.path(), "daemon.log.2026-08-11", "b\n");
        let mut last = current_log_file(d.path());

        // Nothing rotated: no sweep, nothing removed, even though a cap of 1
        // would otherwise take a file.
        assert_eq!(
            prune_if_rotated(d.path(), &mut last, LOG_BUDGET_BYTES, 1),
            0
        );
        assert_eq!(log_files(d.path()).len(), 2);

        // The day rolls. The cap applies now, so the oldest goes.
        write(d.path(), "daemon.log.2026-08-12", "c\n");
        assert_eq!(
            prune_if_rotated(d.path(), &mut last, LOG_BUDGET_BYTES, 2),
            1
        );
        let left: Vec<String> = log_files(d.path())
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, vec!["daemon.log.2026-08-11", "daemon.log.2026-08-12"]);

        // Same newest file on the next tick: no second sweep.
        assert_eq!(
            prune_if_rotated(d.path(), &mut last, LOG_BUDGET_BYTES, 2),
            0
        );
        assert_eq!(log_files(d.path()).len(), 2);
    }

    #[test]
    fn prune_if_rotated_leaves_a_directory_with_no_log_alone() {
        // No log file yet means the daemon has not written one, not that a
        // rotation happened. Counting it as one would sweep on every tick.
        let d = tempfile::tempdir().unwrap();
        let mut last = None;
        assert_eq!(
            prune_if_rotated(d.path(), &mut last, LOG_BUDGET_BYTES, 7),
            0
        );
        assert!(last.is_none());
    }
}
