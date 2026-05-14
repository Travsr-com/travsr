//! travsr-store — pluggable graph storage backend.
//!
//! The MVP uses SQLite (WAL) via `rusqlite`; production replaces this with
//! Kùzu, and hyperscale moves to RocksDB. All backends implement the
//! `Store` trait below.

#![forbid(unsafe_code)]

use std::path::Path;

use travsr_core::{Edge, Node, NodeId};

/// The storage interface every Travsr backend must satisfy.
pub trait Store {
    /// Persist a node, returning its assigned id.
    fn put_node(&mut self, node: &Node) -> anyhow::Result<NodeId>;
    /// Persist an edge.
    fn put_edge(&mut self, edge: &Edge) -> anyhow::Result<()>;
    /// Look up the outgoing edges of a node.
    fn out_edges(&self, src: NodeId) -> anyhow::Result<Vec<Edge>>;
}

/// SQLite-backed store. The MVP target — zero setup, single file on disk.
#[derive(Debug)]
pub struct SqliteStore {
    _private: (),
}

impl SqliteStore {
    /// Open (or create) a SQLite-backed store at `path`.
    ///
    /// Stub — schema creation and WAL mode setup land in Sprint 1.
    pub fn open(_path: &Path) -> anyhow::Result<Self> {
        Ok(Self { _private: () })
    }
}
