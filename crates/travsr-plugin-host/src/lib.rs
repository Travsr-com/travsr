#![deny(unsafe_code)] // overridden only in sandbox/windows/ffi.rs (ADR-017 Amendment A2)
//! travsr-plugin-host — owns the trust boundary between the daemon and plugins.
//!
//! The ONLY crate in the indexer tier that depends on travsr-plugin-protocol.
//! Neither travsr-indexer nor travsr-ingest reference it directly (CLAUDE.md).

pub mod cache;
pub mod dispatcher;
pub mod embed_catalog;
pub mod embed_sidecar;
pub mod embed_supervisor;
pub mod governance;
pub mod indexer;
pub mod phase_b;
pub mod plugins;
pub mod registry;
pub mod resolver;
pub mod sandbox;
mod stderr_ring;
pub mod supervisor;
pub mod transport;
pub mod trust;
mod watchdog;

pub use dispatcher::Dispatcher;
pub use embed_catalog::{
    active_backend_id, backends as embed_backends, cancel_sentinel_path,
    derive_num_workers_for_cli, derive_phase1_threshold_for_status, embed_paused,
    embed_reindex_in_flight, embeddings_by_model, gc_embeddings, lookup as lookup_embed_backend,
    pause_embed, probe_top1_cosines, repo_backend_id, resolve_governance_for_db, resume_embed,
    run_parallel_reindex_blocking, run_parallel_reindex_blocking_quiet,
    spawn_background_reindex_all, spawn_background_reindex_phase1, spawn_background_reindex_phase2,
    terminate_inflight_reindex, write_model_descriptor, write_repo_backend_id, EmbedBackend,
    EmbedModelFile, EmbedOpLock, ModelUsage, MAX_EMBED_WORKERS,
};
pub use embed_sidecar::{EmbedCapabilities, EmbedError, EmbedSidecar};
pub use embed_supervisor::{EmbedQueryHook, EmbedSupervisor};
pub use governance::{Capacity, EmbedGovernance, EmbedOverrides, Priority};
pub use indexer::{PhaseBInputs, PhaseBOutcome, PluginIndexer};
pub use phase_b::{
    lookup as lookup_phase_b, OutputFormat, PhaseBEntry, SandboxRequirement,
    CATALOG as PHASE_B_CATALOG,
};
pub use registry::probe_sandbox;
pub use sandbox::policy::{SandboxPolicy, SandboxUnavailable};
pub use transport::{InProcess, PluginHealth, Sidecar, Transport};

/// Native Windows process-liveness probe (`OpenProcess` +
/// `GetExitCodeProcess`, no signal sent), for callers outside this crate
/// that need it and cannot themselves hold `unsafe` (`travsr-mcp` is
/// `#![forbid(unsafe_code)]`).
///
/// Delegates to [`sandbox::windows::pid_alive`], the same probe
/// `embed_catalog::pid_alive` already uses for the daemon shutdown grace
/// poll; `unsafe` stays confined to `sandbox/windows/ffi.rs` per ADR-017
/// Amendment A2 Invariant 1, this is a safe wrapper only.
///
/// #636 round-2 review: `travsr-mcp`'s `observability::pid_is_alive`
/// previously shelled out to `tasklist` on every call, which measurably
/// failed under the process-spawn contention a full `cargo test --workspace`
/// run puts on Windows CI (`CreateProcess` is comparatively expensive under
/// load there). A syscall has no such failure mode. Unix's `kill -0` stays a
/// subprocess call in every caller, unchanged: it was never the source of
/// that flakiness and doesn't need this treatment.
#[cfg(target_os = "windows")]
pub fn windows_pid_is_alive(pid: u32) -> bool {
    sandbox::windows::pid_alive(pid)
}

/// Formal plugin state — reported at startup and queryable by the daemon.
/// ADR-017 Rule 2: `Disabled` means no subprocess runs, not "degraded mode".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginState {
    /// Plugin is registered, sandbox available, tool on PATH — ready.
    Active,
    /// Sandbox mechanism absent on this host — fail-closed per ADR-017 Rule 2.
    SandboxUnavailableDisabled { mechanism: &'static str },
    /// Phase B tool not on PATH — registered but inactive until installed.
    ToolNotFound { command: String },
    /// Language not registered via `travsr lang add`.
    NotRegistered,
}
