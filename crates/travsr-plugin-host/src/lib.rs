#![deny(unsafe_code)] // overridden only in sandbox/windows/ffi.rs (RFC-014 approved)
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
    embed_reindex_in_flight, lookup as lookup_embed_backend, pause_embed, probe_top1_cosines,
    repo_backend_id, resolve_governance_for_db, resume_embed, run_parallel_reindex_blocking,
    run_parallel_reindex_blocking_quiet, spawn_background_reindex_all,
    spawn_background_reindex_phase1, spawn_background_reindex_phase2, terminate_inflight_reindex,
    write_model_descriptor, write_repo_backend_id, EmbedBackend, EmbedModelFile, MAX_EMBED_WORKERS,
};
pub use embed_sidecar::{EmbedCapabilities, EmbedError, EmbedSidecar};
pub use embed_supervisor::{EmbedQueryHook, EmbedSupervisor};
pub use governance::{EmbedGovernance, EmbedOverrides, Priority};
pub use indexer::{PhaseBInputs, PhaseBOutcome, PluginIndexer};
pub use phase_b::{
    lookup as lookup_phase_b, OutputFormat, PhaseBEntry, SandboxRequirement,
    CATALOG as PHASE_B_CATALOG,
};
pub use registry::probe_sandbox;
pub use sandbox::policy::{SandboxPolicy, SandboxUnavailable};
pub use transport::{InProcess, PluginHealth, Sidecar, Transport};

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
