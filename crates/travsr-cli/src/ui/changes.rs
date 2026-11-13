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
use crate::git_bounded::git_stdout_bounded;

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
}

#[derive(Serialize, Debug)]
pub(super) struct ChangesResponse {
    /// The commit the index describes, which the diff is taken against.
    base: String,
    head: String,
    files: Vec<ChangedFile>,
    truncated: bool,
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

/// Old-side line ranges (inclusive, 1-based) per file from a `-U0` diff. A
/// pure insertion (`-a,0`) touches the lines around position `a`, so the
/// symbol it lands in counts as changed.
pub(super) fn parse_hunks(diff: &str) -> BTreeMap<String, Vec<(i64, i64)>> {
    let mut out: BTreeMap<String, Vec<(i64, i64)>> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("--- ") {
            // The old side names the file as the index knows it; a new file
            // has /dev/null there and takes its name from `+++`.
            current = rest.strip_prefix("a/").map(str::to_string);
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            if current.is_none() {
                current = rest.strip_prefix("b/").map(str::to_string);
            }
        } else if let Some(rest) = line.strip_prefix("@@ -") {
            let Some(path) = current.clone() else {
                continue;
            };
            let old = rest.split_whitespace().next().unwrap_or("");
            let mut it = old.split(',');
            let start: i64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let count: i64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(1);
            let range = if count == 0 {
                (start.max(1), start.max(1) + 1)
            } else {
                (start, start + count - 1)
            };
            out.entry(path).or_default().push(range);
        }
    }
    out
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
    let names = git_stdout_bounded(
        Some(root),
        ["diff", "--name-status", "-z", "--no-renames", &base],
    )
    .ok_or_else(|| anyhow::anyhow!("git diff against {base} failed"))?;
    let untracked = git_stdout_bounded(
        Some(root),
        ["ls-files", "--others", "--exclude-standard", "-z"],
    )
    .unwrap_or_default();
    let diff = git_stdout_bounded(
        Some(root),
        [
            "diff",
            "-U0",
            "--no-color",
            "--no-ext-diff",
            "--no-renames",
            &base,
        ],
    )
    .unwrap_or_default();
    let hunks = parse_hunks(&diff);

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
    let mut indexed_stmt = conn.prepare("SELECT EXISTS(SELECT 1 FROM files WHERE path = ?1)")?;
    let mut files = Vec::new();
    for (status, path) in list.into_iter().take(MAX_FILES) {
        let indexed: bool = indexed_stmt.query_row([&path], |r| r.get(0))?;
        let ranges = hunks.get(&path).cloned().unwrap_or_default();
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
        });
    }
    Ok(ChangesResponse {
        base,
        head,
        files,
        truncated,
    })
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
