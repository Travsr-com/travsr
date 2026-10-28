//! `travsr ui`: a read-only browser view of the code graph (#1036).
//!
//! A small HTTP server in the CLI, not on the daemon: the daemon stays a
//! loopback IPC endpoint and gains no network surface. Every answer comes
//! through the same route the CLI uses ([`daemon_client::try_query`], falling
//! back to a direct open when no daemon is running: read-only, or writable
//! when migrations are pending, as in the CLI), and every graph answer
//! is an MCP tool's own text via [`travsr_mcp::browse_tool_call`], so the page
//! and an agent asking the same question see the same result.
//!
//! The pages are the VS Code extension's own: the Graph page runs the
//! extension's `media/graph.js`, `graph.css` and Cytoscape unchanged behind a
//! small `acquireVsCodeApi` shim, and the Health, Callers, Dependencies, Blast
//! radius and Repos pages reproduce the extension's webviews on its design
//! tokens, fed by the same MCP tools and the same read-only CLI commands the
//! extension runs. Everything is compiled into the binary; there is no build
//! step and nothing to install.
//!
//! Exposure: binds loopback by default and refuses a non-loopback address
//! without `--allow-remote`. While bound to loopback, requests whose `Host`
//! header is not a loopback name are rejected, which is what defeats DNS
//! rebinding (a page on another origin resolving its own name to 127.0.0.1).
//! The API is GET-only and read-only, so there is no token: any local process
//! able to reach the port can already read `.travsr/graph.db` directly.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Path as UrlPath, Query, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use travsr_mcp::query::{self, StatusPayload, ToolPayload, ToolQueryArgs};

use crate::daemon_client;

mod files;

const HEALTH_HTML: &str = include_str!("assets/health.html");
const PAGES_HTML: &str = include_str!("assets/pages.html");
const GRAPH_HTML: &str = include_str!("assets/graph.html");
const SHELL_CSS: &str = include_str!("assets/shell.css");
const NAV_CSS: &str = include_str!("assets/nav.css");
const COMMON_JS: &str = include_str!("assets/common.js");
const HEALTH_JS: &str = include_str!("assets/health.js");
const PAGES_JS: &str = include_str!("assets/pages.js");
const GRAPH_HOST_JS: &str = include_str!("assets/graph-host.js");

// The Graph page is the extension's, byte for byte: one source of truth, so a
// change to the extension's graph view reaches `travsr ui` on the next build.
const GRAPH_JS: &str = include_str!("../../../../packages/travsr-vscode/media/graph.js");
const GRAPH_CSS: &str = include_str!("../../../../packages/travsr-vscode/media/graph.css");
const CYTOSCAPE_JS: &str =
    include_str!("../../../../packages/travsr-vscode/media/vendor/cytoscape.min.js");
const ICON_PNG: &[u8] = include_bytes!("../../../../packages/travsr-vscode/icon.png");

/// Content Security Policy for every response: scripts only from this origin
/// (no inline script anywhere), requests only to this origin, never framed.
/// Inline styles are allowed because the extension's graph view and its
/// Cytoscape renderer set `style` attributes, exactly as its webview CSP does.
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
                   connect-src 'self'; img-src 'self' data: blob:; base-uri 'none'; \
                   form-action 'none'; frame-ancestors 'none'";

struct UiState {
    /// The repo `travsr ui` was started in, preselected in the picker.
    current: Option<NamedRepo>,
    /// Whether the Host-header check applies (loopback bind).
    check_host: bool,
}

#[derive(Clone)]
struct NamedRepo {
    name: String,
    root: PathBuf,
}

impl NamedRepo {
    fn db_path(&self) -> PathBuf {
        self.root.join(".travsr").join("graph.db")
    }
}

/// Start the server and block until Ctrl-C.
pub async fn run(host: String, port: u16, allow_remote: bool, open: bool) -> anyhow::Result<()> {
    let ip = parse_host(&host)?;
    if !ip.is_loopback() && !allow_remote {
        anyhow::bail!(
            "refusing to bind {ip}: travsr ui serves your code graph over plain HTTP with no \
             authentication. Pass --allow-remote to expose it beyond this machine."
        );
    }
    if !ip.is_loopback() {
        tracing::warn!(%ip, "travsr ui is reachable from the network without authentication");
    }

    let current = std::env::current_dir()
        .ok()
        .and_then(|cwd| crate::repo::find_git_root(&cwd).ok())
        .map(|root| NamedRepo {
            name: repo_display_name(&root),
            root,
        });
    let state = Arc::new(UiState {
        current,
        check_host: ip.is_loopback(),
    });

    let addr = SocketAddr::new(ip, port);
    let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            anyhow::anyhow!("port {port} is already in use; pass --port <other> to pick another")
        } else {
            anyhow::anyhow!("cannot bind to {addr}: {e}")
        }
    })?;
    let bound = listener.local_addr()?;
    let url = page_url(bound);

    println!("travsr ui: serving {url}");
    match &state.current {
        Some(r) if r.db_path().is_file() => println!("repository: {}", r.root.display()),
        Some(r) => println!(
            "repository: {} (not indexed yet; run `travsr init` there)",
            r.root.display()
        ),
        None => println!("not inside a repository; pick an indexed repo in the page"),
    }
    println!("press Ctrl-C to stop");
    if open {
        open_browser(&url);
    }

    axum::serve(listener, router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    println!("travsr ui: stopped");
    Ok(())
}

const HTML: &str = "text/html; charset=utf-8";
const JS: &str = "text/javascript; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";

fn router(state: Arc<UiState>) -> Router {
    Router::new()
        // Pages, one per extension webview.
        .route("/", get(|| async { asset(HTML, HEALTH_HTML) }))
        .route("/health", get(|| async { asset(HTML, HEALTH_HTML) }))
        .route("/graph", get(|| async { asset(HTML, GRAPH_HTML) }))
        .route("/callers", get(|| async { asset(HTML, PAGES_HTML) }))
        .route("/dependencies", get(|| async { asset(HTML, PAGES_HTML) }))
        .route("/blast", get(|| async { asset(HTML, PAGES_HTML) }))
        .route("/repos", get(|| async { asset(HTML, PAGES_HTML) }))
        // Assets.
        .route("/assets/shell.css", get(|| async { asset(CSS, SHELL_CSS) }))
        .route("/assets/nav.css", get(|| async { asset(CSS, NAV_CSS) }))
        .route("/assets/common.js", get(|| async { asset(JS, COMMON_JS) }))
        .route("/assets/health.js", get(|| async { asset(JS, HEALTH_JS) }))
        .route("/assets/pages.js", get(|| async { asset(JS, PAGES_JS) }))
        .route(
            "/assets/graph-host.js",
            get(|| async { asset(JS, GRAPH_HOST_JS) }),
        )
        .route("/assets/graph.js", get(|| async { asset(JS, GRAPH_JS) }))
        .route("/assets/graph.css", get(|| async { asset(CSS, GRAPH_CSS) }))
        .route(
            "/assets/cytoscape.min.js",
            get(|| async { asset(JS, CYTOSCAPE_JS) }),
        )
        .route(
            "/assets/icon.png",
            get(|| async { ([(header::CONTENT_TYPE, "image/png")], ICON_PNG).into_response() }),
        )
        // API. GET only; nothing here writes.
        .route("/api/repos", get(repos))
        .route("/api/status", get(status))
        .route("/api/version", get(files::version))
        .route("/api/tool/:tool", get(tool))
        .route("/api/cli/:name", get(files::cli))
        .route("/api/logs", get(files::logs))
        .route("/api/source", get(files::source))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

/// `--host` as an address: an IP literal (`[::1]` brackets allowed), or
/// `localhost`, the loopback name the printed URL and the Host check use.
/// Other names are refused rather than resolved: what a name resolves to can
/// change, and the loopback-only default must not depend on DNS.
fn parse_host(host: &str) -> anyhow::Result<IpAddr> {
    if host.eq_ignore_ascii_case("localhost") {
        return Ok(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    }
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .map_err(|_| anyhow::anyhow!("--host must be an IP address or localhost, got '{host}'"))
}

fn page_url(addr: SocketAddr) -> String {
    match addr.ip() {
        // `localhost` resolves to 127.0.0.1 only, not the rest of 127/8.
        IpAddr::V4(ip) if ip == std::net::Ipv4Addr::LOCALHOST => {
            format!("http://localhost:{}/", addr.port())
        }
        IpAddr::V6(ip) => format!("http://[{ip}]:{}/", addr.port()),
        IpAddr::V4(ip) => format!("http://{ip}:{}/", addr.port()),
    }
}

/// Reject non-loopback `Host` headers on a loopback bind, and add the security
/// headers to every response.
async fn guard(State(state): State<Arc<UiState>>, req: Request, next: Next) -> Response {
    if state.check_host {
        let host = req
            .headers()
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        if !is_loopback_host_header(host) {
            return (
                StatusCode::MISDIRECTED_REQUEST,
                "travsr ui answers loopback names only",
            )
                .into_response();
        }
    }
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

/// Whether a `Host` header names this machine: `localhost`, a `*.localhost`
/// name, or a loopback IP, with or without a port.
fn is_loopback_host_header(host: &str) -> bool {
    let name = if let Some(rest) = host.strip_prefix('[') {
        // `[::1]:port`
        match rest.split_once(']') {
            Some((ip, _)) => ip,
            None => return false,
        }
    } else {
        host.rsplit_once(':').map_or(host, |(name, _)| name)
    };
    let lower = name.to_ascii_lowercase();
    if lower == "localhost" || lower.ends_with(".localhost") {
        return true;
    }
    lower.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn asset(content_type: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type)], body).into_response()
}

// ── API ──────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct RepoRow {
    name: String,
    root: String,
    indexed: bool,
    current: bool,
}

#[derive(Serialize)]
struct ReposResponse {
    repos: Vec<RepoRow>,
}

/// Every repo the page may open: the registry's entries plus the one `travsr
/// ui` started in. Resolved fresh per request so `travsr init` elsewhere shows
/// up on reload, and the page can only ever name a repo from this list, never
/// a path of its choosing.
fn known_repos(state: &UiState) -> Vec<NamedRepo> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = travsr_store::registry::all_entries() {
        for (_, entry) in entries {
            // A registry entry whose index is gone (a deleted checkout, a test's
            // temp dir) has nothing to show; `travsr repos --prune` drops them.
            if !entry.db_path.is_file() {
                continue;
            }
            if let Some(root) = entry.db_path.parent().and_then(Path::parent) {
                roots.push(root.to_path_buf());
            }
        }
    }
    if let Some(cur) = &state.current {
        roots.push(cur.root.clone());
    }
    name_repos(roots)
}

/// One plain path per repo, named as `travsr repos` and `repos_list` name it:
/// the folder name. The registry keys its entries by the full canonical root,
/// which made URLs carry whole paths and kept the Repos page (which lists
/// `repos_list` names) from ever matching a row to a repo.
///
/// Roots are compared without Windows' `\\?\` prefix and, on Windows, without
/// case, so the registry's canonical form and the cwd-derived root of the
/// repo `travsr ui` started in are one repo, not two. Two repos with the same
/// folder name get the parent folder appended, and the full path if that
/// still collides, so every name resolves to exactly one repo.
fn name_repos(roots: Vec<PathBuf>) -> Vec<NamedRepo> {
    let mut by_key: BTreeMap<String, PathBuf> = BTreeMap::new();
    for root in roots {
        by_key
            .entry(root_key(&root))
            .or_insert_with(|| plain_path(&root));
    }
    let roots: Vec<PathBuf> = by_key.into_values().collect();
    let count = |names: &[String], n: &str| names.iter().filter(|x| x.as_str() == n).count();
    let base: Vec<String> = roots.iter().map(|r| repo_display_name(r)).collect();
    let with_parent: Vec<String> = roots
        .iter()
        .zip(&base)
        .map(|(r, b)| match r.parent().and_then(Path::file_name) {
            Some(p) => format!("{b} ({})", p.to_string_lossy()),
            None => b.clone(),
        })
        .collect();
    let mut repos: Vec<NamedRepo> = roots
        .iter()
        .enumerate()
        .map(|(i, root)| {
            let name = if count(&base, &base[i]) == 1 {
                base[i].clone()
            } else if count(&with_parent, &with_parent[i]) == 1 {
                with_parent[i].clone()
            } else {
                root.display().to_string()
            };
            NamedRepo {
                name,
                root: root.clone(),
            }
        })
        .collect();
    repos.sort_by_key(|r| r.name.to_lowercase());
    repos
}

fn repo_display_name(root: &Path) -> String {
    root.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.display().to_string())
}

/// `p` without Windows' `\\?\` verbatim prefix.
fn plain_path(p: &Path) -> PathBuf {
    PathBuf::from(travsr_store::registry::strip_verbatim_prefix(&p.to_string_lossy()).as_ref())
}

/// What two roots are compared by: plain, no trailing separator, and
/// case-folded where the file system is case-insensitive.
fn root_key(p: &Path) -> String {
    let s = plain_path(p)
        .to_string_lossy()
        .trim_end_matches(['/', '\\'])
        .to_string();
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
}

async fn repos(State(state): State<Arc<UiState>>) -> Json<ReposResponse> {
    let current_root = state.current.as_ref().map(|c| root_key(&c.root));
    let rows = known_repos(&state)
        .into_iter()
        .map(|r| RepoRow {
            indexed: r.db_path().is_file(),
            current: current_root.as_deref() == Some(root_key(&r.root).as_str()),
            root: r.root.display().to_string(),
            name: r.name,
        })
        .collect();
    Json(ReposResponse { repos: rows })
}

#[derive(Debug)]
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

/// Resolve the page's `repo` parameter to an indexed repo it may read.
fn resolve_repo(state: &UiState, name: Option<&str>) -> Result<NamedRepo, ApiError> {
    let name = name
        .filter(|n| !n.is_empty())
        .ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, "missing 'repo'".into()))?;
    let repo = known_repos(state)
        .into_iter()
        .find(|r| r.name == name)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("unknown repo '{name}'")))?;
    if !repo.db_path().is_file() {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            format!(
                "'{name}' is not indexed; run `travsr init` in {}",
                repo.root.display()
            ),
        ));
    }
    Ok(repo)
}

/// Where an answer came from, so the page can say when no daemon is running.
#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum Route {
    Daemon,
    Direct,
}

/// One daemon round trip at a time from this server. A page loads several
/// panels at once; the daemon answers its pipe serially, so sending them all
/// together only queues them inside the daemon, where an abandoned request
/// still costs it the work. Waiting here instead keeps the daemon's queue to
/// what a terminal user could produce.
static DAEMON_TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Run one query the way the CLI does: the repo's daemon when it answers,
/// otherwise a direct open of its store. Blocking (socket and SQLite I/O),
/// so it runs off the async workers.
///
/// Unlike the CLI's `open_read_store`, the direct open never falls back to a
/// writable one: `travsr ui` is read-only, and a writable open would run
/// migrations on the user's index and checkpoint it on drop. An index that
/// needs migrating is reported as such, with the command that upgrades it.
async fn run_query<T, F>(
    repo: NamedRepo,
    tool: &'static str,
    args: serde_json::Value,
    direct: F,
) -> Result<(T, Route), ApiError>
where
    T: serde::de::DeserializeOwned + Send + 'static,
    F: FnOnce(&travsr_store::SqliteStore) -> anyhow::Result<T> + Send + 'static,
{
    run_query_via(repo, tool, args, direct, true).await
}

async fn run_query_via<T, F>(
    repo: NamedRepo,
    tool: &'static str,
    args: serde_json::Value,
    direct: F,
    use_daemon: bool,
) -> Result<(T, Route), ApiError>
where
    T: serde::de::DeserializeOwned + Send + 'static,
    F: FnOnce(&travsr_store::SqliteStore) -> anyhow::Result<T> + Send + 'static,
{
    let work = tokio::task::spawn_blocking(move || {
        // No turn within TURN_WAIT means another query is still talking to the
        // daemon (possibly one whose page already gave up on it at the
        // deadline): answer from the index, as the CLI does when no daemon
        // answers, instead of queueing behind it.
        if let Some(_turn) = use_daemon.then(take_daemon_turn).flatten() {
            if let Some(v) = daemon_client::try_query::<T>(&repo.root, tool, args) {
                return Ok((v, Route::Daemon));
            }
        }
        let store = open_index_read_only(&repo)?;
        Ok((direct(&store)?, Route::Direct))
    });
    // A page must not wait forever on one answer. The blocking work cannot be
    // cancelled and finishes in the background; the page is told now, and
    // says that this part did not answer rather than spinning.
    match tokio::time::timeout(QUERY_DEADLINE, work).await {
        Err(_) => Err(ApiError(
            StatusCode::GATEWAY_TIMEOUT,
            format!("no answer within {} s", QUERY_DEADLINE.as_secs()),
        )),
        Ok(joined) => joined
            .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .map_err(|e: anyhow::Error| {
                ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
            }),
    }
}

/// The repo's index, read-only, or an error that says how to upgrade it.
fn open_index_read_only(repo: &NamedRepo) -> anyhow::Result<travsr_store::SqliteStore> {
    travsr_store::SqliteStore::open_read_only(&repo.db_path()).map_err(|e| {
        anyhow::anyhow!(
            "cannot read this index without changing it ({e}); travsr ui never writes the \
             index, so run `travsr init` in {} to upgrade it, or start its daemon",
            repo.root.display()
        )
    })
}

/// Longest a query waits for the daemon turn before answering from the index.
const TURN_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// The daemon turn, or `None` if it stayed taken for [`TURN_WAIT`]. A poisoned
/// lock only means an earlier query panicked; the turn guards no data that
/// panic could have left half-written, so it is taken anyway.
fn take_daemon_turn() -> Option<std::sync::MutexGuard<'static, ()>> {
    let until = std::time::Instant::now() + TURN_WAIT;
    loop {
        match DAEMON_TURN.try_lock() {
            Ok(g) => return Some(g),
            Err(std::sync::TryLockError::Poisoned(p)) => return Some(p.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) if std::time::Instant::now() < until => {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(std::sync::TryLockError::WouldBlock) => return None,
        }
    }
}

/// Longest one query may hold a page. Graph reads answer in milliseconds; this
/// bounds the diagnostics that shell out to `git` on a slow or busy machine.
const QUERY_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Serialize)]
struct StatusResponse {
    repo: String,
    root: String,
    route: Route,
    status: StatusPayload,
    /// `graph.db` size, the extension's "DB size" tile.
    db_bytes: Option<u64>,
    /// When a file was last reindexed (newest `files.last_indexed_at`), the
    /// "Last indexed" tile. Not `graph.db`'s modification time, which the
    /// extension uses: any WAL checkpoint moves that, so it read "just now"
    /// after every query rather than after every reindex.
    last_indexed_ms: Option<u64>,
    /// Whether `.git/hooks/post-commit` runs travsr, as the extension checks
    /// it. `None` when the hook could not be read.
    commit_hook: Option<bool>,
}

async fn status(
    State(state): State<Arc<UiState>>,
    Query(params): Query<BTreeMap<String, String>>,
) -> Result<Json<StatusResponse>, ApiError> {
    let repo = resolve_repo(&state, params.get("repo").map(String::as_str))?;
    let (name, root) = (repo.name.clone(), repo.root.display().to_string());
    let db_bytes = std::fs::metadata(repo.db_path()).ok().map(|m| m.len());
    let last_indexed_ms = {
        let db = repo.db_path();
        tokio::task::spawn_blocking(move || files::last_indexed_ms(&db))
            .await
            .ok()
            .flatten()
    };
    let commit_hook = commit_hook_installed(&repo.root);
    let (status, route) =
        run_query(repo, "status", serde_json::json!({}), query::status_query).await?;
    Ok(Json(StatusResponse {
        repo: name,
        root,
        route,
        status,
        db_bytes,
        last_indexed_ms,
        commit_hook,
    }))
}

/// Same test the extension's Health page applies: the post-commit hook exists
/// and mentions travsr. A missing file is a definite "no"; an unreadable one is
/// "could not tell".
fn commit_hook_installed(root: &Path) -> Option<bool> {
    let hook = root.join(".git").join("hooks").join("post-commit");
    match std::fs::read_to_string(&hook) {
        Ok(text) => Some(text.contains("travsr")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(false),
        Err(_) => None,
    }
}

#[derive(Serialize)]
struct ToolResponse {
    tool: String,
    route: Route,
    text: String,
}

/// `GET /api/tool/<name>?repo=<repo>&<arg>=<value>...`: one read-only MCP tool.
/// Query parameters other than `repo` are the tool's arguments, as strings;
/// the MCP argument check converts integers and booleans exactly as it does
/// for the VS Code extension, which also sends strings.
async fn tool(
    State(state): State<Arc<UiState>>,
    UrlPath(tool): UrlPath<String>,
    Query(mut params): Query<BTreeMap<String, String>>,
) -> Result<Json<ToolResponse>, ApiError> {
    if !travsr_mcp::BROWSE_TOOLS.contains(&tool.as_str()) {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("unknown tool '{tool}'"),
        ));
    }
    // `repos_list` reads only the global registry; it takes a store because
    // every tool does, and never touches it. Answered without resolving a
    // repo, so the Repos page lists the registry even when no repo here is
    // indexed (resolving one would 404 exactly when the listing is most
    // useful).
    if tool == "repos_list" {
        let payload = tokio::task::spawn_blocking(|| {
            let store = travsr_store::SqliteStore::open_in_memory()?;
            anyhow::Ok(query::tool_query(
                &store,
                &ToolQueryArgs {
                    tool: "repos_list".into(),
                    args: serde_json::json!({}),
                },
            ))
        })
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
        return Ok(Json(ToolResponse {
            tool,
            route: Route::Direct,
            text: payload.text,
        }));
    }
    let repo = resolve_repo(&state, params.remove("repo").as_deref())?;
    let args = ToolQueryArgs {
        tool: tool.clone(),
        args: serde_json::Value::Object(
            params
                .into_iter()
                .map(|(k, v)| (k, serde_json::Value::String(v)))
                .collect(),
        ),
    };
    let wire = serde_json::to_value(&args)
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    // The Health page's index diagnostics are answered from the index itself,
    // as the extension's own `travsr mcp --stdio` child answers them; the
    // daemon refuses them (travsr_mcp::DIRECT_ONLY_TOOLS says why).
    let use_daemon = !travsr_mcp::DIRECT_ONLY_TOOLS.contains(&tool.as_str());
    let (payload, route): (ToolPayload, Route) = run_query_via(
        repo,
        "tool",
        wire,
        move |store| Ok(query::tool_query(store, &args)),
        use_daemon,
    )
    .await?;
    if let Some(e) = payload.error {
        return Err(ApiError(StatusCode::BAD_REQUEST, e));
    }
    Ok(Json(ToolResponse {
        tool,
        route,
        text: payload.text,
    }))
}

/// Best effort: a failure only means the user opens the printed URL.
fn open_browser(url: &str) {
    #[cfg(windows)]
    let result = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn();
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(url).spawn();
    #[cfg(not(any(windows, target_os = "macos")))]
    let result = std::process::Command::new("xdg-open").arg(url).spawn();
    if let Err(e) = result {
        tracing::debug!(error = %e, "could not open a browser");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt as _;

    fn app() -> Router {
        router(Arc::new(UiState {
            current: None,
            check_host: true,
        }))
    }

    /// The review's point 2: the direct fallback must never write. A writable
    /// open would migrate; even dropping a store runs
    /// `wal_checkpoint(TRUNCATE)`, which must be a no-op on a read-only one.
    #[test]
    fn the_direct_open_never_writes_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join(".travsr")).unwrap();
        let db = root.join(".travsr").join("graph.db");
        drop(travsr_store::SqliteStore::open(&db).unwrap()); // create + migrate

        // A writer holding uncheckpointed pages in the WAL, as a running
        // daemon would.
        let w = rusqlite::Connection::open(&db).unwrap();
        w.execute_batch(
            "PRAGMA wal_autocheckpoint=0;
             INSERT OR REPLACE INTO meta (key, value) VALUES ('ui_probe', 'x');",
        )
        .unwrap();
        let wal = root.join(".travsr").join("graph.db-wal");
        let before = std::fs::metadata(&wal).unwrap().len();
        assert!(before > 0, "the writer left pages in the WAL");

        let repo = NamedRepo {
            name: "r".into(),
            root,
        };
        drop(open_index_read_only(&repo).unwrap());
        assert_eq!(
            std::fs::metadata(&wal).unwrap().len(),
            before,
            "the read-only open checkpointed the WAL"
        );
    }

    #[tokio::test]
    async fn repos_list_needs_no_indexed_repo() {
        // No current repo and no repo parameter: every other tool 400s here,
        // the registry listing still answers.
        let resp = get_with_host("/api/tool/repos_list", "localhost").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = get_with_host("/api/tool/get_graph_stats", "localhost").await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn host_accepts_localhost_and_ip_literals_only() {
        assert_eq!(
            parse_host("localhost").unwrap(),
            IpAddr::from([127, 0, 0, 1])
        );
        assert_eq!(
            parse_host("LocalHost").unwrap(),
            IpAddr::from([127, 0, 0, 1])
        );
        assert_eq!(
            parse_host("127.0.0.1").unwrap(),
            IpAddr::from([127, 0, 0, 1])
        );
        assert!(parse_host("[::1]").unwrap().is_loopback());
        assert!(!parse_host("0.0.0.0").unwrap().is_loopback());
        assert!(parse_host("example.com").is_err());
    }

    fn names(roots: &[&str]) -> Vec<String> {
        name_repos(roots.iter().map(PathBuf::from).collect())
            .into_iter()
            .map(|r| r.name)
            .collect()
    }

    #[test]
    fn repos_are_named_by_folder_like_travsr_repos() {
        // The review's case: registry keys are full roots, but the page, the
        // URL and `repos_list` all speak folder names.
        assert_eq!(
            names(&["/Users/ak/Desktop/Proj/travsr", "/Users/ak/code/api"]),
            vec!["api", "travsr"]
        );
    }

    #[test]
    fn a_shared_folder_name_gets_its_parent_then_its_path() {
        assert_eq!(
            names(&["/a/work/travsr", "/b/home/travsr"]),
            vec!["travsr (home)", "travsr (work)"]
        );
        // Same folder and same parent name: only the full path is unique.
        let full = names(&["/a/work/travsr", "/b/work/travsr"]);
        assert_eq!(full.len(), 2);
        assert_ne!(full[0], full[1]);
        assert!(full
            .iter()
            .all(|n| n.ends_with("travsr") && n.contains('/')));
    }

    #[test]
    #[cfg(windows)]
    fn a_verbatim_and_a_plain_root_are_one_repo() {
        // The registry stores canonical `\\?\C:\...`; `travsr ui` started in
        // the same checkout knows it as `C:\...`.
        let repos = name_repos(vec![
            PathBuf::from(r"\\?\C:\Code\Travsr"),
            PathBuf::from(r"c:\code\travsr\"),
        ]);
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].name, "Travsr");
        assert!(!repos[0].root.to_string_lossy().starts_with(r"\\?\"));
    }

    async fn get_with_host(uri: &str, host: &str) -> Response {
        app()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header(header::HOST, host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    #[test]
    fn loopback_host_headers_are_accepted() {
        for h in [
            "localhost",
            "localhost:7330",
            "LOCALHOST:1",
            "app.localhost:7330",
            "127.0.0.1:7330",
            "127.0.0.53",
            "[::1]:7330",
            "[::1]",
        ] {
            assert!(is_loopback_host_header(h), "{h} should be accepted");
        }
    }

    #[test]
    fn rebinding_host_headers_are_rejected() {
        // DNS rebinding: an attacker's name resolving to 127.0.0.1 still sends
        // its own name in Host.
        for h in [
            "evil.example:7330",
            "localhost.evil.example",
            "10.0.0.5:7330",
            "0.0.0.0:7330",
            "[::1",
            "",
        ] {
            assert!(!is_loopback_host_header(h), "{h} should be rejected");
        }
    }

    #[tokio::test]
    async fn page_is_served_with_a_strict_csp() {
        let resp = get_with_host("/", "localhost:7330").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let csp = resp.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap();
        assert!(csp.contains("default-src 'none'"));
        assert!(csp.contains("frame-ancestors 'none'"));
        assert_eq!(resp.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    }

    #[tokio::test]
    async fn foreign_host_is_refused_before_routing() {
        let resp = get_with_host("/api/repos", "evil.example:7330").await;
        assert_eq!(resp.status(), StatusCode::MISDIRECTED_REQUEST);
    }

    #[tokio::test]
    async fn only_browse_tools_are_reachable() {
        // A writing tool must not be callable through the page, even by name.
        let resp = get_with_host("/api/tool/synonym_add?repo=x", "localhost").await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn an_unknown_repo_is_not_resolved_to_a_path() {
        let resp = get_with_host(
            "/api/tool/search_symbol?repo=..%2F..%2Fetc&name=x",
            "localhost",
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn non_loopback_bind_needs_allow_remote() {
        let err = run("0.0.0.0".into(), 0, false, false).await.unwrap_err();
        assert!(err.to_string().contains("--allow-remote"), "{err}");
    }

    #[tokio::test]
    async fn port_in_use_is_reported_plainly() {
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = held.local_addr().unwrap().port();
        let err = run("127.0.0.1".into(), port, false, false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already in use"), "{err}");
        assert!(err.to_string().contains("--port"), "{err}");
    }

    #[test]
    fn printed_url_uses_localhost_for_loopback() {
        let v4: SocketAddr = "127.0.0.1:7330".parse().unwrap();
        assert_eq!(page_url(v4), "http://localhost:7330/");
        let v6: SocketAddr = "[::1]:7330".parse().unwrap();
        assert_eq!(page_url(v6), "http://[::1]:7330/");
    }
}
