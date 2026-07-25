//! Unified error taxonomy for all Travsr public APIs.
//!
//! See `docs/adrs/ADR-004-error-taxonomy.md` for the full policy.
//!
//! # Key rule: data-absent is NOT an error
//! When a symbol or file is not found, public functions return `Ok(empty)`.
//! Only structural failures (DB unavailable, parse failure, budget exceeded)
//! surface as `TravsrError`.

#![forbid(unsafe_code)]

use thiserror::Error;

// ── Top-level error ───────────────────────────────────────────────────────────

/// Unified error type returned by all Travsr public APIs.
///
/// MCP JSON-RPC error code mapping: see [`TravsrError::mcp_error_code`].
#[derive(Debug, Error)]
pub enum TravsrError {
    /// Bad input from an MCP client or CLI caller.
    /// Maps to JSON-RPC -32602 (INVALID_PARAMS).
    #[error("invalid parameter: {0}")]
    InvalidParams(String),

    /// Storage backend failure.
    /// Maps to JSON-RPC -32001.
    #[error("store error: {0}")]
    Store(#[from] StoreError),

    /// Tree-sitter / LSIF parse failure.
    /// Maps to JSON-RPC -32003.
    #[error("index error: {0}")]
    Index(#[from] IndexError),

    /// Graph traversal failure (PPR, BFS, PCST).
    /// Maps to JSON-RPC -32004.
    #[error("retrieval error: {0}")]
    Retrieval(#[from] RetrievalError),

    /// Token budget exhausted before the result set could be assembled.
    /// Maps to JSON-RPC -32002.
    #[error("token budget exceeded: requested {requested}, limit {limit}")]
    BudgetExceeded { requested: usize, limit: usize },

    /// Unexpected internal failure with no more specific variant.
    /// Maps to JSON-RPC -32000.
    #[error("internal error: {0}")]
    Internal(String),
}

impl TravsrError {
    /// JSON-RPC error code to use when surfacing this error to an MCP client.
    ///
    /// # MCP tool error mapping
    ///
    /// | Tool               | InvalidParams trigger        | Engine error trigger        |
    /// |--------------------|------------------------------|-----------------------------|
    /// | `get_dependencies` | unknown / escaped file path  | store read failure          |
    /// | `get_callers`      | malformed symbol name        | traversal failure           |
    /// | `get_blast_radius` | unknown file path            | graph computation failure   |
    /// | `search_symbol`    | empty query string           | index unavailable           |
    /// | `get_context`      | `token_budget = 0`           | PPR divergence              |
    /// | `get_repo_map`     | unknown repo identifier      | store unavailable           |
    pub fn mcp_error_code(&self) -> i32 {
        match self {
            TravsrError::InvalidParams(_) => -32602,
            TravsrError::Store(_) => -32001,
            TravsrError::Index(_) => -32003,
            TravsrError::Retrieval(_) => -32004,
            TravsrError::BudgetExceeded { .. } => -32002,
            TravsrError::Internal(_) => -32000,
        }
    }
}

// ── Sub-error types ───────────────────────────────────────────────────────────

/// Errors originating in the storage layer (SQLite, RocksDB).
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Database(String),

    #[error("schema migration failed: {0}")]
    Migration(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Errors originating in the indexing pipeline (Tree-sitter, LSIF).
#[derive(Debug, Error)]
pub enum IndexError {
    #[error("parse error in {file}: {message}")]
    Parse { file: String, message: String },

    #[error("LSIF ingest error: {0}")]
    Lsif(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("plugin phase not supported for this transport")]
    PhaseNotSupported,

    #[error("plugin protocol version mismatch: expected {expected}, got {got}")]
    ProtocolVersionMismatch { expected: u32, got: u32 },

    #[error("plugin reported unknown language: {reported:?}")]
    UnknownLanguage { reported: String },

    #[error("plugin {language:?} crashed")]
    PluginCrashed { language: String },
}

/// Errors originating in the retrieval layer (BFS, PPR, PCST).
#[derive(Debug, Error)]
pub enum RetrievalError {
    #[error("graph traversal error: {0}")]
    Traversal(String),

    #[error("PPR failed to converge after {iterations} iterations")]
    PprDivergence { iterations: usize },

    #[error("store error during retrieval: {0}")]
    Store(#[from] StoreError),
}
