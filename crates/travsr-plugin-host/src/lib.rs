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
pub mod indexer;
pub mod phase_b;
pub mod plugins;
pub mod registry;
pub mod resolver;
pub mod sandbox;
pub mod supervisor;
pub mod transport;
pub mod trust;

pub use dispatcher::Dispatcher;
pub use embed_catalog::{
    active_backend_id, lookup as lookup_embed_backend, spawn_background_reindex_all,
    spawn_background_reindex_phase1, spawn_background_reindex_phase2, EmbedBackend, EmbedModelFile,
    BACKENDS as EMBED_BACKENDS,
};
pub use embed_sidecar::{EmbedCapabilities, EmbedError, EmbedSidecar};
pub use embed_supervisor::EmbedSupervisor;
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
