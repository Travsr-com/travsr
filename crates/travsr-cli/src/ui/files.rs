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
/// writes: the page has no way to name an argument. `daemon status` and
/// `status --verbose` talk to the daemon themselves, exactly as they do when
/// the extension runs them.
const CLI_COMMANDS: &[(&str, &[&str], u64)] = &[
    ("version", &["--version"], 8),
    // Longer than the CLI's own 20 s pipe timeout, so a busy daemon is
    // reported in the CLI's words rather than cut off here.
    ("daemon-status", &["daemon", "status"], 30),
    ("embed-list", &["embed", "list", "--json"], 15),
    // `lang list` sweeps PATH per catalog language: ~17 s on a cold Windows
    // cache, which is why the extension gives it a minute.
    ("lang-list", &["lang", "list", "--json"], 60),
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

#[derive(Serialize, PartialEq, Debug)]
pub(super) struct VersionResponse {
    /// Changes whenever the index does. Opaque to the page: it only compares.
    fingerprint: String,
    last_commit: Option<String>,
}

/// `GET /api/version?repo=<repo>`: what the open pages poll to know the index
/// moved, the browser's version of the extension's `graph.db` file watcher.
///
/// Cheap enough to ask every few seconds: `graph.db`'s time and size, the WAL's
/// size (SQLite writes land in the WAL first, so `graph.db` itself can lag a
/// reindex by a checkpoint), plus the two commit markers read from a read-only
/// open. Never the daemon: a poll must not queue on its pipe.
///
/// The WAL's *time* is deliberately left out: opening the database, which this
/// very poll does, touches it, so every poll would read as a change and the
/// page would never settle. A real write grows the WAL; a checkpoint moves
/// `graph.db`. Either one changes the fingerprint.
pub(super) async fn version(
    State(state): State<Arc<UiState>>,
    Query(params): Query<BTreeMap<String, String>>,
) -> Result<Json<VersionResponse>, ApiError> {
    let repo = resolve_repo(&state, params.get("repo").map(String::as_str))?;
    tokio::task::spawn_blocking(move || {
        let db = repo.db_path();
        let mut wal = db.clone().into_os_string();
        wal.push("-wal");
        // Both stats are taken before the open below, which touches the WAL.
        let db_stamp = std::fs::metadata(&db)
            .ok()
            .map(|m| {
                let ms = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_millis());
                format!("{ms}:{}", m.len())
            })
            .unwrap_or_default();
        let wal_len = std::fs::metadata(Path::new(&wal)).map_or(0, |m| m.len());
        let (last_commit, phase_b) = match crate::daemon_client::open_read_store(&db) {
            Ok(store) => (
                store.get_meta("last_commit").ok().flatten(),
                store.get_meta("phase_b_commit").ok().flatten(),
            ),
            Err(_) => (None, None),
        };
        Json(VersionResponse {
            fingerprint: format!(
                "{}|{}|{}|{}",
                db_stamp,
                wal_len,
                last_commit.as_deref().unwrap_or(""),
                phase_b.as_deref().unwrap_or("")
            ),
            last_commit,
        })
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
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
    let def = line - 1;
    let start = def.saturating_sub(8);
    let end = (def + 25).min(all.len().saturating_sub(1));
    let lines = (start..=end)
        .filter_map(|i| {
            all.get(i).map(|t| SourceLine {
                no: i + 1,
                text: (*t).to_string(),
            })
        })
        .collect();
    Ok(Json(SourceResponse {
        path: rel,
        line,
        lines,
    }))
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
