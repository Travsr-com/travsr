//! Time-bounded `git` invocations.
//!
//! Every `git` call in the CLI used `Command::output()`, which blocks until the
//! child exits *and* both pipes reach EOF. On Windows that is not the same
//! thing: `std::process::Command` forces `bInheritHandles=TRUE` whenever stdio
//! is configured, so every inheritable handle in this process is passed to the
//! child, and anything the child spawns inherits them too. A credential helper,
//! an `askpass` dialog or a pager holding the write end means the read end
//! never sees EOF, and the call never returns. The daemon spawn path documents
//! the same mechanism at length (#503 / #572); the git calls never got the same
//! treatment.
//!
//! The user-visible result is a process that hangs with no output and no error,
//! which is exactly the shape of issue #717's BUG-001 even though its stated
//! trigger (a directory with no `.git`) reaches no subprocess at all.
//!
//! This bounds the wait. It does **not** try to make git faster or to guess why
//! it is slow: a query that has not answered within the deadline is treated as
//! unavailable, and callers already have a "git could not answer" path because
//! git may legitimately be missing.

use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

/// How long a read-only `git` query may take before it is abandoned.
///
/// Generous by the standards of `rev-parse` on a warm repo, which answers in
/// milliseconds. It is a ceiling on pathology, not a performance budget: a cold
/// cache, a network filesystem or a very large repo should all finish inside
/// it, while a pipe that will never close should not hold the CLI forever.
pub const GIT_QUERY_TIMEOUT: Duration = Duration::from_secs(10);

/// Run `git <args>` in `cwd`, returning `None` if it cannot be spawned, fails
/// to complete, or exceeds `GIT_QUERY_TIMEOUT`.
///
/// `None` deliberately collapses "git is missing", "git errored" and "git did
/// not answer in time" into one answer, because every caller already has to
/// handle the first two and treats them the same way: fall back to whatever
/// works without git.
///
/// On timeout the child is left running rather than killed. Killing portably
/// without a new dependency means shelling out to `kill`/`taskkill`, which
/// spawns another process on a path that is already misbehaving. These are
/// read-only queries (`rev-parse`, `status --porcelain`), so an abandoned one
/// holds no lock and exits on its own or when this process does. Bounding the
/// wait is the property that matters; reaping is not.
///
/// `args` is generic over `AsRef<OsStr>`, the same bound [`Command::args`] uses,
/// so a caller with a path to pass (`-C <dir>`, a pathspec) can hand over the
/// raw `OsStr` rather than lossy-converting it to a `String` first. A path
/// containing bytes that are not valid UTF-8 is legal on Linux and macOS, and
/// an unpaired surrogate is reachable on Windows; `to_string_lossy` would
/// replace those bytes with U+FFFD and git would then fail to resolve a path
/// that exists.
pub fn git_output_bounded<I, S>(cwd: Option<&Path>, args: I) -> Option<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new("git");
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let child = cmd
        .args(args)
        // Null stdin so git can never block waiting for input that is not
        // coming: a credential or merge prompt on a non-interactive run is
        // itself a way to hang forever.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // `wait_with_output` is what blocks in the pathological case, so it is
        // what gets moved off this thread.
        let _ = tx.send(child.wait_with_output());
    });

    match rx.recv_timeout(GIT_QUERY_TIMEOUT) {
        Ok(Ok(out)) => Some(out),
        Ok(Err(_)) | Err(_) => None,
    }
}

/// Trimmed stdout of a successful `git <args>`, or `None`.
///
/// The shape almost every caller wants: a one-line answer from `rev-parse` and
/// friends, with a non-zero exit treated the same as no answer.
///
/// Stdout is decoded lossily. `None` here means "git had no answer", and a byte
/// sequence that is not valid UTF-8 is an answer, just one that cannot be
/// represented exactly. Strict decoding would fold the two cases together, so a
/// caller reading something that can legitimately carry arbitrary bytes (a path,
/// a commit subject) would silently see "unavailable" for a value git returned
/// perfectly well.
pub fn git_stdout_bounded<I, S>(cwd: Option<&Path>, args: I) -> Option<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let out = git_output_bounded(cwd, args)?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_normal_query_answers() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        // This crate lives in a git repo, so this is a real end-to-end call.
        let sha = git_stdout_bounded(Some(root), &["rev-parse", "--short", "HEAD"]);
        assert!(sha.is_some(), "a warm rev-parse must answer");
        assert!(!sha.unwrap().contains('\n'), "stdout must arrive trimmed");
    }

    #[test]
    fn a_failing_query_is_none_not_a_hang() {
        let dir = tempfile::tempdir().unwrap();
        // Not a git repository: git exits non-zero, promptly.
        assert!(git_stdout_bounded(Some(dir.path()), &["rev-parse", "HEAD"]).is_none());
    }

    #[test]
    fn an_unknown_subcommand_is_none() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(git_stdout_bounded(Some(root), &["definitely-not-a-git-command"]).is_none());
    }

    /// The property the module exists for: a call that would block forever
    /// returns instead. Uses a git subcommand that reads stdin, which is the
    /// cheapest way to get a child that never exits on its own, and asserts
    /// both that it returns and that it returns quickly.
    #[test]
    fn a_command_that_would_block_on_stdin_returns_promptly() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let started = std::time::Instant::now();
        // `hash-object --stdin` waits for input. stdin is null, so it sees EOF
        // immediately: the point is that stdin is never left open to block on.
        let out = git_output_bounded(Some(root), &["hash-object", "--stdin"]);
        assert!(
            started.elapsed() < GIT_QUERY_TIMEOUT,
            "a stdin-reading command must not consume the deadline"
        );
        assert!(out.is_some(), "null stdin gives it EOF rather than a wait");
    }

    /// Stdout that is not valid UTF-8 must come back lossily decoded, not as
    /// `None`. `None` is reserved for "git had no answer"; conflating the two
    /// makes a real answer look like an unavailable one.
    #[test]
    fn non_utf8_stdout_is_decoded_lossily_not_dropped() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            git_output_bounded(Some(dir.path()), ["init", "--quiet"]).is_some(),
            "temp repo must initialise"
        );

        // A blob whose content is not valid UTF-8. 0xFF is never a legal UTF-8
        // byte, so `String::from_utf8` on this fails outright.
        let blob = dir.path().join("raw.bin");
        std::fs::write(&blob, [0x61, 0xFF, 0x62]).unwrap();
        let sha = git_stdout_bounded(
            Some(dir.path()),
            [
                OsStr::new("hash-object"),
                OsStr::new("-w"),
                blob.as_os_str(),
            ],
        )
        .expect("hash-object must answer");

        let content = git_stdout_bounded(Some(dir.path()), ["cat-file", "blob", &sha]);
        assert_eq!(
            content.as_deref(),
            Some("a\u{FFFD}b"),
            "non-UTF-8 stdout must decode lossily rather than becoming None"
        );
    }

    /// A path carrying bytes that are not valid UTF-8 must reach git intact.
    ///
    /// Linux-only, because that is where such a path can actually be created.
    /// macOS rejects the filename at the filesystem layer (APFS and HFS+ both
    /// enforce valid UTF-8, so `create_dir` fails with EILSEQ), and Windows
    /// reaches the same mangling through unpaired surrogates rather than raw
    /// bytes. The defect being guarded is not Linux-specific; only the fixture
    /// is.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_non_utf8_path_argument_is_not_mangled() {
        use std::os::unix::ffi::OsStrExt;

        let dir = tempfile::tempdir().unwrap();
        // 0xFF is a legal byte in a Unix filename and never legal in UTF-8.
        let odd = dir.path().join(OsStr::from_bytes(b"work\xFFtree"));
        std::fs::create_dir(&odd).unwrap();
        assert!(
            git_output_bounded(Some(&odd), ["init", "--quiet"]).is_some(),
            "temp repo must initialise"
        );

        // Passed as an argument, which is the path that used to go through
        // `to_string_lossy`. Lossy conversion replaces 0xFF with U+FFFD, and git
        // then cannot resolve the directory.
        let top = git_stdout_bounded(
            None,
            [
                OsStr::new("-C"),
                odd.as_os_str(),
                OsStr::new("rev-parse"),
                OsStr::new("--show-toplevel"),
            ],
        );
        assert!(
            top.is_some(),
            "git must resolve a repo whose path is not valid UTF-8"
        );

        // The same call with the lossy conversion the reviewer flagged: it must
        // fail, which is what makes the assertion above meaningful rather than
        // accidentally passing.
        let lossy = git_stdout_bounded(
            None,
            ["-C", &odd.to_string_lossy(), "rev-parse", "--show-toplevel"],
        );
        assert!(
            lossy.is_none(),
            "the lossy form must genuinely fail, otherwise this test proves nothing"
        );
    }
}
