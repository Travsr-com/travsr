//! The page's action buttons: the commands the Health and Repos pages used to
//! copy (start the daemon, reindex, fsck, register agents, ...), run here.
//!
//! These change things (the index, the daemon, agent config), so the rest of
//! `travsr ui` stays read-only and only this endpoint can. It is guarded three
//! ways, because a loopback bind keeps other machines out but not other
//! websites open in the same browser, which can send requests to localhost:
//!
//! - **A per-session token** in an `X-Travsr-Token` header. Another site
//!   cannot read `/api/session` (no CORS headers) and cannot set a custom
//!   header without a preflight this server never answers.
//! - **The Origin check:** a request carrying an Origin must come from this
//!   server's own origin.
//! - **No actions at all under `--allow-remote`**, where anyone who can reach
//!   the port would otherwise run them.
//!
//! Only the commands in [`action_argv`] run, with a fixed argv; the two that
//! take a value (a language, a repo name) check it against a narrow pattern.
//! One action runs at a time. A slow one (`init` on a big repo) is left to
//! finish if the page goes away rather than killed halfway through.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::Json;
use serde::Serialize;

use super::terminal::capped;
use super::{is_loopback_host_header, resolve_repo, ApiError, UiState};

/// The header carrying the session token.
pub(super) const TOKEN_HEADER: &str = "x-travsr-token";

#[derive(Serialize)]
pub(super) struct SessionResponse {
    /// Whether the action buttons may run commands here.
    actions: bool,
    /// The token `/api/action` expects; absent when actions are off.
    #[serde(skip_serializing_if = "Option::is_none")]
    token: Option<String>,
}

/// `GET /api/session`: whether actions are on, and the token for them. Only a
/// page on this origin can read the answer.
pub(super) async fn session(State(state): State<Arc<UiState>>) -> Json<SessionResponse> {
    Json(SessionResponse {
        actions: state.action_token.is_some(),
        token: state.action_token.clone(),
    })
}

#[derive(Serialize)]
pub(super) struct ActionResponse {
    command: String,
    stdout: String,
    stderr: String,
    /// `None` when it was still running at the deadline (it keeps running).
    code: Option<i32>,
    truncated: bool,
    millis: u64,
}

/// The argv for action `name` and its deadline, or why it cannot run.
pub(super) fn action_argv(
    name: &str,
    params: &BTreeMap<String, String>,
) -> Result<(Vec<String>, u64), String> {
    let fixed = |args: &[&str], secs: u64| Ok((args.iter().map(|s| s.to_string()).collect(), secs));
    let value = |key: &str| -> Result<String, String> {
        let v = params.get(key).map(|s| s.trim()).unwrap_or("");
        let ok = !v.is_empty()
            && v.len() <= 100
            && !v.starts_with('-')
            && v.chars()
                .all(|c| c.is_ascii_alphanumeric() || "._+#-".contains(c));
        if ok {
            Ok(v.to_string())
        } else {
            Err(format!("{key} must be a plain name"))
        }
    };
    match name {
        "daemon-start" => fixed(&["daemon", "start"], 60),
        "daemon-stop" => fixed(&["daemon", "stop"], 60),
        "daemon-restart" => fixed(&["daemon", "restart"], 90),
        // Reindex / Set up again / Install: init is incremental.
        "init" => fixed(&["init"], 30 * 60),
        "init-force" => fixed(&["init", "--force"], 60 * 60),
        "fsck" => fixed(&["fsck"], 5 * 60),
        "fsck-fix" => fixed(&["fsck", "--fix"], 10 * 60),
        "connect" => fixed(&["connect"], 120),
        // The Sidecars section's Install and Change model.
        "embed-init" => fixed(&["embed", "init"], 30 * 60),
        "embed-list" => fixed(&["embed", "list"], 60),
        "lang-remove" => Ok((vec!["lang".into(), "remove".into(), value("language")?], 60)),
        "repos-prune" => fixed(&["repos", "--prune"], 60),
        "repos-remove" => Ok((vec!["repos".into(), "--remove".into(), value("name")?], 60)),
        other => Err(format!("unknown action '{other}'")),
    }
}

/// Whether `origin` is this server's own: `http://<the Host the request
/// named>`, and a loopback name.
fn same_origin(origin: &str, host: &str) -> bool {
    origin
        .strip_prefix("http://")
        .is_some_and(|o| o.eq_ignore_ascii_case(host) && is_loopback_host_header(o))
}

fn running() -> &'static tokio::sync::Mutex<()> {
    static RUNNING: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    RUNNING.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// `POST /api/action/<name>?repo=<repo>[&language=..|&name=..]`
pub(super) async fn action(
    State(state): State<Arc<UiState>>,
    UrlPath(name): UrlPath<String>,
    Query(params): Query<BTreeMap<String, String>>,
    headers: HeaderMap,
) -> Result<Json<ActionResponse>, ApiError> {
    let forbidden = |m: &str| ApiError(StatusCode::FORBIDDEN, m.to_string());
    let Some(token) = state.action_token.as_deref() else {
        return Err(forbidden(
            "actions are off: travsr ui runs commands only on a loopback address, not with --allow-remote",
        ));
    };
    let sent = headers.get(TOKEN_HEADER).and_then(|v| v.to_str().ok());
    if sent != Some(token) {
        return Err(forbidden("missing or wrong session token; reload the page"));
    }
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        let host = headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !same_origin(origin, host) {
            return Err(forbidden("actions run only from this page"));
        }
    }
    let (args, secs) =
        action_argv(&name, &params).map_err(|e| ApiError(StatusCode::BAD_REQUEST, e))?;
    let repo = resolve_repo(&state, params.get("repo").map(String::as_str))?;
    let Ok(_one) = running().try_lock() else {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "another action is still running; wait for it to finish".into(),
        ));
    };
    let exe = std::env::current_exe()
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let command = format!("travsr {}", args.join(" "));
    tracing::info!(%command, repo = %repo.name, "travsr ui action");
    let started = std::time::Instant::now();
    let child = tokio::process::Command::new(exe)
        .args(&args)
        .current_dir(&repo.root)
        .env("TERM", "dumb")
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Left running if the page goes away or the deadline passes: killing
        // `init` or `fsck --fix` halfway is worse than letting it finish.
        .kill_on_drop(false)
        .spawn()
        .map_err(|e| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("could not start travsr: {e}"),
            )
        })?;
    let resp = match tokio::time::timeout(Duration::from_secs(secs), child.wait_with_output()).await
    {
        Ok(Ok(out)) => {
            let (stdout, a) = capped(&out.stdout);
            let (stderr, b) = capped(&out.stderr);
            ActionResponse {
                command,
                stdout,
                stderr,
                code: out.status.code(),
                truncated: a || b,
                millis: started.elapsed().as_millis() as u64,
            }
        }
        Ok(Err(e)) => {
            return Err(ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("travsr failed: {e}"),
            ))
        }
        Err(_) => ActionResponse {
            command,
            stdout: String::new(),
            stderr: format!(
                "still running after {secs} s; it carries on in the background. Refresh later to see the result."
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
    fn only_the_listed_actions_run_with_fixed_argv() {
        assert_eq!(action_argv("init", &p(&[])).unwrap().0, vec!["init"]);
        assert_eq!(
            action_argv("init-force", &p(&[])).unwrap().0,
            vec!["init", "--force"]
        );
        assert_eq!(
            action_argv("lang-remove", &p(&[("language", "java")]))
                .unwrap()
                .0,
            vec!["lang", "remove", "java"]
        );
        assert_eq!(
            action_argv("repos-remove", &p(&[("name", ".tmpAbC123")]))
                .unwrap()
                .0,
            vec!["repos", "--remove", ".tmpAbC123"]
        );
        for bad in ["uninstall", "ui", "serve", "config", "", "../init"] {
            assert!(action_argv(bad, &p(&[])).is_err(), "{bad}");
        }
    }

    #[test]
    fn values_cannot_become_options_or_paths() {
        for v in ["--all", "-rf", "a b", "../x", "a/b", "c:\\x", "", "x;y"] {
            assert!(
                action_argv("lang-remove", &p(&[("language", v)])).is_err(),
                "{v}"
            );
            assert!(
                action_argv("repos-remove", &p(&[("name", v)])).is_err(),
                "{v}"
            );
        }
        assert!(action_argv("lang-remove", &p(&[])).is_err());
    }

    #[test]
    fn only_this_servers_own_origin_is_accepted() {
        assert!(same_origin("http://localhost:7330", "localhost:7330"));
        assert!(same_origin("http://127.0.0.1:7330", "127.0.0.1:7330"));
        assert!(!same_origin("http://evil.example", "localhost:7330"));
        assert!(!same_origin("http://localhost:9999", "localhost:7330"));
        assert!(!same_origin("https://localhost:7330", "localhost:7330"));
        assert!(!same_origin("null", "localhost:7330"));
    }
}
