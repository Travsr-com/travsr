#![forbid(unsafe_code)]
//! travsr-plugin-host — owns the trust boundary between the daemon and plugins.
//!
//! The ONLY crate in the indexer tier that depends on travsr-plugin-protocol.
//! Neither travsr-indexer nor travsr-ingest reference it directly (CLAUDE.md).

pub mod dispatcher;
pub mod sandbox;
pub mod supervisor;
pub mod transport;

pub use dispatcher::Dispatcher;
pub use sandbox::policy::{SandboxPolicy, SandboxUnavailable};
pub use transport::{InProcess, PluginHealth, Sidecar, Transport};
