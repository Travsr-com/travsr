//! travsr-mcp — Model Context Protocol server.
//!
//! Exposes Travsr's graph retrieval tools to MCP clients (Claude Desktop,
//! VS Code, etc.) over stdio. Per CLAUDE.md principle 4, MCP is the *only*
//! external interface — no REST, no GraphQL.
//!
//! Framing: newline-delimited JSON-RPC 2.0 over stdin/stdout.
//! All diagnostics are written to stderr via `tracing`.

#![forbid(unsafe_code)]

pub mod auth;
mod protocol;
pub mod query;
mod sanitize;
mod seed;
mod server;
pub mod session;
pub mod sse;
mod tools;

// Re-exported for fuzz targets (fuzz/fuzz_targets/fuzz_mcp_parser.rs).
pub use auth::fetch_signing_keys;
pub use protocol::RpcRequest;
pub use session::{session_id_log_hash, Session, SessionId, SessionStore};
pub use sse::{router as sse_router, AppState};

use std::path::Path;

use anyhow::Context as _;
use travsr_store::SqliteStore;

pub(crate) const PROTOCOL_VERSION: &str = "2024-11-05";
pub(crate) const SERVER_NAME: &str = "travsr";
pub(crate) const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Start the MCP stdio server backed by the graph database at `db_path`.
///
/// Reads JSON-RPC 2.0 requests from stdin, dispatches them to the graph
/// tools, and writes responses to stdout. Runs until stdin is closed.
pub fn serve_stdio(db_path: &Path) -> anyhow::Result<()> {
    // `&mut`: the synonym_* tools perform writes (some via SQLite transactions,
    // which require `&mut Connection`). Read-only tools auto-reborrow `&mut`→`&`.
    let mut store = SqliteStore::open(db_path)
        .with_context(|| format!("opening graph database at {}", db_path.display()))?;
    // Inject the embed KNN hook so get_context's semantic seed selection (Step 4)
    // works in standalone `travsr mcp --stdio` mode, not just daemon mode.
    inject_embed_hook(&mut store, db_path);
    server::run(&mut store)
}

/// Start the global MCP stdio server backed by all repos in `~/.travsr/registry.json`.
///
/// The registry is re-read on every tool call so repos added via `travsr init`
/// after startup are picked up live without restarting the server.
pub fn serve_stdio_global() -> anyhow::Result<()> {
    server::run_global()
}

/// Wire the active embed backend's KNN hook into `store`.
///
/// The sidecar loads a 200–1400 MB ONNX model at startup — this takes 15–25 s
/// on a cold start. To keep `serve_stdio` (and the extension's `initialize`
/// handshake) responsive, sidecar startup is moved to a background thread.
///
/// A lazy meta-hook is injected immediately: it returns `Ok(vec![])` while the
/// sidecar is still loading, and delegates to the real KNN hook once it is ready.
/// This produces "embedding in progress" signals in `get_context` responses rather
/// than blank results or a 15–25 s connect stall.
fn inject_embed_hook(store: &mut SqliteStore, db_path: &Path) {
    use std::sync::{Arc, Mutex};

    use travsr_error::StoreError;
    use travsr_plugin_host::{
        active_backend_id, lookup_embed_backend, EmbedSupervisor, EMBED_BACKENDS,
    };
    use travsr_store::{EmbedKnnHook, EmbedReadiness};

    // Guard: no embed.db → nothing to query; skip to avoid spawning a sidecar
    // against a non-existent HNSW index.
    if !db_path.with_file_name("embed.db").exists() {
        return;
    }

    let Some(home) = dirs::home_dir() else { return };
    let backend = active_backend_id()
        .as_deref()
        .and_then(lookup_embed_backend)
        .or_else(|| EMBED_BACKENDS.first())
        .copied();
    let Some(backend) = backend else { return };

    let binary = home.join(".travsr").join("bin").join(backend.binary_name);
    // Fast path: if the binary isn't installed there's nothing to do.
    if !binary.exists() {
        return;
    }

    let db_path_bg = db_path.to_path_buf();
    let model_id_bg = backend.id.to_string();

    // Shared hook slot: None = sidecar starting; Some = ready to call.
    // `knn_hook()` clones the inner Arc<Mutex<EmbedSidecar>> so the sidecar
    // process stays alive after `EmbedSupervisor` itself is dropped on the thread.
    let slot: Arc<Mutex<Option<EmbedKnnHook>>> = Arc::new(Mutex::new(None));
    let slot_bg = Arc::clone(&slot);

    // Arm-state shared with the query path: the background thread marks it ready
    // the instant the sidecar is warm, so the first get_context can briefly wait
    // (and the header reports `warming` honestly) instead of silently degrading
    // to lexical-only during the ~3 s startup window.
    let readiness = EmbedReadiness::new();
    let readiness_bg = Arc::clone(&readiness);

    std::thread::Builder::new()
        .name("embed-hook-init".into())
        .spawn(move || {
            let supervisor = EmbedSupervisor::try_start(&binary, &db_path_bg, &model_id_bg);
            if supervisor.is_active() {
                if let Some(mid) = supervisor.model_id().map(str::to_string) {
                    if let Some(hook) = supervisor.knn_hook(mid.clone()) {
                        // Warm the sidecar (ONNX + HNSW load) BEFORE arming the
                        // hook, so the first real query never pays the cold-start
                        // cost that would trip the host's 600 ms KNN breaker and
                        // silently degrade to FTS. Blocking — we are already on a
                        // background init thread, so this delays nothing visible.
                        supervisor.prewarm();
                        if let Ok(mut guard) = slot_bg.lock() {
                            *guard = Some(hook);
                        }
                        // Signal arm-complete AFTER the slot is populated so any
                        // thread woken by `mark_ready` sees `Some(hook)`.
                        readiness_bg.mark_ready();
                        tracing::info!(
                            model_id = %mid,
                            "embed plugin active — Step 4 (semantic ANN) enabled"
                        );
                    }
                }
            }
            // supervisor drops here; sidecar stays alive via the hook's Arc.
        })
        .ok();

    // Inject the meta-hook immediately so `serve_stdio` can respond to
    // `initialize` without waiting for the sidecar to finish loading its model.
    let meta: EmbedKnnHook = Arc::new(move |query: &str, k: u32| {
        let guard = slot
            .lock()
            .map_err(|_| StoreError::Database("embed hook slot poisoned".into()))?;
        match guard.as_ref() {
            None => Ok(vec![]), // sidecar still warming up — return no seeds
            Some(hook) => hook(query, k),
        }
    });
    store.set_embed_readiness(readiness);
    store.set_embed_knn_hook(meta);
    tracing::info!("embed plugin hook installed (lazy — sidecar starting in background)");
}
