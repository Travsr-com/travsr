//! travsr-daemon — long-running orchestrator for Travsr.
//!
//! Owns the git hook integration, file watcher, indexer scheduler, and
//! hosts the MCP server. "Always fresh" is the daemon's job — see
//! CLAUDE.md, principle #2.

#![forbid(unsafe_code)]

/// The Travsr daemon process.
#[derive(Debug, Default)]
pub struct Daemon {
    _private: (),
}

impl Daemon {
    /// Construct a new daemon with default configuration.
    pub fn new() -> Self {
        Self::default()
    }

    /// Run the daemon's event loop until shutdown.
    ///
    /// Stub — wiring lands across Sprints 2–3.
    pub async fn run() -> anyhow::Result<()> {
        tracing::info!("travsr-daemon stub: run() invoked");
        Ok(())
    }
}
