//! travsr-store — pluggable graph storage backend.
//!
//! The MVP uses SQLite (WAL) via `rusqlite`; production replaces this with
//! Kùzu, and hyperscale moves to RocksDB. All backends implement the
//! `Store` trait below.

#![forbid(unsafe_code)]

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use travsr_core::{Edge, EdgeKind, Node, NodeId, VName};

/// The current schema version. Bump when adding a migration.
const SCHEMA_VERSION: u32 = 1;

/// Migration scripts applied in order. Index 0 takes the DB from version 0
/// (fresh) to version 1.
const MIGRATIONS: &[&str] = &[include_str!("migrations/v1_initial.sql")];

/// The storage interface every Travsr backend must satisfy.
pub trait Store {
    /// Persist a node, returning its assigned id.
    fn put_node(&mut self, node: &Node) -> Result<NodeId>;
    /// Persist an edge.
    fn put_edge(&mut self, edge: &Edge) -> Result<()>;
    /// Look up a node by id.
    fn get_node(&self, id: NodeId) -> Result<Option<Node>>;
    /// Return every outgoing edge from `src`.
    fn iter_edges_from(&self, src: NodeId) -> Result<Vec<Edge>>;
    /// Return every incoming edge to `dst`.
    fn iter_edges_to(&self, dst: NodeId) -> Result<Vec<Edge>>;
}

/// SQLite-backed store. The MVP target — zero setup, single file on disk.
#[derive(Debug)]
pub struct SqliteStore {
    conn: Connection,
}

impl SqliteStore {
    /// Open (or create) a SQLite-backed store at `path`, enabling WAL and
    /// running any pending migrations.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening sqlite database at {}", path.display()))?;
        Self::configure(&conn)?;
        Self::migrate(&conn)?;
        Ok(Self { conn })
    }

    /// Open an in-memory SQLite store. Used in tests; WAL is not available
    /// on `:memory:` connections, so journal mode falls back to MEMORY.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("opening in-memory sqlite database")?;
        Self::migrate(&conn)?;
        Ok(Self { conn })
    }

    fn configure(conn: &Connection) -> Result<()> {
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("enabling WAL journal mode")?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .context("setting synchronous=NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .context("enabling foreign_keys pragma")?;
        Ok(())
    }

    fn migrate(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )
        .context("creating meta table")?;

        let current: u32 = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .context("reading schema_version")?
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        for (idx, sql) in MIGRATIONS.iter().enumerate() {
            let target = idx as u32 + 1;
            if current >= target {
                continue;
            }
            conn.execute_batch(sql)
                .with_context(|| format!("applying migration v{target}"))?;
        }

        conn.execute(
            "INSERT INTO meta(key, value) VALUES('schema_version', ?1) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![SCHEMA_VERSION.to_string()],
        )
        .context("recording schema_version")?;

        Ok(())
    }

    /// Return the live journal mode reported by SQLite. Useful in tests.
    pub fn journal_mode(&self) -> Result<String> {
        self.conn
            .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
            .context("querying journal_mode")
    }

    pub fn node_count(&self) -> Result<u64> {
        let n: i64 = self
            .conn
            .query_row("SELECT count(*) FROM nodes", [], |row| row.get(0))
            .context("counting nodes")?;
        Ok(n as u64)
    }

    pub fn edge_count(&self) -> Result<u64> {
        let n: i64 = self
            .conn
            .query_row("SELECT count(*) FROM edges", [], |row| row.get(0))
            .context("counting edges")?;
        Ok(n as u64)
    }

    pub fn get_file_hash(&self, path: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT sha256 FROM files WHERE path = ?1",
                params![path],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .context("reading file hash")
    }

    pub fn put_file_hash(&mut self, path: &str, hex: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO files(path, sha256, last_indexed_at) \
                 VALUES(?1, ?2, unixepoch()) \
                 ON CONFLICT(path) DO UPDATE SET \
                   sha256 = excluded.sha256, \
                   last_indexed_at = excluded.last_indexed_at",
                params![path, hex],
            )
            .context("writing file hash")?;
        Ok(())
    }

    /// Delete all nodes (and their edges) whose VName path equals `path`.
    /// Edges are deleted first to avoid orphan references; both operations
    /// run inside a single transaction.  Returns the number of nodes removed.
    pub fn delete_nodes_for_path(&mut self, path: &str) -> Result<u64> {
        let tx = self
            .conn
            .transaction()
            .context("starting delete transaction")?;

        let count: i64 = tx
            .query_row(
                "SELECT count(*) FROM nodes WHERE path = ?1",
                params![path],
                |row| row.get(0),
            )
            .context("counting nodes to delete")?;

        // Edges have no FK cascade — delete them explicitly before removing nodes.
        tx.execute(
            "DELETE FROM edges \
             WHERE src IN (SELECT id FROM nodes WHERE path = ?1) \
                OR dst IN (SELECT id FROM nodes WHERE path = ?1)",
            params![path],
        )
        .context("deleting edges for path")?;

        tx.execute("DELETE FROM nodes WHERE path = ?1", params![path])
            .context("deleting nodes for path")?;

        tx.commit().context("committing delete transaction")?;
        Ok(count as u64)
    }

    pub fn search_nodes_by_name(&self, name: &str) -> Result<Vec<Node>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, corpus, root, path, language, signature, kind \
                 FROM nodes WHERE signature LIKE '%' || ?1 || '%' \
                    OR path LIKE '%' || ?1 || '%'",
            )
            .context("preparing search query")?;

        let rows = stmt
            .query_map(params![name], |row| {
                let id = i64_to_node_id(row.get::<_, i64>(0)?);
                let vname = VName::new(
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                );
                let kind: String = row.get(6)?;
                Ok(Node { id, vname, kind })
            })
            .context("executing search query")?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row.context("decoding search row")?);
        }
        Ok(out)
    }

    pub fn all_nodes(&self) -> Result<Vec<Node>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, corpus, root, path, language, signature, kind FROM nodes")
            .context("preparing all_nodes query")?;
        let rows = stmt
            .query_map([], |row| {
                let id = i64_to_node_id(row.get::<_, i64>(0)?);
                let vname = VName::new(
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                );
                let kind: String = row.get(6)?;
                Ok(Node { id, vname, kind })
            })
            .context("executing all_nodes query")?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.context("decoding all_nodes row")?);
        }
        Ok(out)
    }

    pub fn all_edges(&self) -> Result<Vec<(NodeId, NodeId, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT src, dst, kind FROM edges")
            .context("preparing all_edges query")?;
        let rows = stmt
            .query_map([], |row| {
                let src = i64_to_node_id(row.get::<_, i64>(0)?);
                let dst = i64_to_node_id(row.get::<_, i64>(1)?);
                let kind: String = row.get(2)?;
                Ok((src, dst, kind))
            })
            .context("executing all_edges query")?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.context("decoding all_edges row")?);
        }
        Ok(out)
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![key],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .context("reading meta key")
    }

    pub fn set_meta(&mut self, key: &str, value: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO meta(key, value) VALUES(?1, ?2) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .context("writing meta key")?;
        Ok(())
    }
}

impl Store for SqliteStore {
    fn put_node(&mut self, node: &Node) -> Result<NodeId> {
        let id_i64 = node_id_to_i64(node.id);
        self.conn
            .execute(
                "INSERT INTO nodes(id, corpus, root, path, language, signature, kind) \
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                 ON CONFLICT(id) DO UPDATE SET kind = excluded.kind",
                params![
                    id_i64,
                    node.vname.corpus,
                    node.vname.root,
                    node.vname.path,
                    node.vname.language,
                    node.vname.signature,
                    node.kind,
                ],
            )
            .context("inserting node")?;
        Ok(node.id)
    }

    fn put_edge(&mut self, edge: &Edge) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO edges(src, dst, kind) VALUES(?1, ?2, ?3)",
                params![
                    node_id_to_i64(edge.src),
                    node_id_to_i64(edge.dst),
                    edge.kind.as_str(),
                ],
            )
            .context("inserting edge")?;
        Ok(())
    }

    fn get_node(&self, id: NodeId) -> Result<Option<Node>> {
        let row = self
            .conn
            .query_row(
                "SELECT corpus, root, path, language, signature, kind \
                 FROM nodes WHERE id = ?1",
                params![node_id_to_i64(id)],
                |row| {
                    let vname = VName::new(
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    );
                    let kind: String = row.get(5)?;
                    Ok(Node { id, vname, kind })
                },
            )
            .optional()
            .context("querying node by id")?;
        Ok(row)
    }

    fn iter_edges_from(&self, src: NodeId) -> Result<Vec<Edge>> {
        let mut stmt = self
            .conn
            .prepare("SELECT dst, kind FROM edges WHERE src = ?1")
            .context("preparing iter_edges_from query")?;
        let rows = stmt
            .query_map(params![node_id_to_i64(src)], |row| {
                let dst_i64: i64 = row.get(0)?;
                let kind_str: String = row.get(1)?;
                Ok((dst_i64, kind_str))
            })
            .context("executing iter_edges_from query")?;

        let mut out = Vec::new();
        for row in rows {
            let (dst_i64, kind_str) = row.context("decoding edge row")?;
            let kind = EdgeKind::from_str(&kind_str)
                .with_context(|| format!("unknown edge kind in storage: {kind_str}"))?;
            out.push(Edge::new(src, i64_to_node_id(dst_i64), kind));
        }
        Ok(out)
    }

    fn iter_edges_to(&self, dst: NodeId) -> Result<Vec<Edge>> {
        let mut stmt = self
            .conn
            .prepare("SELECT src, kind FROM edges WHERE dst = ?1")
            .context("preparing iter_edges_to query")?;
        let rows = stmt
            .query_map(params![node_id_to_i64(dst)], |row| {
                let src_i64: i64 = row.get(0)?;
                let kind_str: String = row.get(1)?;
                Ok((src_i64, kind_str))
            })
            .context("executing iter_edges_to query")?;

        let mut out = Vec::new();
        for row in rows {
            let (src_i64, kind_str) = row.context("decoding edge row")?;
            let kind = EdgeKind::from_str(&kind_str)
                .with_context(|| format!("unknown edge kind in storage: {kind_str}"))?;
            out.push(Edge::new(i64_to_node_id(src_i64), dst, kind));
        }
        Ok(out)
    }
}

fn node_id_to_i64(id: NodeId) -> i64 {
    id.0 as i64
}

fn i64_to_node_id(v: i64) -> NodeId {
    NodeId(v as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use travsr_core::VName;

    fn sample_node(sig: &str) -> Node {
        Node::new(
            VName::new("test-corpus", "main", "src/foo.ts", "typescript", sig),
            "function",
        )
    }

    #[test]
    fn migration_is_idempotent_in_memory() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        // Re-running the migration on the same connection must not error.
        SqliteStore::migrate(&store.conn).unwrap();
        SqliteStore::migrate(&store.conn).unwrap();
        let n = sample_node("fn:roundtrip");
        store.put_node(&n).unwrap();
    }

    #[test]
    fn put_and_get_node_round_trip() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let n = sample_node("fn:rt");
        let id = store.put_node(&n).unwrap();
        let back = store.get_node(id).unwrap().expect("node missing");
        assert_eq!(back, n);
    }

    #[test]
    fn put_and_iter_edges() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let a = sample_node("fn:a");
        let b = sample_node("fn:b");
        let c = sample_node("fn:c");
        store.put_node(&a).unwrap();
        store.put_node(&b).unwrap();
        store.put_node(&c).unwrap();
        store
            .put_edge(&Edge::new(a.id, b.id, EdgeKind::RefCall))
            .unwrap();
        store
            .put_edge(&Edge::new(a.id, c.id, EdgeKind::Depends))
            .unwrap();
        // Insert the same edge twice — should be a no-op.
        store
            .put_edge(&Edge::new(a.id, b.id, EdgeKind::RefCall))
            .unwrap();

        let mut edges = store.iter_edges_from(a.id).unwrap();
        edges.sort_by_key(|e| (e.dst.0, e.kind.as_str()));
        assert_eq!(edges.len(), 2);
        assert!(edges
            .iter()
            .any(|e| e.dst == b.id && e.kind == EdgeKind::RefCall));
        assert!(edges
            .iter()
            .any(|e| e.dst == c.id && e.kind == EdgeKind::Depends));
    }

    #[test]
    fn get_missing_node_returns_none() {
        let store = SqliteStore::open_in_memory().unwrap();
        assert!(store.get_node(NodeId(123_456_789)).unwrap().is_none());
    }

    #[test]
    fn wal_journal_mode_enabled_on_file_backed_store() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("graph.db");
        let store = SqliteStore::open(&db_path).unwrap();
        let mode = store.journal_mode().unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }

    #[test]
    fn reopening_existing_db_keeps_data() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("graph.db");
        let n = sample_node("fn:persist");
        let id = {
            let mut store = SqliteStore::open(&db_path).unwrap();
            store.put_node(&n).unwrap()
        };
        let store = SqliteStore::open(&db_path).unwrap();
        assert_eq!(store.get_node(id).unwrap().as_ref(), Some(&n));
    }

    fn node_with_path(path: &str, sig: &str) -> Node {
        Node::new(VName::new("", "", path, "typescript", sig), "function")
    }

    #[test]
    fn file_hash_round_trips() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        store.put_file_hash("src/foo.ts", "abc123").unwrap();
        let got = store.get_file_hash("src/foo.ts").unwrap();
        assert_eq!(got, Some("abc123".to_string()));
        assert!(store.get_file_hash("nonexistent.ts").unwrap().is_none());
    }

    #[test]
    fn delete_nodes_for_path_removes_nodes_and_edges() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let a = node_with_path("a.ts", "fn:foo");
        let b = node_with_path("a.ts", "fn:bar");
        store.put_node(&a).unwrap();
        store.put_node(&b).unwrap();
        store
            .put_edge(&Edge::new(a.id, b.id, EdgeKind::DefinesBinding))
            .unwrap();
        assert_eq!(store.node_count().unwrap(), 2);
        assert_eq!(store.edge_count().unwrap(), 1);

        let deleted = store.delete_nodes_for_path("a.ts").unwrap();
        assert_eq!(deleted, 2);
        assert_eq!(store.node_count().unwrap(), 0);
        assert_eq!(store.edge_count().unwrap(), 0);
    }

    #[test]
    fn delete_nodes_for_path_leaves_other_paths() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let a = node_with_path("a.ts", "fn:foo");
        let b = node_with_path("b.ts", "fn:bar");
        store.put_node(&a).unwrap();
        store.put_node(&b).unwrap();
        store
            .put_edge(&Edge::new(a.id, b.id, EdgeKind::Depends))
            .unwrap();

        store.delete_nodes_for_path("a.ts").unwrap();

        assert_eq!(store.node_count().unwrap(), 1, "b.ts node must survive");
        assert_eq!(
            store.edge_count().unwrap(),
            0,
            "edge referencing a.ts must be removed"
        );
        assert!(store.get_node(b.id).unwrap().is_some());
    }

    #[test]
    fn search_nodes_by_name_finds_partial_match() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let n = node_with_path("svc.ts", "fn:charge");
        store.put_node(&n).unwrap();
        let results = store.search_nodes_by_name("charge").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].vname.signature, "fn:charge");
    }

    #[test]
    fn meta_get_set_round_trips() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        assert!(store.get_meta("foo").unwrap().is_none());
        store.set_meta("foo", "bar").unwrap();
        assert_eq!(store.get_meta("foo").unwrap(), Some("bar".to_string()));
        store.set_meta("foo", "baz").unwrap();
        assert_eq!(store.get_meta("foo").unwrap(), Some("baz".to_string()));
    }

    #[test]
    fn iter_edges_to_returns_incoming_edges() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let a = node_with_path("a.ts", "fn:a");
        let b = node_with_path("b.ts", "fn:b");
        let c = node_with_path("c.ts", "fn:c");
        store.put_node(&a).unwrap();
        store.put_node(&b).unwrap();
        store.put_node(&c).unwrap();
        store
            .put_edge(&Edge::new(a.id, c.id, EdgeKind::DefinesBinding))
            .unwrap();
        store
            .put_edge(&Edge::new(b.id, c.id, EdgeKind::DefinesBinding))
            .unwrap();

        let edges = store.iter_edges_to(c.id).unwrap();
        let srcs: Vec<NodeId> = edges.iter().map(|e| e.src).collect();
        assert_eq!(edges.len(), 2, "both A→C and B→C must be returned");
        assert!(srcs.contains(&a.id), "A must appear as a caller");
        assert!(srcs.contains(&b.id), "B must appear as a caller");
        assert!(
            edges.iter().all(|e| e.dst == c.id),
            "dst must be C for all edges"
        );
    }
}
