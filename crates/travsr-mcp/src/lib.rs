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
mod observability;
mod protocol;
pub mod query;
mod rerank;
mod sanitize;
mod seed;
mod server;
pub use server::{tools_list as stdio_tools_list, tools_list_global as global_tools_list};
pub mod session;
pub mod sse;
mod tools;

// Re-exported for fuzz targets (fuzz/fuzz_targets/fuzz_mcp_parser.rs).
pub use auth::{fetch_signing_keys, is_valid_tenant_id};
pub use protocol::RpcRequest;
pub use session::{session_id_log_hash, Session, SessionId, SessionStore};
pub use sse::{router as sse_router, AppState};
// Re-exported for the `travsr refs` / `travsr pattern` CLI subcommands (#299),
// which run the same occurrence-store read as the MCP tools against a locally
// opened store. The rest of `tools` stays private (MCP-only surface).
pub use tools::{
    find_pattern, find_pattern_raw, find_references, find_references_structured, ResolvedSymbol,
    StructuredReferences,
};
// Re-exported for the `travsr graph` CLI subcommand.
pub use query::AMBIGUOUS_DISPLAY_LIMIT;
// #645 WS-B: the CLI `status` surface reuses this exact classifier so the CLI
// and MCP notes never disagree about an index/HEAD mismatch.
pub use tools::head_index_mismatch_note;
// The CLI surfaces the same call-graph completeness note as the MCP tools.
// Shared rather than reimplemented: the doc on `phase_b_degraded_note` already
// promised the two would never disagree, and until now only the MCP side used
// it, so a terminal user got an empty answer with no indication why.
pub use tools::phase_b_degraded_note;
// #448: exported solely so `travsr-daemon` can assert its own `SKIP_DIRS` still
// matches this copy. The dependency edge runs daemon → mcp, so the equality can
// only be checked from that side; without it a future edit to the daemon's list
// silently widens `find_pattern` past the graph's file set.
pub use tools::SKIP_DIRS;
// RFC-021 P5: model distribution. The daemon auto-fetches on warm; the
// `travsr rerank` CLI subcommand drives the same install path. The rest of
// `rerank` stays private (query-path internals).
pub use rerank::{
    install_model_blocking as install_rerank_model, model_installed as rerank_model_installed,
};

use std::path::Path;

use anyhow::Context as _;
use travsr_store::SqliteStore;

pub(crate) const PROTOCOL_VERSION: &str = "2024-11-05";
pub(crate) const SERVER_NAME: &str = "travsr";

/// The version reported in `initialize`'s `serverInfo`, over both stdio and SSE.
///
/// Reads this crate's own `Cargo.toml`, independently of `travsr-cli`'s. Nothing
/// at the Cargo level keeps the two in lockstep (`verify-version` in release.yml
/// only pins `travsr-cli` and npm's `package.json` to the tag); they agree today
/// only because both files say `1.0.0`. The only thing that would catch a release
/// that bumps one and forgets the other is `check_mcp_server_version_matches_cli`
/// in `travsrAutomation`, which runs on every PR via `run.py --binary` in `ci.yml`.
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
    // #645 WS-B: record the caller's checkout (the IDE launches the stdio server
    // in the workspace dir) so structural tools can flag an index built for a
    // different commit than the live HEAD.
    if let Ok(cwd) = std::env::current_dir() {
        tools::set_launch_cwd(cwd);
    }
    // Inject the embed KNN hook so get_context's semantic seed selection (Step 4)
    // works in standalone `travsr mcp --stdio` mode, not just daemon mode.
    inject_embed_hook(&mut store, db_path);
    // RFC-021: background-warm the reranker (idempotent, non-blocking) so the
    // first real query doesn't pay the model-load cost.
    rerank::warm_background();
    server::run(&mut store)
}

/// Start the global MCP stdio server backed by all repos in `~/.travsr/registry.json`.
///
/// The registry is re-read on every tool call so repos added via `travsr init`
/// after startup are picked up live without restarting the server.
pub fn serve_stdio_global() -> anyhow::Result<()> {
    // #645 WS-B: same checkout capture as `serve_stdio` (see there).
    if let Ok(cwd) = std::env::current_dir() {
        tools::set_launch_cwd(cwd);
    }
    rerank::warm_background();
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
        embed_backends, lookup_embed_backend, EmbedQueryHook, EmbedSupervisor,
    };
    use travsr_store::{EmbedKnnHook, EmbedReadiness, EmbedScoreHook};

    // Guard: no embed.db → nothing to query; skip to avoid spawning a sidecar
    // against a non-existent HNSW index.
    if !db_path.with_file_name("embed.db").exists() {
        return;
    }

    let Some(home) = dirs::home_dir() else { return };
    // Prefer the repo's own `.travsr/embed.toml` override, then the user's
    // machine-wide active backend from ~/.travsr/embed.toml, then the catalog
    // default so a fresh install without `travsr embed switch` still works.
    // Mirrors `resolve_backend`'s resolution order (travsr-plugin-host) — this
    // is a per-repo embedding decision, not an install/list/hint path, so it
    // must not resolve on `active_backend_id()` alone (#547).
    let backend = hook_backend_id(db_path)
        .as_deref()
        .and_then(lookup_embed_backend)
        .or_else(|| embed_backends().first())
        .cloned();
    let Some(backend) = backend else { return };

    let binary = home
        .join(".travsr")
        .join("bin")
        .join(backend.binary_filename());
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

    // #376 Phase 2: parallel slot for the doc-space hook, armed by the same
    // background thread right after the code hook. Stays `None` forever when
    // the sidecar predates doc-space support or the repo has no doc-chunk
    // nodes — `EmbedSupervisor::doc_knn_hook` returns `None` in both cases.
    let doc_slot: Arc<Mutex<Option<EmbedKnnHook>>> = Arc::new(Mutex::new(None));
    let doc_slot_bg = Arc::clone(&doc_slot);

    // RFC-019: parallel slot for the query-embedding hook, armed by the same
    // background init thread. None = sidecar still warming (no direct-cosine
    // scoring yet, so the classifier/ranker use the FTS-only path).
    let score_slot: Arc<Mutex<Option<EmbedQueryHook>>> = Arc::new(Mutex::new(None));
    let score_slot_bg = Arc::clone(&score_slot);

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
                        // RFC-019: arm the query-embedding hook before the KNN slot
                        // so a query that observes `Some(knn)` also observes the
                        // score hook (never a half-armed state).
                        if let Some(qhook) = supervisor.embed_query_hook() {
                            if let Ok(mut guard) = score_slot_bg.lock() {
                                *guard = Some(qhook);
                            }
                        }
                        if let Ok(mut guard) = slot_bg.lock() {
                            *guard = Some(hook);
                        }
                        // #376 Phase 2: arm the doc hook alongside the code hook.
                        // `None` when the sidecar predates doc-space support or
                        // has no doc-space index — `doc_slot` then simply stays
                        // empty forever, and `doc_lane_seeds` (seed.rs) treats an
                        // absent hook as "docs unavailable", not an error.
                        if let Some(doc_hook) = supervisor.doc_knn_hook(mid.clone()) {
                            if let Ok(mut guard) = doc_slot_bg.lock() {
                                *guard = Some(doc_hook);
                            }
                        }
                        // Signal arm-complete AFTER the slots are populated so any
                        // thread woken by `mark_ready` sees `Some(hook)`.
                        readiness_bg.mark_ready();
                        tracing::info!(
                            model_id = %mid,
                            "embed plugin active, Step 4 (semantic ANN) enabled"
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

    // #376 Phase 2: doc-space meta-hook, same lazy-slot shape as `meta` above.
    // While `doc_slot` is `None` (sidecar warming, unsupported, or no doc-chunk
    // nodes) this returns an empty vec — `doc_lane_seeds` then emits no docs
    // section, never a stall or an error.
    let meta_doc: EmbedKnnHook = Arc::new(move |query: &str, k: u32| {
        let guard = doc_slot
            .lock()
            .map_err(|_| StoreError::Database("embed doc hook slot poisoned".into()))?;
        match guard.as_ref() {
            None => Ok(vec![]),
            Some(hook) => hook(query, k),
        }
    });
    store.set_embed_doc_knn_hook(meta_doc);

    // RFC-019: meta direct-cosine oracle hook. Reads the lazily-armed query hook;
    // while the sidecar warms (slot None) it scores nothing, so the classifier and
    // ranker use the FTS-only path — identical to the pre-RFC behaviour.
    let embed_db = db_path.with_file_name("embed.db");
    let score_model = backend.id.to_string();
    let meta_score: EmbedScoreHook = Arc::new(move |query: &str, ids: &[travsr_core::NodeId]| {
        let guard = score_slot
            .lock()
            .map_err(|_| StoreError::Database("embed score slot poisoned".into()))?;
        let Some(qhook) = guard.as_ref() else {
            return Ok(vec![]); // sidecar still warming — no scoring yet
        };
        let blob = qhook(query)?;
        match travsr_store::decode_embedding(&blob) {
            Some(qv) => travsr_store::score_candidates(&qv, &embed_db, &score_model, ids),
            None => Ok(vec![]),
        }
    });
    store.set_embed_score_hook(meta_score);
    tracing::info!("embed plugin hook installed (lazy, sidecar starting in background)");
}

/// Resolve the embed backend id to use for hook injection at `db_path`,
/// preferring the repo's own `.travsr/embed.toml` override over the
/// machine-wide `~/.travsr/embed.toml` default. Mirrors `resolve_backend`'s
/// resolution order in travsr-plugin-host — hook injection is a per-repo
/// embedding decision, so it must not resolve on `active_backend_id()` alone
/// (#547: a repo-level `travsr embed switch` was silently ignored, causing
/// `travsr mcp --stdio` to spawn the wrong sidecar model and disable Step 4).
///
/// Same helper as `travsr-daemon`'s `hook_backend_id` (#526), which fixed the
/// identical resolution order in the daemon's own hook injection.
///
/// `parent().parent()` assumes `db_path` is `<repo>/.travsr/graph.db`, which
/// holds for the default path. An explicit `travsr mcp --stdio --db <path>`
/// outside a repo yields no `embed.toml` there and falls through to the
/// machine default, which is the pre-existing behaviour and not a regression.
/// `resolve_backend_paths` on the spawn side makes the same assumption from
/// the same input, so hook and spawn still agree in that case, which is the
/// invariant that matters (#770 review).
fn hook_backend_id(db_path: &Path) -> Option<String> {
    use travsr_plugin_host::{active_backend_id, repo_backend_id};

    let repo_root = db_path.parent().and_then(|p| p.parent());
    repo_root
        .and_then(repo_backend_id)
        .or_else(active_backend_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    // HOME is process-global and Rust tests run in parallel, so every test
    // that mutates it must serialize on ONE lock. Reuses the existing
    // `seed::DOCS_ENV_LOCK` rather than declaring a second: these tests and
    // the docs-lane tests share the `travsr_mcp` lib test binary, so two
    // disjoint mutexes would serialize each group against itself while still
    // racing the other. That is not hypothetical here; DOCS_ENV_LOCK's own doc
    // records it happening once already, and #770's review demonstrated this
    // pair interleaving (#770 review).
    use crate::seed::DOCS_ENV_LOCK as ENV_LOCK;

    /// #547: hook injection must prefer a repo's own `.travsr/embed.toml`
    /// override over the machine-wide `~/.travsr/embed.toml` default, the
    /// same resolution order `resolve_backend` (travsr-plugin-host) uses.
    #[test]
    fn hook_backend_id_prefers_repo_config_over_global() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());

        std::fs::create_dir_all(home.path().join(".travsr")).unwrap();
        std::fs::write(
            home.path().join(".travsr").join("embed.toml"),
            "active = \"global-backend\"\n",
        )
        .unwrap();

        let repo_travsr = repo.path().join(".travsr");
        std::fs::create_dir_all(&repo_travsr).unwrap();
        std::fs::write(
            repo_travsr.join("embed.toml"),
            "active = \"repo-backend\"\n",
        )
        .unwrap();

        let resolved = hook_backend_id(&repo_travsr.join("graph.db"));

        match old_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }

        assert_eq!(
            resolved.as_deref(),
            Some("repo-backend"),
            "repo's .travsr/embed.toml must win over the machine-wide default"
        );
    }

    /// #547: with no repo-level override the machine-wide default still applies,
    /// so the fix narrows nothing that used to work.
    #[test]
    #[cfg_attr(
        windows,
        ignore = "dirs::home_dir() on Windows ignores HOME/USERPROFILE entirely (SHGetKnownFolderPath) - this test's isolation cannot work there, see crates/travsr-cli/tests/embed_switch.rs's module doc comment"
    )]
    fn hook_backend_id_falls_back_to_global_when_repo_unconfigured() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());

        std::fs::create_dir_all(home.path().join(".travsr")).unwrap();
        std::fs::write(
            home.path().join(".travsr").join("embed.toml"),
            "active = \"global-backend\"\n",
        )
        .unwrap();

        // No repo .travsr/embed.toml written at all.
        let resolved = hook_backend_id(&repo.path().join(".travsr").join("graph.db"));

        match old_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }

        assert_eq!(resolved.as_deref(), Some("global-backend"));
    }
}
