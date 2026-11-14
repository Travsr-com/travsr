//! The Changes page: what the working tree's uncommitted work touches.
//!
//! The diff is taken against the commit the index describes, not HEAD, so
//! its old-side line numbers are exactly the index's: each hunk is mapped to
//! the innermost indexed symbols it overlaps, and each of those to its
//! callers. Read-only throughout: `git diff` / `git status` (bounded) and a
//! read-only SQLite connection, as the rest of `travsr ui` reads.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Serialize;

use super::{resolve_repo, ApiError, UiState};
use crate::git_bounded::{git_output_bounded, git_stdout_bounded};

/// Most files listed; a branch switch can leave thousands.
const MAX_FILES: usize = 200;
/// Most symbols listed per file.
const MAX_SYMBOLS: usize = 50;
/// Most callers listed per symbol (the total is reported separately).
const MAX_CALLERS: usize = 25;

#[derive(Serialize, Debug, PartialEq)]
pub(super) struct Caller {
    sig: String,
    kind: String,
    path: String,
    line: i64,
}

#[derive(Serialize, Debug, PartialEq)]
pub(super) struct Symbol {
    sig: String,
    kind: String,
    line: i64,
    end_line: i64,
    callers_total: i64,
    callers: Vec<Caller>,
}

#[derive(Serialize, Debug)]
pub(super) struct ChangedFile {
    path: String,
    /// modified, added, deleted, renamed or untracked.
    status: &'static str,
    /// Whether the index has this file (an untracked or newly added file has
    /// no symbols to map yet).
    indexed: bool,
    hunks: usize,
    symbols: Vec<Symbol>,
    /// The index holds a version of this file that is neither the base commit
    /// nor what is on disk (a save the daemon indexed before it stopped), so
    /// the edits are mapped with today's line numbers and may be off.
    index_between: bool,
}

#[derive(Serialize, Debug)]
pub(super) struct ChangesResponse {
    /// The commit the index describes, which the diff is taken against.
    base: String,
    head: String,
    files: Vec<ChangedFile>,
    truncated: bool,
    /// The line-level diff could not be read in time, so files are listed
    /// but no edit is mapped to a symbol.
    diff_incomplete: bool,
}

/// One `git diff --name-status` or porcelain status letter, in words.
fn status_word(code: char) -> &'static str {
    match code {
        'A' => "added",
        'D' => "deleted",
        'R' => "renamed",
        'C' => "added",
        '?' => "untracked",
        _ => "modified",
    }
}

/// `git diff --name-status -z` output: `<code>\0<path>\0`, with a second path
/// for renames and copies (`R100\0old\0new\0`). Returns (status, path), the
/// new path for a rename.
pub(super) fn parse_name_status(out: &str) -> Vec<(&'static str, String)> {
    let mut parts = out.split('\0').filter(|s| !s.is_empty());
    let mut files = Vec::new();
    while let Some(code) = parts.next() {
        let c = code.chars().next().unwrap_or('M');
        let Some(path) = parts.next() else { break };
        let path = if matches!(c, 'R' | 'C') {
            parts.next().unwrap_or(path)
        } else {
            path
        };
        files.push((status_word(c), path.replace('\\', "/")));
    }
    files
}

/// Old-side line ranges (inclusive, 1-based) per file from a `-U0` diff (tests;
/// the page picks a side per file with [`ranges_for_index`]). A
/// pure insertion (`-a,0`) touches the lines around position `a`, so the
/// symbol it lands in counts as changed.
#[cfg(test)]
pub(super) fn parse_hunks(diff: &str) -> BTreeMap<String, Vec<(i64, i64)>> {
    parse_hunk_sides(diff)
        .into_iter()
        .map(|(path, sides)| (path, sides.old))
        .collect()
}

/// Both sides of every hunk of one file: `old` in the base commit's line
/// numbers (`-a,b`), `new` in the working tree's (`+c,d`).
#[derive(Default, Clone, Debug, PartialEq)]
pub(super) struct HunkSides {
    pub(super) old: Vec<(i64, i64)>,
    pub(super) new: Vec<(i64, i64)>,
}

/// One side's `start,count` as an inclusive line range. A side with no lines
/// (`a,0`: a pure insertion seen from the old side, a pure deletion from the
/// new) touches the lines around position `a`.
fn side_range(token: &str) -> (i64, i64) {
    let mut it = token.split(',');
    let start: i64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let count: i64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(1);
    if count == 0 {
        (start.max(1), start.max(1) + 1)
    } else {
        (start, start + count - 1)
    }
}

pub(super) fn parse_hunk_sides(diff: &str) -> BTreeMap<String, HunkSides> {
    let mut out: BTreeMap<String, HunkSides> = BTreeMap::new();
    let mut current: Option<String> = None;
    // `---` / `+++` are headers only between `diff --git` and the first `@@`.
    // In a hunk body they are content: deleting the line `-- note` (an SQL or
    // Lua comment) prints `--- note`, which must not end the file.
    let mut in_header = false;
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            in_header = true;
            current = None;
        } else if in_header && line.starts_with("--- ") {
            // The old side names the file as the index knows it; a new file
            // has /dev/null there and takes its name from `+++`.
            current = header_path(&line[4..], "a/");
        } else if in_header && line.starts_with("+++ ") {
            if current.is_none() {
                current = header_path(&line[4..], "b/");
            }
        } else if let Some(rest) = line.strip_prefix("@@ -") {
            in_header = false;
            let Some(path) = current.clone() else {
                continue;
            };
            let mut tokens = rest.split_whitespace();
            let old = tokens.next().unwrap_or("");
            let new = tokens
                .next()
                .and_then(|t| t.strip_prefix('+'))
                .unwrap_or("");
            let sides = out.entry(path).or_default();
            sides.old.push(side_range(old));
            sides.new.push(side_range(new));
        }
    }
    out
}

/// The repo-relative path on a `---` / `+++` header line, without its `a/` or
/// `b/` side prefix. Git ends a path that contains a space with a TAB, and
/// quotes one with special characters C-style (`"a/caf\303\251.rs"`); both
/// are undone here so the path matches `--name-status -z`'s raw spelling.
fn header_path(rest: &str, side: &str) -> Option<String> {
    let rest = rest.strip_suffix('\t').unwrap_or(rest);
    let path = if rest.starts_with('"') {
        unquote_c(rest)?
    } else {
        rest.to_string()
    };
    path.strip_prefix(side).map(str::to_string)
}

/// Git's C-style quoting: `\\`, `\"`, the usual letter escapes and `\ooo`
/// octal bytes (UTF-8 encoded one byte at a time).
fn unquote_c(s: &str) -> Option<String> {
    let inner = s.strip_prefix('"')?.strip_suffix('"')?;
    let mut out: Vec<u8> = Vec::with_capacity(inner.len());
    let bytes = inner.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        let next = *bytes.get(i + 1)?;
        let (byte, used) = match next {
            b'0'..=b'7' => {
                let oct = std::str::from_utf8(bytes.get(i + 1..i + 4)?).ok()?;
                (u8::from_str_radix(oct, 8).ok()?, 4)
            }
            b'a' => (0x07, 2),
            b'b' => (0x08, 2),
            b't' => (b'\t', 2),
            b'n' => (b'\n', 2),
            b'v' => (0x0b, 2),
            b'f' => (0x0c, 2),
            b'r' => (b'\r', 2),
            other => (other, 2),
        };
        out.push(byte);
        i += used;
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// Keep only the innermost of nested spans: an `impl` block and the method
/// inside it both overlap an edit to the method, and the method is the change.
pub(super) fn innermost<T>(mut items: Vec<(i64, i64, T)>) -> Vec<(i64, i64, T)> {
    items.sort_by_key(|(s, e, _)| (e - s, *s));
    let mut kept: Vec<(i64, i64, T)> = Vec::new();
    for (s, e, v) in items {
        if !kept.iter().any(|(ks, ke, _)| s <= *ks && *ke <= e) {
            kept.push((s, e, v));
        }
    }
    kept.sort_by_key(|(s, _, _)| *s);
    kept
}

fn read_only(db: &Path) -> rusqlite::Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_secs(2))?;
    Ok(conn)
}

/// The indexed symbols of `path` overlapping `ranges`, innermost first, each
/// with its callers.
pub(super) fn touched_symbols(
    conn: &rusqlite::Connection,
    path: &str,
    ranges: &[(i64, i64)],
) -> rusqlite::Result<Vec<Symbol>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, signature, kind, line, COALESCE(end_line, line) FROM nodes
         WHERE path = ?1 AND line > 0 AND kind NOT IN ('file', 'import', 'module')
           AND signature NOT LIKE 'use:%'",
    )?;
    let nodes: Vec<(i64, String, String, i64, i64)> = stmt
        .query_map([path], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let hit: Vec<(i64, i64, (i64, String, String))> = nodes
        .into_iter()
        .filter(|(_, _, _, s, e)| ranges.iter().any(|(a, b)| *a <= *e && *s <= *b))
        .map(|(id, sig, kind, s, e)| (s, e, (id, sig, kind)))
        .collect();
    let mut callers_stmt = conn.prepare_cached(
        "SELECT DISTINCT ns.signature, ns.kind, ns.path, ns.line FROM edges e
         JOIN nodes ns ON ns.id = e.src
         WHERE e.dst = ?1 AND e.kind = 'ref/call' AND e.src <> e.dst
         ORDER BY ns.path, ns.line LIMIT ?2",
    )?;
    let mut count_stmt = conn.prepare_cached(
        "SELECT COUNT(DISTINCT src) FROM edges WHERE dst = ?1 AND kind = 'ref/call' AND src <> dst",
    )?;
    let mut out = Vec::new();
    for (s, e, (id, sig, kind)) in innermost(hit).into_iter().take(MAX_SYMBOLS) {
        let callers = callers_stmt
            .query_map(rusqlite::params![id, MAX_CALLERS as i64], |r| {
                Ok(Caller {
                    sig: r.get(0)?,
                    kind: r.get(1)?,
                    path: r.get(2)?,
                    line: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let callers_total: i64 = count_stmt.query_row([id], |r| r.get(0))?;
        out.push(Symbol {
            sig,
            kind,
            line: s,
            end_line: e,
            callers_total,
            callers,
        });
    }
    Ok(out)
}

fn changes_for(root: &Path, db: &Path) -> anyhow::Result<ChangesResponse> {
    let conn = read_only(db)?;
    let head = git_stdout_bounded(Some(root), ["rev-parse", "HEAD"])
        .unwrap_or_default()
        .trim()
        .to_string();
    let base: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'last_commit'",
            [],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| head.clone());
    let base = if base.is_empty() { head.clone() } else { base };
    // The index may record an abbreviated id (`9738b7d69`); the full one is
    // what HEAD is compared with, so an index at HEAD does not read as behind.
    let base = git_stdout_bounded(
        Some(root),
        [
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{base}^{{commit}}"),
        ],
    )
    .unwrap_or(base);
    // `--` after the commit: a file or folder named like an abbreviated
    // commit id would otherwise make the revision ambiguous.
    let names = match git_text(
        root,
        &["diff", "--name-status", "-z", "--no-renames", &base, "--"],
    ) {
        // No output is a clean tree, not a failure.
        GitText::Out(s) => s,
        GitText::Failed(why) => anyhow::bail!("git diff against {base} failed: {why}"),
        GitText::NoAnswer => anyhow::bail!("git diff against {base} did not answer in time"),
    };
    let untracked = match git_text(root, &["ls-files", "--others", "--exclude-standard", "-z"]) {
        GitText::Out(s) => s,
        _ => String::new(),
    };
    // Paths left unquoted (quotePath=false) so they match the -z listing;
    // header_path still undoes the quoting git keeps for special characters.
    let (diff, diff_incomplete) = match git_text(
        root,
        &[
            "-c",
            "core.quotePath=false",
            "diff",
            "-U0",
            "--no-color",
            "--no-ext-diff",
            "--no-renames",
            &base,
            "--",
        ],
    ) {
        GitText::Out(s) => (s, false),
        // Too big or too slow for the bounded call: list the files anyway,
        // and say their symbols could not be mapped.
        _ => (String::new(), true),
    };
    let hunks = parse_hunk_sides(&diff);

    let mut list = parse_name_status(&names);
    list.extend(
        untracked
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(|p| ("untracked", p.replace('\\', "/"))),
    );
    // The index's own files never count as source changes.
    list.retain(|(_, p)| !p.starts_with(".travsr/"));
    let truncated = list.len() > MAX_FILES;
    let mut hash_stmt = conn.prepare("SELECT sha256 FROM files WHERE path = ?1")?;
    let mut files = Vec::new();
    for (status, path) in list.into_iter().take(MAX_FILES) {
        let stored: Option<String> = hash_stmt
            .query_row([&path], |r| r.get::<_, Option<String>>(0))
            .unwrap_or(None);
        let indexed = stored.is_some() || hash_stmt.exists([&path]).unwrap_or(false);
        let sides = hunks.get(&path).cloned().unwrap_or_default();
        let (ranges, index_between) =
            ranges_for_index(&sides, stored.as_deref(), &root.join(&path), || {
                base_as_checked_out(root, &base, &path)
            });
        let symbols = if indexed {
            touched_symbols(&conn, &path, &ranges)?
        } else {
            Vec::new()
        };
        files.push(ChangedFile {
            path,
            status,
            indexed,
            hunks: ranges.len(),
            symbols,
            index_between,
        });
    }
    Ok(ChangesResponse {
        base,
        head,
        files,
        truncated,
        diff_incomplete,
    })
}

/// The base commit's version of `path` as a checkout would write it.
///
/// The index hashes the bytes on disk, so the base must go through the same
/// eol and smudge filters before its hash can match: `git show` gives the raw
/// blob, which under `core.autocrlf=true` (the Git for Windows default) is LF
/// while the checkout is CRLF (re-review of #1098). `cat-file --filters`
/// applies the checkout conversion.
fn base_as_checked_out(root: &Path, base: &str, path: &str) -> Option<Vec<u8>> {
    git_output_bounded(
        Some(root),
        ["cat-file", "--filters", &format!("{base}:{path}")],
    )
    .filter(|o| o.status.success())
    .map(|o| o.stdout)
}

/// Hex SHA-256 of `bytes`, the hash the index stores per file.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// The side of the diff whose line numbers the index's nodes are in, and
/// whether the index holds neither side.
///
/// The diff is against the commit the index describes, but a running daemon
/// re-indexes every save, so a saved file's nodes carry working-tree line
/// numbers while `last_commit` stays put (review of #1098). The file's stored
/// hash says which version the index holds:
/// - the bytes on disk (the daemon's own `file_matches_index` test): the new
///   side;
/// - the base commit's version (`base_bytes`, read only when needed): the old
///   side;
/// - neither: a save indexed before the daemon stopped, then edited again
///   (re-review). That version is far closer to today's file than to the base,
///   so the new side is used and the file is flagged as approximate.
///
/// No stored hash (a file the index lacks) keeps the old side.
fn ranges_for_index(
    sides: &HunkSides,
    stored_hash: Option<&str>,
    on_disk: &Path,
    base_bytes: impl FnOnce() -> Option<Vec<u8>>,
) -> (Vec<(i64, i64)>, bool) {
    let Some(stored) = stored_hash else {
        return (sides.old.clone(), false);
    };
    let matches = |bytes: &[u8]| sha256_hex(bytes).eq_ignore_ascii_case(stored);
    if std::fs::read(on_disk).is_ok_and(|b| matches(&b)) {
        return (sides.new.clone(), false);
    }
    match base_bytes() {
        Some(base) if matches(&base) => (sides.old.clone(), false),
        // The base version cannot be read (a file added since): the index can
        // only hold a save of it, so it is the in-between case too.
        _ => (sides.new.clone(), true),
    }
}

/// What a bounded git call gave back.
enum GitText {
    /// Exit 0: its stdout, which may be empty.
    Out(String),
    /// A non-zero exit: its stderr, for the message.
    Failed(String),
    /// It could not start, or did not finish within the bound.
    NoAnswer,
}

/// `git <args>` in `root`, bounded like the rest of travsr's git calls. Unlike
/// `git_stdout_bounded`, an empty stdout is an answer: a clean tree has no
/// diff, and that must not read as "git failed".
fn git_text(root: &Path, args: &[&str]) -> GitText {
    match git_output_bounded(Some(root), args) {
        None => GitText::NoAnswer,
        Some(out) if out.status.success() => {
            GitText::Out(String::from_utf8_lossy(&out.stdout).into_owned())
        }
        Some(out) => GitText::Failed(String::from_utf8_lossy(&out.stderr).trim().to_string()),
    }
}

/// `GET /api/changes?repo=<repo>`
pub(super) async fn changes(
    State(state): State<Arc<UiState>>,
    Query(params): Query<BTreeMap<String, String>>,
) -> Result<Json<ChangesResponse>, ApiError> {
    let repo = resolve_repo(&state, params.get("repo").map(String::as_str))?;
    let (root, db) = (repo.root.clone(), repo.db_path());
    let work = tokio::task::spawn_blocking(move || changes_for(&root, &db));
    match tokio::time::timeout(Duration::from_secs(60), work).await {
        Err(_) => Err(ApiError(
            StatusCode::GATEWAY_TIMEOUT,
            "no answer within 60 s".into(),
        )),
        Ok(joined) => joined
            .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .map(Json)
            .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_status_reads_every_kind_and_takes_a_renames_new_path() {
        let out = "M\0src/a.rs\0A\0src/new.rs\0D\0old.rs\0R100\0x.rs\0y.rs\0";
        assert_eq!(
            parse_name_status(out),
            vec![
                ("modified", "src/a.rs".to_string()),
                ("added", "src/new.rs".to_string()),
                ("deleted", "old.rs".to_string()),
                ("renamed", "y.rs".to_string()),
            ]
        );
        assert!(parse_name_status("").is_empty());
    }

    #[test]
    fn hunks_give_old_side_ranges_including_pure_insertions() {
        let diff = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n\
                    @@ -10,3 +10,4 @@ fn x\n-a\n@@ -40 +41 @@\n@@ -50,0 +52,2 @@\n\
                    diff --git a/new.rs b/new.rs\n--- /dev/null\n+++ b/new.rs\n@@ -0,0 +1,5 @@\n";
        let h = parse_hunks(diff);
        assert_eq!(h["src/a.rs"], vec![(10, 12), (40, 40), (50, 51)]);
        assert_eq!(h["new.rs"], vec![(1, 2)]);
    }

    /// Review of #1098 (blocker 1): a running daemon re-indexes every save, so
    /// a saved file's nodes are in working-tree line numbers. Two lines were
    /// inserted above `b` (base lines 10..12, now 12..14) and `b`'s body edited.
    /// Matched against the stored lines, the old side (11) misses `b` (now
    /// 12..14); the new side (13) lands on it.
    #[test]
    fn a_daemon_reindexed_file_is_mapped_with_new_side_lines() {
        let diff = "diff --git a/m.rs b/m.rs\n--- a/m.rs\n+++ b/m.rs\n\
                    @@ -9,0 +10,2 @@\n+x\n+y\n@@ -11 +13 @@\n-old\n+new\n";
        let sides = &parse_hunk_sides(diff)["m.rs"];
        assert_eq!(sides.old, vec![(9, 10), (11, 11)]);
        assert_eq!(sides.new, vec![(10, 11), (13, 13)]);

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("m.rs");
        std::fs::write(&file, b"working tree bytes\n").unwrap();
        let disk_hash = {
            use sha2::{Digest, Sha256};
            Sha256::digest(b"working tree bytes\n")
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };
        let base = b"base commit bytes\n".to_vec();
        let base_hash = sha256_hex(&base);
        let base_fn = || Some(base.clone());
        // Index re-read the saved file: new side.
        assert_eq!(
            ranges_for_index(sides, Some(&disk_hash), &file, base_fn),
            (sides.new.clone(), false)
        );
        // Index still holds the base version: old side.
        assert_eq!(
            ranges_for_index(sides, Some(&base_hash), &file, base_fn),
            (sides.old.clone(), false)
        );
        // Re-review: the index holds a save that is neither (the daemon indexed
        // it, then stopped, and the file was edited again): new side, flagged.
        assert_eq!(
            ranges_for_index(sides, Some("00"), &file, base_fn),
            (sides.new.clone(), true)
        );
        assert_eq!(
            ranges_for_index(sides, None, &file, base_fn),
            (sides.old.clone(), false)
        );

        // Through the symbol mapping: nodes as the daemon stored them.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE nodes (id INTEGER PRIMARY KEY, path TEXT, signature TEXT, kind TEXT, line INTEGER, end_line INTEGER);
             CREATE TABLE edges (src INTEGER, dst INTEGER, kind TEXT);
             INSERT INTO nodes VALUES (1, 'm.rs', 'fn:a', 'function', 1, 9),
                                      (2, 'm.rs', 'fn:b', 'function', 12, 14);",
        )
        .unwrap();
        let sig = |ranges: &[(i64, i64)]| {
            touched_symbols(&conn, "m.rs", ranges)
                .unwrap()
                .into_iter()
                .map(|s| s.sig)
                .collect::<Vec<_>>()
        };
        // The insertion sits between a and b; the edit is in b.
        assert_eq!(
            sig(&ranges_for_index(sides, Some(&disk_hash), &file, base_fn).0),
            vec!["fn:b"]
        );
        assert!(
            !sig(&sides.old).contains(&"fn:b".to_string()),
            "old side misses the edit to b"
        );
    }

    /// Review fixes: content lines that look like headers, a path with a space
    /// (git ends it with a TAB) and a quoted non-ASCII path, all taken from
    /// what git actually prints.
    #[test]
    fn hunks_survive_header_lookalikes_and_quoted_paths() {
        let diff = "diff --git a/q.sql b/q.sql\n--- a/q.sql\n+++ b/q.sql\n\
                    @@ -1 +0,0 @@\n--- note\n@@ -5 +4 @@\n-x\n+++ added\n\
                    diff --git a/my file.rs b/my file.rs\n--- a/my file.rs\t\n+++ b/my file.rs\t\n@@ -2 +2 @@\n\
                    diff --git \"a/caf\\303\\251.rs\" \"b/caf\\303\\251.rs\"\n--- \"a/caf\\303\\251.rs\"\n+++ \"b/caf\\303\\251.rs\"\n@@ -7,2 +7,2 @@\n";
        let h = parse_hunks(diff);
        assert_eq!(h["q.sql"], vec![(1, 1), (5, 5)], "{h:?}");
        assert_eq!(h["my file.rs"], vec![(2, 2)], "{h:?}");
        assert_eq!(h["caf\u{e9}.rs"], vec![(7, 8)], "{h:?}");
        assert_eq!(h.len(), 3, "no stray file from a body line: {h:?}");
        assert_eq!(
            unquote_c("\"a\\\\b\\\"c\\t\"").as_deref(),
            Some("a\\b\"c\t")
        );
    }

    /// Review fix: a clean tree gives git no output, which is an answer.
    #[test]
    fn an_empty_git_diff_is_a_clean_tree_not_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .output()
                .unwrap()
        };
        git(&["init", "-q"]);
        std::fs::write(root.join("a.txt"), "x\n").unwrap();
        git(&["add", "."]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "i",
        ]);
        let head = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout).unwrap();
        let head = head.trim();
        assert!(matches!(
            git_text(root, &["diff", "--name-status", "-z", "--no-renames", head, "--"]),
            GitText::Out(ref s) if s.is_empty()
        ));
        assert!(matches!(
            git_text(root, &["diff", "no-such-commit", "--"]),
            GitText::Failed(_)
        ));
    }

    /// Re-review: under `core.autocrlf=true` the blob is LF and the checkout
    /// CRLF, so the base must be read with the checkout filters to match the
    /// hash the index took from disk.
    #[test]
    fn the_base_is_read_as_checked_out_under_autocrlf() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .output()
                .unwrap()
        };
        git(&["init", "-q"]);
        git(&["config", "core.autocrlf", "true"]);
        let crlf = b"fn a() {}\r\nfn b() {}\r\n";
        std::fs::write(root.join("m.rs"), crlf).unwrap();
        git(&["add", "."]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "i",
        ]);
        // The blob itself was normalized to LF on add.
        let raw = git(&["show", "HEAD:m.rs"]).stdout;
        assert_eq!(raw, b"fn a() {}\nfn b() {}\n");
        // Read as checked out, it hashes like the file the index read.
        let base = base_as_checked_out(root, "HEAD", "m.rs").unwrap();
        assert_eq!(sha256_hex(&base), sha256_hex(crlf));

        // So an index built at the commit maps a later edit with the old side.
        let sides = HunkSides {
            old: vec![(1, 1)],
            new: vec![(3, 3)],
        };
        std::fs::write(root.join("m.rs"), b"x\r\ny\r\nfn a() {}\r\n").unwrap();
        assert_eq!(
            ranges_for_index(&sides, Some(&sha256_hex(crlf)), &root.join("m.rs"), || {
                base_as_checked_out(root, "HEAD", "m.rs")
            }),
            (vec![(1, 1)], false)
        );
    }

    #[test]
    fn innermost_drops_containers_of_a_changed_member() {
        let kept = innermost(vec![
            (1, 100, "impl"),
            (10, 20, "method"),
            (50, 60, "other"),
        ]);
        let names: Vec<_> = kept.iter().map(|(_, _, n)| *n).collect();
        assert_eq!(names, vec!["method", "other"]);
    }

    #[test]
    fn touched_symbols_map_ranges_to_symbols_and_their_callers() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE nodes (id INTEGER PRIMARY KEY, path TEXT, signature TEXT, kind TEXT, line INTEGER, end_line INTEGER);
             CREATE TABLE edges (src INTEGER, dst INTEGER, kind TEXT);
             INSERT INTO nodes VALUES
               (1, 'src/a.rs', 'impl:S', 'impl', 1, 100),
               (2, 'src/a.rs', 'method:S.run', 'method', 10, 20),
               (3, 'src/a.rs', 'fn:other', 'function', 50, 60),
               (4, 'src/b.rs', 'fn:caller', 'function', 5, 9),
               (5, 'src/a.rs', 'use:x', 'import', 2, 2);
             INSERT INTO edges VALUES (4, 2, 'ref/call'), (4, 2, 'ref/call'), (2, 2, 'ref/call'), (4, 3, 'defines/binding');",
        )
        .unwrap();
        let syms = touched_symbols(&conn, "src/a.rs", &[(12, 13)]).unwrap();
        assert_eq!(syms.len(), 1, "{syms:?}");
        assert_eq!(syms[0].sig, "method:S.run");
        assert_eq!(syms[0].callers_total, 1);
        assert_eq!(syms[0].callers[0].sig, "fn:caller");
        // A range touching nothing indexed maps to nothing.
        assert!(touched_symbols(&conn, "src/a.rs", &[(200, 210)])
            .unwrap()
            .is_empty());
    }
}
