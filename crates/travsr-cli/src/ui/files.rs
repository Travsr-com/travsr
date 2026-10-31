//! The parts of the extension's pages that read the repo's own files or run
//! the `travsr` CLI rather than an MCP tool: the Health page's CLI-sourced
//! sections, its daemon log reader, and the Graph page's definition peek.
//!
//! Each is bounded the way the extension bounds it: a fixed allowlist of
//! read-only commands, log files named only from the directory listing, and
//! source reads confined to the selected repository.

use std::collections::BTreeMap;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Component, Path};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Serialize;

use super::{resolve_repo, ApiError, UiState};

/// The read-only commands the extension's Health page spawns, by the name the
/// page asks for. Nothing outside this table can be run, and none of these
/// writes: the page has no way to name an argument. The first four are
/// answered in-process (see [`in_process`]); the argv here is what they mean
/// and what the read-only test checks.
const CLI_COMMANDS: &[(&str, &[&str], u64)] = &[
    ("version", &["--version"], 8),
    // Longer than the CLI's own 20 s pipe timeout, so a busy daemon is
    // reported in the CLI's words rather than cut off here.
    ("daemon-status", &["daemon", "status"], 30),
    ("embed-list", &["embed", "list", "--json"], 15),
    // `lang list` sweeps PATH per catalog language: ~17 s on a cold Windows
    // cache, which is why the extension gives it a minute. Measured at 116 s
    // on a Windows machine whose endpoint scanner slows every process start,
    // so the page gives it three: the section shows "checking" meanwhile, and
    // a cut-off answer is the one outcome that helps nobody.
    ("lang-list", &["lang", "list", "--json"], 180),
    ("config-list", &["config", "list", "--json"], 8),
    ("status-verbose", &["status", "--verbose"], 45),
];

#[derive(Serialize)]
pub(super) struct CliResponse {
    /// stdout and stderr interleaved, for the human-facing readers.
    out: String,
    /// stdout alone, for the JSON readers: a stray stderr line must not break
    /// `JSON.parse`.
    stdout: String,
    /// `None` on timeout or spawn failure.
    code: Option<i32>,
}

/// `GET /api/cli/<name>?repo=<repo>`: run one allowlisted command in the repo.
pub(super) async fn cli(
    State(state): State<Arc<UiState>>,
    UrlPath(name): UrlPath<String>,
    Query(params): Query<BTreeMap<String, String>>,
) -> Result<Json<CliResponse>, ApiError> {
    let (_, args, secs) = CLI_COMMANDS
        .iter()
        .find(|(n, _, _)| *n == name)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("unknown command '{name}'")))?;
    let repo = resolve_repo(&state, params.get("repo").map(String::as_str))?;
    // This server is the `travsr` binary, so the commands that only format
    // what this process can read itself are answered here, from the same code
    // the CLI prints. Starting a process for them cost 4-5 s each on a slow
    // machine and queued the Daemon section behind `lang list`.
    if let Some(answer) = in_process(&name, &repo.root).await {
        return Ok(Json(CliResponse {
            out: answer.clone(),
            stdout: answer,
            code: Some(0),
        }));
    }
    // At most CLI_PROCESS_SLOTS of these run at once, server-wide. The page's
    // own throttle is per tab and advisory; a second tab, a script, or (with
    // --allow-remote) any client could otherwise start unbounded `lang list`
    // children, each sweeping PATH for up to three minutes. A request that
    // cannot get a slot in CLI_SLOT_WAIT is told so rather than queued forever.
    let _slot = match tokio::time::timeout(CLI_SLOT_WAIT, cli_slots().acquire()).await {
        Ok(Ok(permit)) => permit,
        _ => {
            return Err(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "{CLI_PROCESS_SLOTS} travsr commands are already running; try again shortly"
                ),
            ))
        }
    };
    let exe = std::env::current_exe()
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let child = tokio::process::Command::new(exe)
        .args(*args)
        .current_dir(&repo.root)
        .env("TERM", "dumb")
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let resp = match tokio::time::timeout(Duration::from_secs(*secs), child).await {
        Ok(Ok(out)) => {
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&out.stderr);
            CliResponse {
                out: format!("{stdout}{stderr}"),
                stdout,
                code: out.status.code(),
            }
        }
        Ok(Err(e)) => CliResponse {
            out: format!("error: {e}"),
            stdout: String::new(),
            code: None,
        },
        Err(_) => CliResponse {
            out: String::new(),
            stdout: String::new(),
            code: None,
        },
    };
    Ok(Json(resp))
}

/// Allowlisted commands that may run as processes at once, across every
/// client of this server.
const CLI_PROCESS_SLOTS: usize = 2;
/// Longest a request waits for one of those slots.
const CLI_SLOT_WAIT: Duration = Duration::from_secs(30);

fn cli_slots() -> &'static tokio::sync::Semaphore {
    static SLOTS: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    SLOTS.get_or_init(|| tokio::sync::Semaphore::new(CLI_PROCESS_SLOTS))
}

/// The output of an allowlisted command that needs no process of its own, or
/// `None` for the ones that do (`lang list`, `status --verbose`). Each line
/// ends the way the CLI's `println!` ends it.
async fn in_process(name: &str, root: &Path) -> Option<String> {
    let root = root.to_path_buf();
    let text = match name {
        "version" => format!("travsr {}", env!("CARGO_PKG_VERSION")),
        // Pipe I/O, up to the CLI's own 20 s timeout: off the async workers.
        "daemon-status" => tokio::task::spawn_blocking(move || crate::daemon_status_text(&root))
            .await
            .ok()?,
        "config-list" => crate::config::list_json(Some(&root)),
        "embed-list" => crate::embed::list_json(Some(&root)),
        _ => return None,
    };
    Some(text + "\n")
}

#[derive(Serialize, PartialEq, Debug)]
pub(super) struct VersionResponse {
    /// Changes whenever the index does. Opaque to the page: it only compares.
    fingerprint: String,
}

/// `GET /api/version?repo=<repo>`: what the open pages poll to know the index
/// moved, the browser's version of the extension's `graph.db` file watcher.
/// Never the daemon: a poll must not queue on its pipe.
///
/// The fingerprint is the index's *content*, not its files: see
/// [`index_fingerprint`]. File metadata cannot work here. Every reader moves
/// it: SQLite touches the WAL on open, and every `SqliteStore` runs
/// `wal_checkpoint(TRUNCATE)` when dropped, which resets the WAL and rewrites
/// `graph.db`. This server's own direct reads during a redraw would then read
/// as a change and redraw again; and after a checkpoint a small write can
/// leave the WAL's length unchanged, so a length test also misses changes.
pub(super) async fn version(
    State(state): State<Arc<UiState>>,
    Query(params): Query<BTreeMap<String, String>>,
) -> Result<Json<VersionResponse>, ApiError> {
    let repo = resolve_repo(&state, params.get("repo").map(String::as_str))?;
    tokio::task::spawn_blocking(move || {
        let fingerprint = index_fingerprint(&repo.db_path()).unwrap_or_default();
        Json(VersionResponse { fingerprint })
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// When any file was last reindexed, in ms since the epoch: the newest
/// `files.last_indexed_at` (whole seconds), over a read-only connection.
pub(super) fn last_indexed_ms(db: &Path) -> Option<u64> {
    let conn = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    conn.busy_timeout(Duration::from_secs(2)).ok()?;
    let secs: Option<i64> = conn
        .query_row("SELECT max(last_indexed_at) FROM files", [], |r| r.get(0))
        .ok()?;
    u64::try_from(secs?).ok().map(|s| s * 1000)
}

/// What changes when the index does, read through a plain read-only
/// connection that never checkpoints, so the poll writes nothing.
///
/// The two commit markers move on every commit and every semantic pass.
/// Between commits, `files.last_indexed_at` is stamped on every file the
/// watcher or a hook reindexes, even when its symbols (and so their node ids,
/// which are content hashes and come back identical) do not change but their
/// lines do; the node and edge counts cover deletions and the edges a
/// semantic pass or the live lane adds. `last_indexed_at` has one-second
/// resolution, so a second reindex in the same second as a poll that already
/// saw the first is picked up with the next change rather than at once.
fn index_fingerprint(db: &Path) -> rusqlite::Result<String> {
    let conn = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_secs(2))?;
    conn.query_row(
        "SELECT (SELECT value FROM meta WHERE key = 'last_commit'),
                (SELECT value FROM meta WHERE key = 'phase_b_commit'),
                (SELECT count(*) FROM nodes), (SELECT count(*) FROM edges),
                (SELECT count(*) FROM files), (SELECT max(last_indexed_at) FROM files)",
        [],
        |r| {
            Ok(format!(
                "{}|{}|{}|{}|{}|{}",
                r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, Option<i64>>(5)?.unwrap_or(0),
            ))
        },
    )
}

/// Most lines the log reader returns, the extension's `LOG_MAX_LINES`.
const LOG_MAX_LINES: usize = 5000;
/// Rotated files listed, the extension's `LOG_MAX_FILES_LISTED`.
const LOG_MAX_FILES_LISTED: usize = 7;
/// Furthest back one file is scanned, the extension's `LOG_MAX_BYTES_PER_FILE`.
const LOG_MAX_BYTES_PER_FILE: u64 = 8 * 1024 * 1024;

#[derive(Serialize)]
pub(super) struct LogFile {
    name: String,
    size: u64,
}

#[derive(Serialize)]
pub(super) struct LogsResponse {
    /// Newest first, capped at [`LOG_MAX_FILES_LISTED`].
    files: Vec<LogFile>,
    on_disk: usize,
    selected: Option<String>,
    /// Raw JSON lines of the selected file, oldest first.
    lines: Vec<String>,
}

/// `GET /api/logs?repo=<repo>&file=<name>&lines=<n>`: the tail of one rotated
/// `daemon.log.<date>` file, as the extension's Health page reads it.
pub(super) async fn logs(
    State(state): State<Arc<UiState>>,
    Query(params): Query<BTreeMap<String, String>>,
) -> Result<Json<LogsResponse>, ApiError> {
    let repo = resolve_repo(&state, params.get("repo").map(String::as_str))?;
    let want = params
        .get("lines")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(500)
        .clamp(1, LOG_MAX_LINES);
    let requested = params.get("file").cloned();
    let dir = repo.root.join(".travsr");
    tokio::task::spawn_blocking(move || {
        let names = daemon_log_files(&dir);
        let on_disk = names.len();
        let newest_first: Vec<&String> = names.iter().rev().take(LOG_MAX_FILES_LISTED).collect();
        let files = newest_first
            .iter()
            .map(|n| LogFile {
                name: (*n).clone(),
                size: std::fs::metadata(dir.join(n)).map(|m| m.len()).unwrap_or(0),
            })
            .collect();
        // Named only from the listing: a name the directory did not report
        // reads as the newest file, so a request cannot walk out of `.travsr`.
        let selected = requested
            .filter(|r| names.contains(r))
            .or_else(|| newest_first.first().map(|s| (*s).clone()));
        let lines = selected
            .as_ref()
            .map(|n| tail_lines(&dir.join(n), want))
            .unwrap_or_default();
        Json(LogsResponse {
            files,
            on_disk,
            selected,
            lines,
        })
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// Every rotated log file in `dir`, oldest first (ISO dates sort by name).
fn daemon_log_files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with("daemon.log"))
        .collect();
    names.sort();
    names
}

/// The last `want` complete lines of `file`, oldest first, reading at most
/// [`LOG_MAX_BYTES_PER_FILE`] from its end. A read that starts mid-file drops
/// the partial first line.
fn tail_lines(file: &Path, want: usize) -> Vec<String> {
    let Ok(mut f) = std::fs::File::open(file) else {
        return Vec::new();
    };
    let size = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = size.saturating_sub(LOG_MAX_BYTES_PER_FILE);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<&str> = text.lines().collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    let skip = lines.len().saturating_sub(want);
    lines[skip..]
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| (*l).to_string())
        .collect()
}

#[derive(Serialize)]
pub(super) struct SourceLine {
    no: usize,
    text: String,
}

#[derive(Serialize)]
pub(super) struct SourceResponse {
    path: String,
    line: usize,
    lines: Vec<SourceLine>,
}

/// Largest file the peek will read.
const SOURCE_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// `GET /api/source?repo=<repo>&path=<rel>&line=<n>`: the lines around a
/// definition, for the Graph page's peek panel (8 before, 25 after, as the
/// extension shows). Only repo-relative paths that stay inside the repo after
/// symlinks are resolved.
pub(super) async fn source(
    State(state): State<Arc<UiState>>,
    Query(params): Query<BTreeMap<String, String>>,
) -> Result<Json<SourceResponse>, ApiError> {
    let repo = resolve_repo(&state, params.get("repo").map(String::as_str))?;
    let rel = params.get("path").cloned().unwrap_or_default();
    let line = params
        .get("line")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(1)
        .max(1);
    let bad = |m: &str| ApiError(StatusCode::BAD_REQUEST, m.to_string());
    if !is_plain_relative(&rel) {
        return Err(bad("path must be relative to the repository"));
    }
    if is_private_dir(&rel) {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "the peek shows source files, not .git or .travsr".into(),
        ));
    }
    // Only files the index holds. The peek exists to show a definition the
    // graph points at, and every such file has a `files` row; anything else
    // under the root (a gitignored `.env`, a key, a build output) is not the
    // graph's to show, and with `--allow-remote` it would be served to
    // whoever can reach the port.
    let wanted = index_path(&rel);
    let indexed = {
        let db = repo.db_path();
        let wanted = wanted.clone();
        tokio::task::spawn_blocking(move || is_indexed_file(&db, &wanted))
            .await
            .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };
    if !matches!(indexed, Ok(true)) {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            format!("'{wanted}' is not a file in this repository's index"),
        ));
    }
    let root = std::fs::canonicalize(&repo.root).map_err(|e| bad(&e.to_string()))?;
    let full = std::fs::canonicalize(root.join(&rel))
        .map_err(|_| ApiError(StatusCode::NOT_FOUND, format!("no file '{rel}'")))?;
    if !full.starts_with(&root) {
        return Err(bad("path leaves the repository"));
    }
    // Again on the resolved path: a symlink in the tree can point into .git.
    let resolved_rel = full.strip_prefix(&root).unwrap_or(&full);
    if is_private_dir(&resolved_rel.to_string_lossy()) {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "the peek shows source files, not .git or .travsr".into(),
        ));
    }
    if std::fs::metadata(&full)
        .map(|m| m.len())
        .unwrap_or(u64::MAX)
        > SOURCE_MAX_BYTES
    {
        return Err(bad("file too large to preview"));
    }
    let text = std::fs::read(&full).map_err(|e| bad(&e.to_string()))?;
    // A NUL byte in the first 8 KiB is what git calls binary: there is no
    // definition to show in it, only bytes rendered as mojibake.
    if text.iter().take(8192).any(|b| *b == 0) {
        return Err(bad("not a text file"));
    }
    let text = String::from_utf8_lossy(&text);
    let all: Vec<&str> = text.lines().collect();
    let lines = peek_window(line, all.len())
        .map(|(start, end)| {
            (start..=end)
                .map(|i| SourceLine {
                    no: i + 1,
                    text: all[i].to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Json(SourceResponse {
        path: rel,
        line,
        lines,
    }))
}

/// `rel` in the form the index stores paths (`files.path`, a node's own
/// path): `/` separators, no leading `./`.
fn index_path(rel: &str) -> String {
    let p = rel.replace('\\', "/");
    let mut s = p.as_str();
    while let Some(rest) = s.strip_prefix("./") {
        s = rest;
    }
    s.to_string()
}

/// Whether the index has a `files` row for `path`, over a plain read-only
/// connection (no `SqliteStore`, whose drop checkpoints).
fn is_indexed_file(db: &Path, path: &str) -> rusqlite::Result<bool> {
    let conn = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_secs(2))?;
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM files WHERE path = ?1)",
        [path],
        |r| r.get(0),
    )
}

/// The 0-based, inclusive line range the peek shows around 1-based `line`: 8
/// before and 25 after, as the extension shows. `line` comes from the query
/// string, so it is clamped to the file first: a huge value is the file's last
/// line, never an overflow (which wrapped in release builds and panicked in
/// debug ones). `None` for an empty file.
fn peek_window(line: usize, total: usize) -> Option<(usize, usize)> {
    let last = total.checked_sub(1)?;
    let def = line.saturating_sub(1).min(last);
    Some((def.saturating_sub(8), def.saturating_add(25).min(last)))
}

/// Under `.git` or `.travsr`, at any depth and in any letter case (Windows and
/// macOS file systems are case-insensitive). Neither holds source: `.git` holds
/// config that can carry credentials, `.travsr` the index and its logs.
fn is_private_dir(p: &str) -> bool {
    Path::new(p).components().any(|c| match c {
        Component::Normal(s) => {
            let s = s.to_string_lossy().to_ascii_lowercase();
            s == ".git" || s == ".travsr"
        }
        _ => false,
    })
}

/// Relative, with no `..`, root or prefix component.
fn is_plain_relative(p: &str) -> bool {
    !p.is_empty()
        && Path::new(p)
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_relative_paths_are_accepted() {
        assert!(is_plain_relative("src/main.rs"));
        assert!(is_plain_relative("./a.ts"));
        assert!(!is_plain_relative(""));
        assert!(!is_plain_relative("../etc/passwd"));
        assert!(!is_plain_relative("src/../../x"));
        assert!(!is_plain_relative("/etc/passwd"));
        #[cfg(windows)]
        assert!(!is_plain_relative("C:\\Windows\\win.ini"));
    }

    #[test]
    fn fingerprint_follows_content_and_never_writes() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("graph.db");
        let w = rusqlite::Connection::open(&db).unwrap();
        w.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE nodes (id INTEGER PRIMARY KEY, sig TEXT);
             CREATE TABLE edges (src INTEGER, dst INTEGER, kind TEXT);
             CREATE TABLE files (path TEXT PRIMARY KEY, sha256 TEXT, last_indexed_at INTEGER);
             INSERT INTO meta VALUES ('last_commit', 'aaa');
             INSERT INTO nodes VALUES (11, 'a'), (22, 'b');
             INSERT INTO files VALUES ('src/a.rs', 'h1', 1000);",
        )
        .unwrap();
        let wal = dir.path().join("graph.db-wal");
        let wal_before = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);

        let first = index_fingerprint(&db).unwrap();
        assert_eq!(
            index_fingerprint(&db).unwrap(),
            first,
            "reads alone must not move it"
        );
        assert_eq!(
            std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0),
            wal_before,
            "the poll must not checkpoint or otherwise write"
        );

        // The case the review's max(rowid) idea misses: a reindex that keeps
        // the same symbols writes the same content-hash node ids back, so
        // counts and ids are unchanged; only the file's stamp moves.
        w.execute_batch(
            "DELETE FROM nodes WHERE id = 22; INSERT INTO nodes VALUES (22, 'b');
             UPDATE files SET sha256 = 'h2', last_indexed_at = 1001 WHERE path = 'src/a.rs';",
        )
        .unwrap();
        let reindexed = index_fingerprint(&db).unwrap();
        assert_ne!(reindexed, first);

        w.execute("INSERT INTO edges VALUES (11, 22, 'ref/call')", [])
            .unwrap();
        assert_ne!(
            index_fingerprint(&db).unwrap(),
            reindexed,
            "a new edge is a change"
        );
    }

    #[test]
    fn only_indexed_files_can_be_peeked() {
        // The review's case: `.env` sits in the tree (gitignored) but the
        // index holds only `src/main.rs`.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("graph.db");
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute_batch(
                "CREATE TABLE files (path TEXT PRIMARY KEY, sha256 TEXT, last_indexed_at INTEGER);
                 INSERT INTO files VALUES ('src/main.rs', 'h', 1);",
            )
            .unwrap();
        assert!(is_indexed_file(&db, &index_path("src/main.rs")).unwrap());
        assert!(is_indexed_file(&db, &index_path("./src/main.rs")).unwrap());
        assert!(is_indexed_file(&db, &index_path("src\\main.rs")).unwrap());
        assert!(!is_indexed_file(&db, &index_path(".env")).unwrap());
        assert!(!is_indexed_file(&db, &index_path("src/main.rs.bak")).unwrap());
    }

    #[test]
    fn peek_window_is_clamped_to_the_file() {
        // The reviewed overflow: a line number at usize::MAX.
        assert_eq!(peek_window(usize::MAX, 100), Some((91, 99)));
        assert_eq!(peek_window(50, 100), Some((41, 74)));
        assert_eq!(peek_window(1, 100), Some((0, 25)));
        assert_eq!(peek_window(0, 100), Some((0, 25)));
        assert_eq!(peek_window(3, 5), Some((0, 4)));
        assert_eq!(peek_window(10, 0), None);
    }

    #[test]
    fn git_and_travsr_dirs_are_private() {
        assert!(is_private_dir(".git/config"));
        assert!(is_private_dir(".GIT/config"));
        assert!(is_private_dir(".travsr/graph.db"));
        assert!(is_private_dir("sub/.git/HEAD"));
        assert!(!is_private_dir("src/main.rs"));
        assert!(!is_private_dir(".github/workflows/ci.yml"));
        assert!(!is_private_dir("src/.gitignore"));
    }

    #[test]
    fn tail_keeps_the_newest_lines_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("daemon.log.2026-10-25");
        std::fs::write(&f, "a\nb\nc\nd\n").unwrap();
        assert_eq!(tail_lines(&f, 2), vec!["c", "d"]);
        assert_eq!(tail_lines(&f, 10), vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn log_files_are_listed_oldest_first_and_files_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("daemon.log.2026-10-24"), "").unwrap();
        std::fs::write(dir.path().join("daemon.log.2026-10-25"), "").unwrap();
        std::fs::write(dir.path().join("graph.db"), "").unwrap();
        std::fs::create_dir(dir.path().join("daemon.log.dir")).unwrap();
        assert_eq!(
            daemon_log_files(dir.path()),
            vec!["daemon.log.2026-10-24", "daemon.log.2026-10-25"]
        );
    }

    #[test]
    fn every_cli_command_is_read_only() {
        // The page can only name these; a writing subcommand here would be a
        // write reachable from a browser.
        for (_, args, _) in CLI_COMMANDS {
            assert!(
                !args.iter().any(|a| matches!(
                    *a,
                    "init" | "install" | "remove" | "set" | "unset" | "start" | "stop" | "restart"
                )),
                "{args:?}"
            );
        }
    }
}
