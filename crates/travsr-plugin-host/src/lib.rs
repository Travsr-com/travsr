#![forbid(unsafe_code)]
//! travsr-plugin-host — owns the trust boundary between the daemon and plugins.
//!
//! The ONLY crate in the indexer tier that depends on travsr-plugin-protocol.
//! Neither travsr-indexer nor travsr-ingest reference it directly (CLAUDE.md).

pub mod cache;
pub mod dispatcher;
pub mod indexer;
pub mod phase_b;
pub mod plugins;
pub mod registry;
pub mod sandbox;
pub mod supervisor;
pub mod transport;
pub mod trust;

pub use dispatcher::Dispatcher;
pub use indexer::PluginIndexer;
pub use phase_b::{lookup as lookup_phase_b, CATALOG as PHASE_B_CATALOG, PhaseBEntry, OutputFormat, SandboxRequirement};
pub use sandbox::policy::{SandboxPolicy, SandboxUnavailable};
pub use transport::{InProcess, PluginHealth, Sidecar, Transport};
