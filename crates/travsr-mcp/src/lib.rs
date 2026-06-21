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
/// Silently no-ops when no backend is installed or the sidecar fails to start.
fn inject_embed_hook(store: &mut SqliteStore, db_path: &Path) {
    use travsr_plugin_host::{
        active_backend_id, lookup_embed_backend, EmbedSupervisor, EMBED_BACKENDS,
    };

    let Some(home) = dirs::home_dir() else {
        return;
    };
    let backend = active_backend_id()
        .as_deref()
        .and_then(lookup_embed_backend)
        .or_else(|| EMBED_BACKENDS.first())
        .copied();
    let Some(backend) = backend else { return };

    let binary = home.join(".travsr").join("bin").join(backend.binary_name);
    let supervisor = EmbedSupervisor::try_start(&binary, db_path);
    if !supervisor.is_active() {
        return;
    }
    let Some(model_id) = supervisor.model_id().map(str::to_string) else {
        return;
    };
    if let Some(hook) = supervisor.knn_hook(model_id.clone()) {
        store.set_embed_knn_hook(hook);
        tracing::info!(model_id = %model_id, "embed plugin active — Step 4 (semantic ANN) enabled");
    }
}
