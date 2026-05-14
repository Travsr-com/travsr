//! travsr-mcp — Model Context Protocol server.
//!
//! Exposes Travsr's graph retrieval tools to MCP clients (Claude Desktop,
//! VS Code, etc.) over stdio (local) or SSE (cloud). Per CLAUDE.md, MCP is
//! the *only* external interface — no REST, no GraphQL.

#![forbid(unsafe_code)]

/// The Travsr MCP server.
#[derive(Debug, Default)]
pub struct McpServer {
    _private: (),
}

impl McpServer {
    /// Construct a new MCP server.
    pub fn new() -> Self {
        Self::default()
    }

    /// Serve the MCP protocol over stdio. Used by the local daemon for
    /// IDE / agent integration.
    ///
    /// Stub — protocol wiring lands in Sprint 3.
    pub async fn serve_stdio() -> anyhow::Result<()> {
        Ok(())
    }
}
