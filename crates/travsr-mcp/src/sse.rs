//! Axum SSE transport for the cloud/team MCP server (RFC-007).
//!
//! Routes:
//!   GET  /sse      — open a persistent SSE stream (scope: mcp)
//!   POST /rpc      — submit a JSON-RPC 2.0 request (scope: mcp)
//!   GET  /health   — liveness probe (unauthenticated)
//!   GET  /metrics  — Prometheus text format (scope: metrics)

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::DefaultBodyLimit;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::Event;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use dashmap::DashMap;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tower_http::trace::TraceLayer;
use uuid::Uuid;

use crate::auth::{auth_error_status, verify_token, AuthError, TokenScope, VerifiedToken};
use crate::tools;

// ── Types ─────────────────────────────────────────────────────────────────────

pub type TenantId = String;

/// Shared application state — all fields must be `Send + Sync`.
pub struct AppState {
    /// Active SSE sessions keyed by session UUID.
    pub sessions: DashMap<Uuid, SseSessionHandle>,
    /// Tenant repo registries: tenant_id → repo_name → db_path.
    pub tenant_repos: DashMap<TenantId, HashMap<String, PathBuf>>,
    /// Signing keys for bearer token verification.
    pub signing_keys: Vec<[u8; 32]>,
    /// Ring buffer for SSE event replay (at-most-once delivery on reconnect).
    /// Key: (tenant_id, session_id). Value: VecDeque of (event_id, received_at, json_payload).
    pub ring_buffers: RingBufferMap,
    /// Per-session monotonic event counter.
    pub event_counters: DashMap<Uuid, u64>,
    /// In-flight RPC requests: (tenant_id, request_id) → ().
    pub in_flight: DashMap<(TenantId, String), ()>,
}

impl AppState {
    pub fn new(
        tenant_repos: DashMap<TenantId, HashMap<String, PathBuf>>,
        signing_keys: Vec<[u8; 32]>,
    ) -> Self {
        Self {
            sessions: DashMap::new(),
            tenant_repos,
            signing_keys,
            ring_buffers: DashMap::new(),
            event_counters: DashMap::new(),
            in_flight: DashMap::new(),
        }
    }
}

/// Handle to a live SSE session stored in AppState.
pub struct SseSessionHandle {
    pub tenant_id: TenantId,
    pub tx: mpsc::Sender<SseEvent>,
    pub opened_at: Instant,
}

/// Events that can be sent on an SSE stream.
pub enum SseEvent {
    JsonRpc(String),
    Keepalive,
    Close,
}

// Ring buffer limits.
const RING_BUFFER_CAPACITY: usize = 1000;
const RING_BUFFER_TTL: std::time::Duration = std::time::Duration::from_secs(300);

/// A single ring-buffer entry: (event_id, received_at, json_payload).
type RingEntry = (u64, Instant, String);
/// The ring buffer map type.
type RingBufferMap = DashMap<(TenantId, Uuid), VecDeque<RingEntry>>;

// ── Router ────────────────────────────────────────────────────────────────────

/// Build the axum router. Path-only spans are included; Authorization headers are
/// never captured in trace spans.
pub fn router(state: Arc<AppState>) -> Router {
    use axum::routing::{get, post};

    // RFC-021: background-warm the reranker (idempotent, non-blocking) so the
    // first real request doesn't pay the model-load cost.
    crate::rerank::warm_background();

    Router::new()
        .route("/sse", get(sse_handler))
        .route("/rpc", post(rpc_handler))
        .route("/health", get(health_handler))
        .route("/metrics", get(metrics_handler))
        // Reject bodies larger than MAX_BODY_BYTES at middleware level before axum
        // buffers them — prevents multi-MB heap allocations ahead of the in-handler check.
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(
            TraceLayer::new_for_http().make_span_with(|request: &axum::http::Request<_>| {
                tracing::info_span!(
                    "http_request",
                    method = %request.method(),
                    path = %request.uri().path(),
                )
            }),
        )
        .with_state(state)
}

// ── Auth helper ───────────────────────────────────────────────────────────────

/// Extract and verify the bearer token from request headers.
fn extract_bearer(
    headers: &HeaderMap,
    signing_keys: &[[u8; 32]],
) -> Result<VerifiedToken, AuthError> {
    let auth_header = headers
        .get(axum::http::header::AUTHORIZATION)
        .ok_or(AuthError::MissingHeader)?;

    let auth_str = auth_header
        .to_str()
        .map_err(|_| AuthError::BadHeaderFormat)?;

    let token = auth_str
        .strip_prefix("Bearer ")
        .ok_or(AuthError::BadHeaderFormat)?;

    verify_token(token, signing_keys)
}

/// Return a JSON-RPC 2.0 error response string.
fn jsonrpc_error(id: &str, code: i32, message: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
    .to_string()
}

/// Return a JSON-RPC 2.0 success response string.
fn jsonrpc_ok(id: &str, result: serde_json::Value) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    })
    .to_string()
}

// ── Ring buffer helpers ───────────────────────────────────────────────────────

fn ring_buffer_push(
    state: &AppState,
    tenant_id: &str,
    session_id: Uuid,
    event_id: u64,
    payload: String,
) {
    let key = (tenant_id.to_string(), session_id);
    let now = Instant::now();
    let mut buf = state.ring_buffers.entry(key).or_default();
    // Evict expired entries.
    while let Some(front) = buf.front() {
        if now.duration_since(front.1) > RING_BUFFER_TTL {
            buf.pop_front();
        } else {
            break;
        }
    }
    if buf.len() >= RING_BUFFER_CAPACITY {
        buf.pop_front();
    }
    buf.push_back((event_id, now, payload));
}

fn ring_buffer_replay(
    state: &AppState,
    tenant_id: &str,
    session_id: Uuid,
    after_id: u64,
) -> Vec<(u64, String)> {
    let key = (tenant_id.to_string(), session_id);
    let now = Instant::now();
    match state.ring_buffers.get(&key) {
        None => vec![],
        Some(buf) => buf
            .iter()
            .filter(|(id, ts, _)| *id > after_id && now.duration_since(*ts) <= RING_BUFFER_TTL)
            .map(|(id, _, payload)| (*id, payload.clone()))
            .collect(),
    }
}

fn next_event_id(state: &AppState, session_id: Uuid) -> u64 {
    let mut counter = state.event_counters.entry(session_id).or_insert(0);
    *counter += 1;
    *counter
}

// ── GET /health ───────────────────────────────────────────────────────────────

async fn health_handler() -> impl IntoResponse {
    Json(json!({"status": "ok"}))
}

// ── GET /metrics ──────────────────────────────────────────────────────────────

async fn metrics_handler(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let vt = match extract_bearer(&headers, &state.signing_keys) {
        Ok(vt) => vt,
        Err(e) => {
            let status = auth_error_status(&e);
            return (status, e.to_string()).into_response();
        }
    };
    if vt.scope != TokenScope::Metrics {
        return (
            StatusCode::FORBIDDEN,
            "insufficient scope: metrics required",
        )
            .into_response();
    }

    let active_sessions = state.sessions.len();

    // Sum graph.db sizes across /data/tenants — or 0 if directory not accessible.
    let db_size_bytes: u64 = {
        let base = std::path::Path::new("/data/tenants");
        if base.is_dir() {
            let mut total = 0u64;
            for entry in state.tenant_repos.iter() {
                for db_path in entry.value().values() {
                    if let Ok(meta) = std::fs::metadata(db_path) {
                        total += meta.len();
                    }
                }
            }
            total
        } else {
            0
        }
    };

    let body = format!(
        "# HELP travsr_sse_active_sessions_total Number of active SSE sessions\n\
         # TYPE travsr_sse_active_sessions_total gauge\n\
         travsr_sse_active_sessions_total {active_sessions}\n\
         # HELP travsr_tenant_db_size_bytes Total size of all tenant graph databases in bytes\n\
         # TYPE travsr_tenant_db_size_bytes gauge\n\
         travsr_tenant_db_size_bytes {db_size_bytes}\n"
    );

    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        body,
    )
        .into_response()
}

// ── GET /sse ──────────────────────────────────────────────────────────────────

async fn sse_handler(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    // Authenticate — scope must be Mcp.
    let vt = match extract_bearer(&headers, &state.signing_keys) {
        Ok(vt) => vt,
        Err(e) => {
            let status = auth_error_status(&e);
            return (status, e.to_string()).into_response();
        }
    };
    if vt.scope != TokenScope::Mcp {
        return (StatusCode::FORBIDDEN, "insufficient scope: mcp required").into_response();
    }

    let tenant_id = vt.tenant_id.clone();
    let session_id = Uuid::new_v4();

    // Check for reconnect headers.
    let last_event_id: Option<u64> = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok());
    let reconnect_session_id: Option<Uuid> = headers
        .get("x-session-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| Uuid::parse_str(s).ok());

    // Create mpsc channel for this session.
    let (tx, rx) = mpsc::channel::<SseEvent>(64);

    let handle = SseSessionHandle {
        tenant_id: tenant_id.clone(),
        tx: tx.clone(),
        opened_at: Instant::now(),
    };
    state.sessions.insert(session_id, handle);

    // Keepalive task — cancels the token when the client disconnects (send error).
    let cancel = CancellationToken::new();
    let cancel_for_ka = cancel.clone();
    let tx_ka = tx.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel_for_ka.cancelled() => break,
                _ = tokio::time::sleep(tokio::time::Duration::from_secs(15)) => {
                    if tx_ka.send(SseEvent::Keepalive).await.is_err() {
                        // Receiver dropped — client disconnected. Signal cleanup.
                        cancel_for_ka.cancel();
                        break;
                    }
                }
            }
        }
    });

    // Build replay events if reconnecting.
    let replay_events: Vec<(u64, String)> =
        if let (Some(after_id), Some(old_session_id)) = (last_event_id, reconnect_session_id) {
            ring_buffer_replay(&state, &tenant_id, old_session_id, after_id)
        } else {
            vec![]
        };

    // Convert the mpsc receiver into an SSE stream.
    let state_for_stream = state.clone();
    let tenant_id_for_stream = tenant_id.clone();

    let initial_event = {
        let session_json = json!({"session_id": session_id.to_string()}).to_string();
        // session event: id 0, event: session
        Ok::<Event, std::convert::Infallible>(
            Event::default().event("session").id("0").data(session_json),
        )
    };

    // Build the replay stream.
    let replay_stream = tokio_stream::iter(replay_events.into_iter().map(|(id, payload)| {
        Ok::<Event, std::convert::Infallible>(Event::default().id(id.to_string()).data(payload))
    }));

    // Convert the live mpsc channel to a stream.
    let live_stream = {
        let state_s = state_for_stream.clone();
        let tenant_id_s = tenant_id_for_stream.clone();
        tokio_stream::StreamExt::map(ReceiverStream::new(rx), move |ev| match ev {
            SseEvent::JsonRpc(payload) => {
                let eid = next_event_id(&state_s, session_id);
                ring_buffer_push(&state_s, &tenant_id_s, session_id, eid, payload.clone());
                Ok::<Event, std::convert::Infallible>(
                    Event::default().id(eid.to_string()).data(payload),
                )
            }
            SseEvent::Keepalive => Ok(Event::default().comment("keepalive")),
            SseEvent::Close => Ok(Event::default().comment("close")),
        })
    };

    // Chain: initial session event → replayed events → live events.
    use tokio_stream::StreamExt as _;
    let full_stream = tokio_stream::once(initial_event)
        .chain(replay_stream)
        .chain(live_stream);

    // Spawn a cleanup task: when the mpsc tx is dropped (stream consumed / client
    // disconnected), the keepalive task will get a send error and stop. We also
    // remove the session from AppState when the CancellationToken fires.
    //
    // The cleanup task waits until the cancellation token is cancelled (which
    // happens when the keepalive task exits because all tx clones are dropped).
    // Since the SSE stream holds the last tx clone inside `live_stream`, when axum
    // drops the stream, tx is dropped → keepalive errors → cancel fires → cleanup runs.
    let state_cleanup = state.clone();
    let tenant_id_cleanup = tenant_id.clone();
    tokio::spawn(async move {
        cancel.cancelled().await;
        state_cleanup.sessions.remove(&session_id);
        state_cleanup.event_counters.remove(&session_id);
        state_cleanup
            .ring_buffers
            .remove(&(tenant_id_cleanup.clone(), session_id));
        tracing::debug!(
            session = %session_id,
            tenant = %tenant_id_cleanup,
            "SSE session cleaned up"
        );
    });

    axum::response::sse::Sse::new(full_stream)
        .keep_alive(
            axum::response::sse::KeepAlive::new()
                .interval(tokio::time::Duration::from_secs(15))
                .text("keepalive"),
        )
        .into_response()
}

// ── POST /rpc ─────────────────────────────────────────────────────────────────

/// Maximum JSON body size accepted (512 KB).
const MAX_BODY_BYTES: usize = 512 * 1024;

/// Maximum JSON response size before chunking (512 KB).
const MAX_RESPONSE_BYTES: usize = 512 * 1024;

async fn rpc_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    // Authenticate — scope must be Mcp.
    let vt = match extract_bearer(&headers, &state.signing_keys) {
        Ok(vt) => vt,
        Err(e) => {
            let status = auth_error_status(&e);
            return (status, e.to_string()).into_response();
        }
    };
    if vt.scope != TokenScope::Mcp {
        return (StatusCode::FORBIDDEN, "insufficient scope: mcp required").into_response();
    }

    let tenant_id = vt.tenant_id.clone();

    // Body size check.
    if body.len() > MAX_BODY_BYTES {
        return (StatusCode::PAYLOAD_TOO_LARGE, "request body exceeds 512KB").into_response();
    }

    // Parse JSON-RPC 2.0.
    let req_value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("invalid JSON: {e}")).into_response();
        }
    };

    // Validate JSON-RPC fields.
    if req_value.get("jsonrpc").and_then(|v| v.as_str()) != Some("2.0") {
        return (StatusCode::BAD_REQUEST, "jsonrpc field must be \"2.0\"").into_response();
    }

    // id must be a UUID4 string.
    let id_str = match req_value.get("id").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            return (StatusCode::BAD_REQUEST, "id field must be a UUID4 string").into_response();
        }
    };
    if Uuid::parse_str(id_str).is_err() {
        return (StatusCode::BAD_REQUEST, "id field must be a valid UUID").into_response();
    }
    let rpc_id = id_str.to_string();

    // Check in-flight dedup.
    let in_flight_key = (tenant_id.clone(), rpc_id.clone());
    if state.in_flight.contains_key(&in_flight_key) {
        return (
            StatusCode::BAD_REQUEST,
            "duplicate request id: already in flight",
        )
            .into_response();
    }
    state.in_flight.insert(in_flight_key.clone(), ());

    // Find the most recently opened SSE session for this tenant.
    let session_entry = {
        state
            .sessions
            .iter()
            .filter(|e| e.value().tenant_id == tenant_id)
            .max_by_key(|e| e.value().opened_at)
            .map(|e| (*e.key(), e.value().tx.clone()))
    };

    let (_session_id, session_tx) = match session_entry {
        Some((id, tx)) => (id, tx),
        None => {
            state.in_flight.remove(&in_flight_key);
            let mut resp = (
                StatusCode::SERVICE_UNAVAILABLE,
                "no active SSE session for tenant",
            )
                .into_response();
            resp.headers_mut()
                .insert("retry-after", axum::http::HeaderValue::from_static("1"));
            return resp;
        }
    };

    // Get repos for this tenant.
    let repos: HashMap<String, PathBuf> = state
        .tenant_repos
        .get(&tenant_id)
        .map(|r| r.clone())
        .unwrap_or_default();

    // Dispatch tool call (same dispatch as server.rs global mode). Rerank and
    // other blocking work must not run on the async/tokio worker thread (RFC-021
    // §15 G5(a)); move the whole synchronous dispatch to a blocking pool.
    let response_payload = {
        let rpc_id_for_dispatch = rpc_id.clone();
        match tokio::task::spawn_blocking(move || {
            dispatch_tool_call(&req_value, &rpc_id_for_dispatch, &repos)
        })
        .await
        {
            Ok(payload) => payload,
            Err(join_err) => {
                // A panic that previously unwound through rpc_handler now surfaces
                // as JoinError; answer with a JSON-RPC internal error instead of
                // dropping the request silently.
                tracing::error!(%join_err, "tool dispatch panicked");
                serde_json::json!({
                    "jsonrpc": "2.0", "id": rpc_id,
                    "error": { "code": -32603, "message": "internal error" }
                })
                .to_string()
            }
        }
    };

    // Send response(s) via SSE.
    let response_str = response_payload;
    let bytes = response_str.as_bytes();

    if bytes.len() <= MAX_RESPONSE_BYTES {
        // Single event — the live_stream map will call ring_buffer_push when it maps
        // the SseEvent::JsonRpc variant from the mpsc channel.
        let _ = session_tx.send(SseEvent::JsonRpc(response_str)).await;
    } else {
        // Chunk the response.
        let chunks: Vec<&[u8]> = bytes.chunks(MAX_RESPONSE_BYTES).collect();
        let total = chunks.len();
        for (i, chunk) in chunks.iter().enumerate() {
            let chunk_str = String::from_utf8_lossy(chunk).to_string();
            let envelope = json!({
                "jsonrpc": "2.0",
                "id": rpc_id,
                "result": {
                    "partial": true,
                    "part": i + 1,
                    "total": total,
                    "data": chunk_str
                }
            })
            .to_string();
            let _ = session_tx.send(SseEvent::JsonRpc(envelope)).await;
        }
    }

    state.in_flight.remove(&in_flight_key);
    StatusCode::OK.into_response()
}

/// Dispatch a JSON-RPC 2.0 tool call and return the response string.
///
/// Mirrors the dispatch logic from server.rs global mode without touching that file.
fn dispatch_tool_call(
    req: &serde_json::Value,
    rpc_id: &str,
    repos: &HashMap<String, PathBuf>,
) -> String {
    use crate::protocol::{INVALID_PARAMS, METHOD_NOT_FOUND};
    use travsr_retrieval::OpenFilter;

    let method = req.get("method").and_then(|v| v.as_str()).unwrap_or("");
    let params = req
        .get("params")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    match method {
        "initialize" => jsonrpc_ok(
            rpc_id,
            json!({
                "protocolVersion": crate::PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": crate::SERVER_NAME, "version": crate::SERVER_VERSION }
            }),
        ),
        "tools/call" => {
            let tool_name = match params["name"].as_str() {
                Some(n) => n,
                None => {
                    return jsonrpc_error(rpc_id, INVALID_PARAMS, "missing tool name");
                }
            };
            let args = &params["arguments"];
            let repo_arg = args["repo"].as_str();

            let _span = tracing::info_span!("mcp.sse.tool_call", tool = tool_name, tenant = "sse")
                .entered();

            let text = match tool_name {
                "get_dependencies" => tools::get_dependencies_global(
                    repos,
                    args["file"].as_str().unwrap_or(""),
                    repo_arg,
                ),
                "get_callers" => tools::get_callers_global(
                    repos,
                    args["symbol"].as_str().unwrap_or(""),
                    repo_arg,
                ),
                "get_blast_radius" => {
                    let mode = match args["analysis"].as_str().unwrap_or("tree-sitter") {
                        "semantic" => tools::AnalysisMode::Semantic,
                        _ => tools::AnalysisMode::TreeSitter,
                    };
                    tools::get_blast_radius_global(
                        repos,
                        args["file"].as_str().unwrap_or(""),
                        repo_arg,
                        mode,
                    )
                }
                "get_lang_status" => tools::get_lang_status_global(
                    repos,
                    args["file"].as_str().unwrap_or(""),
                    repo_arg,
                ),
                "search_symbol" => tools::search_symbol_global(
                    repos,
                    args["name"].as_str().unwrap_or(""),
                    repo_arg,
                ),
                "get_repo_map" => tools::get_repo_map_global(repos, repo_arg),
                // TODO(RFC-008 / #197): replace OpenFilter with per-session RbacFilter
                // once token-scoped RBAC is implemented for the SSE path.
                "get_execution_path" => tools::get_execution_path_global(
                    repos,
                    args["source"].as_str().unwrap_or(""),
                    args["sink"].as_str().unwrap_or(""),
                    repo_arg,
                    &OpenFilter,
                ),
                other => {
                    return jsonrpc_error(
                        rpc_id,
                        INVALID_PARAMS,
                        &format!("unknown tool: {other}"),
                    );
                }
            };

            tracing::info!(
                tool = tool_name,
                tool_calls_total = 1u64,
                "mcp.sse.tool_call complete"
            );

            jsonrpc_ok(
                rpc_id,
                json!({ "content": [{ "type": "text", "text": text }] }),
            )
        }
        other => jsonrpc_error(
            rpc_id,
            METHOD_NOT_FOUND,
            &format!("method not found: {other}"),
        ),
    }
}
