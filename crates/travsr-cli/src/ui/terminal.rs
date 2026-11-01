//! The Terminal page's two commands, `travsr ask` and `travsr graph`, run
//! exactly as typed in a shell so the page shows the CLI's own output.
//!
//! Only those two, and only the read-only options of each. The page never
//! sends an argv: it sends named fields, [`terminal_argv`] checks each one and
//! builds the argv from a fixed shape, and every user value goes after `--` or
//! as `--flag=value`, so no input can become an option. The child is this same
//! binary, run with no shell.
//!
//! Both commands fall back to a writable open when an index needs a schema
//! upgrade (the CLI's `open_read_store`); `travsr ui` never writes the index,
//! so a run starts only once a read-only open has succeeded here.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Serialize;

use super::files::{cli_slots, CLI_PROCESS_SLOTS, CLI_SLOT_WAIT};
use super::{open_index_read_only, resolve_repo, ApiError, UiState};

/// Longest a question or symbol may be, as `get_context` allows.
const MAX_QUERY: usize = 200;
/// Most output kept per stream: `graph --all --format json` on a large repo
/// runs to many megabytes, more than a page should hold.
const MAX_OUTPUT: usize = 2 * 1024 * 1024;

#[derive(Serialize)]
pub(super) struct RunResponse {
    /// The command as it would be typed, for the page to show and copy.
    command: String,
    stdout: String,
    /// The CLI's notes and warnings; shown apart from the answer.
    stderr: String,
    /// `None` when the command timed out.
    code: Option<i32>,
    /// Set when a stream was cut at [`MAX_OUTPUT`].
    truncated: bool,
    millis: u64,
}

/// What [`terminal_argv`] built: the argv after the binary name, its timeout,
/// and whether it reads the index.
#[derive(Debug, PartialEq)]
pub(super) struct Planned {
    pub args: Vec<String>,
    pub secs: u64,
    pub reads_index: bool,
}

/// A value from the page: present, short enough, one line.
fn text(params: &BTreeMap<String, String>, key: &str) -> Result<Option<String>, String> {
    let Some(v) = params.get(key).map(|s| s.trim()).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if v.chars().count() > MAX_QUERY {
        return Err(format!("{key} is longer than {MAX_QUERY} characters"));
    }
    if v.chars().any(char::is_control) {
        return Err(format!("{key} must be one line"));
    }
    Ok(Some(v.to_string()))
}

fn flag(params: &BTreeMap<String, String>, key: &str) -> bool {
    matches!(params.get(key).map(String::as_str), Some("1" | "true"))
}

/// The argv for `cmd` from the page's fields, or why it cannot run.
pub(super) fn terminal_argv(
    cmd: &str,
    params: &BTreeMap<String, String>,
) -> Result<Planned, String> {
    match cmd {
        "ask" => {
            if flag(params, "examples") {
                return Ok(Planned {
                    args: vec!["ask".into(), "--examples".into()],
                    secs: 60,
                    reads_index: true,
                });
            }
            let query = text(params, "query")?.ok_or("type a question or a symbol name")?;
            let mut args = vec!["ask".to_string()];
            if params.get("format").map(String::as_str) == Some("json") {
                args.push("--format=json".into());
            }
            args.extend(["--".to_string(), query]);
            // The same budget as the Context page's get_context: retrieval over
            // the whole graph, seconds with a daemon, minutes from a cold open.
            Ok(Planned {
                args,
                secs: 180,
                reads_index: true,
            })
        }
        "graph" => {
            let format = match params.get("format").map(String::as_str).unwrap_or("tree") {
                f @ ("tree" | "dot" | "json") => f,
                other => return Err(format!("unknown format '{other}'")),
            };
            let mut args = vec!["graph".to_string(), format!("--format={format}")];
            if flag(params, "all") {
                args.push("--all".into());
                return Ok(Planned {
                    args,
                    secs: 180,
                    reads_index: true,
                });
            }
            let query = text(params, "query")?.ok_or("type a symbol or file name")?;
            let depth = match params.get("depth") {
                None => 3,
                Some(d) => d
                    .parse::<u32>()
                    .ok()
                    .filter(|d| (1..=10).contains(d))
                    .ok_or("depth must be a number from 1 to 10")?,
            };
            let direction = match params
                .get("direction")
                .map(String::as_str)
                .unwrap_or("deps")
            {
                d @ ("deps" | "callers" | "both") => d,
                other => return Err(format!("unknown direction '{other}'")),
            };
            args.push(format!("--depth={depth}"));
            args.push(format!("--direction={direction}"));
            if let Some(path) = text(params, "path")? {
                args.push(format!("--path={path}"));
            }
            args.extend(["--".to_string(), query]);
            Ok(Planned {
                args,
                secs: 120,
                reads_index: true,
            })
        }
        other => Err(format!(
            "unknown command '{other}'; the Terminal runs ask and graph"
        )),
    }
}

/// `args` as a shell line, quoting what needs it, for display only.
fn shell_line(args: &[String]) -> String {
    let quote = |a: &String| {
        if !a.is_empty()
            && a.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_./=:,".contains(c))
        {
            a.clone()
        } else {
            format!("\"{}\"", a.replace('\\', "\\\\").replace('"', "\\\""))
        }
    };
    // `--` only guards the argv; a reader typing the command does not need it.
    let shown: Vec<String> = args.iter().filter(|a| *a != "--").map(quote).collect();
    format!("travsr {}", shown.join(" "))
}

/// At most [`MAX_OUTPUT`] bytes of `bytes`, cut on a character boundary.
fn capped(bytes: &[u8]) -> (String, bool) {
    let text = String::from_utf8_lossy(bytes);
    if text.len() <= MAX_OUTPUT {
        return (text.into_owned(), false);
    }
    let mut end = MAX_OUTPUT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

/// `GET /api/run/<ask|graph>?repo=<repo>&<field>=<value>...`
pub(super) async fn run(
    State(state): State<Arc<UiState>>,
    UrlPath(cmd): UrlPath<String>,
    Query(params): Query<BTreeMap<String, String>>,
) -> Result<Json<RunResponse>, ApiError> {
    let plan = terminal_argv(&cmd, &params).map_err(|e| ApiError(StatusCode::BAD_REQUEST, e))?;
    let repo = resolve_repo(&state, params.get("repo").map(String::as_str))?;
    if plan.reads_index && repo.db_path().exists() {
        let check = repo.clone();
        tokio::task::spawn_blocking(move || open_index_read_only(&check).map(drop))
            .await
            .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .map_err(|e| ApiError(StatusCode::CONFLICT, format!("{e:#}")))?;
    }
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
    let command = shell_line(&plan.args);
    let started = std::time::Instant::now();
    let child = tokio::process::Command::new(exe)
        .args(&plan.args)
        .current_dir(&repo.root)
        .env("TERM", "dumb")
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let resp = match tokio::time::timeout(Duration::from_secs(plan.secs), child).await {
        Ok(Ok(out)) => {
            let (stdout, cut_out) = capped(&out.stdout);
            let (stderr, cut_err) = capped(&out.stderr);
            RunResponse {
                command,
                stdout,
                stderr,
                code: out.status.code(),
                truncated: cut_out || cut_err,
                millis: started.elapsed().as_millis() as u64,
            }
        }
        Ok(Err(e)) => {
            return Err(ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("could not start travsr: {e}"),
            ))
        }
        Err(_) => RunResponse {
            command,
            stdout: String::new(),
            stderr: format!(
                "no answer within {} s; start the daemon (travsr daemon start) for fast answers",
                plan.secs
            ),
            code: None,
            truncated: false,
            millis: started.elapsed().as_millis() as u64,
        },
    };
    Ok(Json(resp))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn ask_puts_the_question_after_the_option_terminator() {
        let plan = terminal_argv("ask", &p(&[("query", "--format json")])).unwrap();
        assert_eq!(plan.args, vec!["ask", "--", "--format json"]);
        let plan = terminal_argv("ask", &p(&[("query", "x"), ("format", "json")])).unwrap();
        assert_eq!(plan.args, vec!["ask", "--format=json", "--", "x"]);
        assert_eq!(
            terminal_argv("ask", &p(&[("examples", "1")])).unwrap().args,
            vec!["ask", "--examples"]
        );
        assert!(terminal_argv("ask", &p(&[])).is_err());
    }

    #[test]
    fn graph_checks_every_field_and_binds_values_to_their_flags() {
        let plan = terminal_argv(
            "graph",
            &p(&[
                ("query", "-rf"),
                ("depth", "2"),
                ("direction", "callers"),
                ("path", "--all"),
                ("format", "dot"),
            ]),
        )
        .unwrap();
        assert_eq!(
            plan.args,
            vec![
                "graph",
                "--format=dot",
                "--depth=2",
                "--direction=callers",
                "--path=--all",
                "--",
                "-rf"
            ]
        );
        assert_eq!(
            terminal_argv("graph", &p(&[("all", "1")])).unwrap().args,
            vec!["graph", "--format=tree", "--all"]
        );
        for bad in [
            p(&[("query", "x"), ("depth", "0")]),
            p(&[("query", "x"), ("depth", "11")]),
            p(&[("query", "x"), ("direction", "up")]),
            p(&[("query", "x"), ("format", "svg")]),
            p(&[("query", "a\nb")]),
            p(&[]),
        ] {
            assert!(terminal_argv("graph", &bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn only_ask_and_graph_run() {
        for cmd in ["init", "daemon", "synonym", "config", "uninstall", ""] {
            assert!(terminal_argv(cmd, &p(&[("query", "x")])).is_err(), "{cmd}");
        }
    }

    #[test]
    fn overlong_values_are_refused() {
        let long = "a".repeat(MAX_QUERY + 1);
        assert!(terminal_argv("ask", &p(&[("query", &long)])).is_err());
    }

    #[test]
    fn the_shown_command_is_what_a_reader_would_type() {
        let plan = terminal_argv("graph", &p(&[("query", "run"), ("path", "src/a b.rs")])).unwrap();
        assert_eq!(
            shell_line(&plan.args),
            "travsr graph --format=tree --depth=3 --direction=deps \"--path=src/a b.rs\" run"
        );
        let plan = terminal_argv("ask", &p(&[("query", "where is \"x\" set")])).unwrap();
        assert_eq!(
            shell_line(&plan.args),
            "travsr ask \"where is \\\"x\\\" set\""
        );
    }

    #[test]
    fn output_is_cut_on_a_character_boundary() {
        let mut big = "a".repeat(MAX_OUTPUT - 1).into_bytes();
        big.extend("\u{e9}tail".as_bytes());
        let (s, cut) = capped(&big);
        assert!(cut);
        assert!(s.len() <= MAX_OUTPUT);
        assert!(s.chars().all(|c| c == 'a'));
        assert_eq!(capped(b"short"), ("short".to_string(), false));
    }
}
