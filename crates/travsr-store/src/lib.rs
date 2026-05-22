//! travsr-store — pluggable graph storage backend.
//!
//! The MVP uses SQLite (WAL) via `rusqlite`; production replaces this with
//! Kùzu, and hyperscale moves to RocksDB. All backends implement the
//! `Store` trait below.

#![forbid(unsafe_code)]

pub mod migration;
pub mod migration_manifest;
pub mod registry;

#[cfg(feature = "kuzu")]
pub mod kuzu_store;
#[cfg(feature = "kuzu")]
pub use kuzu_store::KuzuStore;

pub use migration::{Migration, MigrationRunner, StoreMigratable};
pub use migration_manifest::compute_manifest_sqlite;
#[cfg(feature = "kuzu")]
pub use migration_manifest::{compute_manifest_kuzu, migrate_sqlite_to_kuzu};
pub use migration_manifest::{Manifest, ManifestEntry, MigrationError};

use std::path::Path;

use anyhow::{Context, Result as AnyResult};
use rusqlite::{params, Connection, OptionalExtension};
use travsr_core::{Edge, EdgeKind, Node, NodeId, VName};
use travsr_error::StoreError;

// ── SQLite migration structs (T2) ─────────────────────────────────────────────
// Each SQL file becomes a concrete Migration so the runner can apply them
// independently and track progress per-version rather than all-or-nothing.

struct V1Initial;
impl Migration for V1Initial {
    fn version(&self) -> u32 {
        1
    }
    fn up(&self, store: &mut dyn StoreMigratable) -> anyhow::Result<()> {
        store.exec_ddl(include_str!("migrations/v1_initial.sql"))
    }
}

struct V2EdgeProvenance;
impl Migration for V2EdgeProvenance {
    fn version(&self) -> u32 {
        2
    }
    fn up(&self, store: &mut dyn StoreMigratable) -> anyhow::Result<()> {
        // SQLite does not support `ALTER TABLE … ADD COLUMN IF NOT EXISTS`.
        // Guard manually so re-running after a crash (atomicity gap) is safe.
        if !store.column_exists("edges", "provenance")? {
            store.exec_ddl(include_str!("migrations/v2_edge_provenance.sql"))?;
        }
        Ok(())
    }
}

struct V3SignatureFormatVersion;
impl Migration for V3SignatureFormatVersion {
    fn version(&self) -> u32 {
        3
    }
    fn up(&self, store: &mut dyn StoreMigratable) -> anyhow::Result<()> {
        // INSERT OR IGNORE is idempotent — safe to re-run after a crash.
        store.exec_ddl(include_str!("migrations/v3_signature_format_version.sql"))
    }
}

struct V4EdgesSrcKindIdx;
impl Migration for V4EdgesSrcKindIdx {
    fn version(&self) -> u32 {
        4
    }
    fn up(&self, store: &mut dyn StoreMigratable) -> anyhow::Result<()> {
        // CREATE INDEX IF NOT EXISTS is idempotent — safe to re-run after a crash.
        store.exec_ddl(include_str!("migrations/v4_edges_src_kind_idx.sql"))
    }
}

struct V5LanguagePackage;
impl Migration for V5LanguagePackage {
    fn version(&self) -> u32 {
        5
    }
    fn up(&self, store: &mut dyn StoreMigratable) -> anyhow::Result<()> {
        // ALTER TABLE … ADD COLUMN has no IF NOT EXISTS in SQLite.
        // Guard manually so re-running after a crash (atomicity gap) is safe.
        if !store.column_exists("nodes", "package")? {
            store.exec_ddl("ALTER TABLE nodes ADD COLUMN package TEXT NOT NULL DEFAULT ''")?;
        }
        if !store.column_exists("edges", "language")? {
            store.exec_ddl(
                "ALTER TABLE edges ADD COLUMN language TEXT NOT NULL DEFAULT 'typescript'",
            )?;
        }
        // CREATE INDEX IF NOT EXISTS is idempotent.
        store.exec_ddl(include_str!("migrations/v5_language_package.sql"))
    }
}

/// Build the ordered migration runner for the SQLite backend.
/// Register new SQLite migrations here; version order is enforced by the runner.
fn sqlite_migration_runner() -> MigrationRunner {
    let mut r = MigrationRunner::new();
    r.register(V1Initial);
    r.register(V2EdgeProvenance);
    r.register(V3SignatureFormatVersion);
    r.register(V4EdgesSrcKindIdx);
    r.register(V5LanguagePackage);
    r
}

/// The storage interface every Travsr backend must satisfy.
///
/// All methods return `Result<T, StoreError>` (from `travsr-error`).
/// Callers that use `?` in an `anyhow::Result` context get automatic
/// conversion via `anyhow::Error: From<StoreError>`.
pub trait Store {
    /// Persist a node, returning its assigned id.
    fn put_node(&mut self, node: &Node) -> Result<NodeId, StoreError>;
    /// Persist an edge.
    fn put_edge(&mut self, edge: &Edge) -> Result<(), StoreError>;
    /// Look up a node by id.
    fn get_node(&self, id: NodeId) -> Result<Option<Node>, StoreError>;
    /// Return every outgoing edge from `src`.
    fn iter_edges_from(&self, src: NodeId) -> Result<Vec<Edge>, StoreError>;
    /// Return outgoing edges from `src` filtered to a single [`EdgeKind`].
    ///
    /// Implementations **must** use an indexed `WHERE kind = ?` clause so that
    /// PPR traversal (which filters by kind at every step) meets the p95 < 50ms
    /// budget on graphs up to the MVP ceiling (75M nodes / 2.5B edges).
    ///
    /// The default implementation delegates to [`Store::iter_edges_from`] and
    /// filters in Rust — backends without a kind index should override this.
    fn iter_edges_from_kind(&self, src: NodeId, kind: EdgeKind) -> Result<Vec<Edge>, StoreError> {
        Ok(self
            .iter_edges_from(src)?
            .into_iter()
            .filter(|e| e.kind == kind)
            .collect())
    }
    /// Return every incoming edge to `dst`.
    fn iter_edges_to(&self, dst: NodeId) -> Result<Vec<Edge>, StoreError>;
}

/// SQLite-backed store. The MVP target — zero setup, single file on disk.
#[derive(Debug)]
pub struct SqliteStore {
    conn: Connection,
}

impl SqliteStore {
    /// Open (or create) a SQLite-backed store at `path`, enabling WAL and
    /// running any pending migrations via [`MigrationRunner`].
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        (|| -> AnyResult<Self> {
            let conn = Connection::open(path)
                .with_context(|| format!("opening sqlite database at {}", path.display()))?;
            Self::configure(&conn)?;
            // Bootstrap the meta table before the runner reads the schema version.
            Self::bootstrap_meta(&conn)?;
            let mut store = Self { conn };
            sqlite_migration_runner()
                .run(&mut store)
                .context("running SQLite migrations")?;
            Ok(store)
        })()
        .map_err(|e| StoreError::Database(e.to_string()))
    }

    /// Open an in-memory SQLite store. Used in tests; WAL is not available
    /// on `:memory:` connections, so journal mode falls back to MEMORY.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        (|| -> AnyResult<Self> {
            let conn = Connection::open_in_memory().context("opening in-memory sqlite database")?;
            Self::bootstrap_meta(&conn)?;
            let mut store = Self { conn };
            sqlite_migration_runner()
                .run(&mut store)
                .context("running SQLite migrations (in-memory)")?;
            Ok(store)
        })()
        .map_err(|e| StoreError::Database(e.to_string()))
    }

    /// Create the `meta` table if it does not already exist.
    /// Must run before the migration runner, which uses meta to read the version.
    fn bootstrap_meta(conn: &Connection) -> AnyResult<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )
        .context("bootstrapping meta table")
    }

    fn configure(conn: &Connection) -> AnyResult<()> {
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("enabling WAL journal mode")?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .context("setting synchronous=NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .context("enabling foreign_keys pragma")?;
        Ok(())
    }

    /// Return the live journal mode reported by SQLite. Useful in tests.
    pub fn journal_mode(&self) -> Result<String, StoreError> {
        self.conn
            .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
            .context("querying journal_mode")
            .map_err(|e| StoreError::Database(e.to_string()))
    }

    pub fn node_count(&self) -> Result<u64, StoreError> {
        (|| -> AnyResult<u64> {
            let n: i64 = self
                .conn
                .query_row("SELECT count(*) FROM nodes", [], |row| row.get(0))
                .context("counting nodes")?;
            Ok(n as u64)
        })()
        .map_err(|e| StoreError::Database(e.to_string()))
    }

    pub fn edge_count(&self) -> Result<u64, StoreError> {
        (|| -> AnyResult<u64> {
            let n: i64 = self
                .conn
                .query_row("SELECT count(*) FROM edges", [], |row| row.get(0))
                .context("counting edges")?;
            Ok(n as u64)
        })()
        .map_err(|e| StoreError::Database(e.to_string()))
    }

    pub fn get_file_hash(&self, path: &str) -> Result<Option<String>, StoreError> {
        self.conn
            .query_row(
                "SELECT sha256 FROM files WHERE path = ?1",
                params![path],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .context("reading file hash")
            .map_err(|e| StoreError::Database(e.to_string()))
    }

    pub fn put_file_hash(&mut self, path: &str, hex: &str) -> Result<(), StoreError> {
        self.conn
            .execute(
                "INSERT INTO files(path, sha256, last_indexed_at) \
                 VALUES(?1, ?2, unixepoch()) \
                 ON CONFLICT(path) DO UPDATE SET \
                   sha256 = excluded.sha256, \
                   last_indexed_at = excluded.last_indexed_at",
                params![path, hex],
            )
            .context("writing file hash")
            .map_err(|e| StoreError::Database(e.to_string()))?;
        Ok(())
    }

    /// Delete all nodes (and their edges) whose VName path equals `path`.
    /// Edges are deleted first to avoid orphan references; both operations
    /// run inside a single transaction.  Returns the number of nodes removed.
    pub fn delete_nodes_for_path(&mut self, path: &str) -> Result<u64, StoreError> {
        (|| -> AnyResult<u64> {
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
        })()
        .map_err(|e| StoreError::Database(e.to_string()))
    }

    pub fn search_nodes_by_name(&self, name: &str) -> Result<Vec<Node>, StoreError> {
        // Log the query name (symbol/path, not file contents — SEC log-redaction rule).
        let _span = tracing::debug_span!("store.search_nodes_by_name", query = name).entered();
        (|| -> AnyResult<Vec<Node>> {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT id, corpus, root, path, language, signature, kind, package \
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
                    let package: String = row.get(7)?;
                    Ok(Node {
                        id,
                        vname,
                        kind,
                        package,
                    })
                })
                .context("executing search query")?;

            let mut out = Vec::new();
            for row in rows {
                out.push(row.context("decoding search row")?);
            }
            tracing::debug!(nodes_returned = out.len());
            Ok(out)
        })()
        .map_err(|e| StoreError::Database(e.to_string()))
    }

    pub fn all_nodes(&self) -> Result<Vec<Node>, StoreError> {
        (|| -> AnyResult<Vec<Node>> {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT id, corpus, root, path, language, signature, kind, package FROM nodes",
                )
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
                    let package: String = row.get(7)?;
                    Ok(Node {
                        id,
                        vname,
                        kind,
                        package,
                    })
                })
                .context("executing all_nodes query")?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.context("decoding all_nodes row")?);
            }
            Ok(out)
        })()
        .map_err(|e| StoreError::Database(e.to_string()))
    }

    pub fn all_edges(&self) -> Result<Vec<(NodeId, NodeId, String, String)>, StoreError> {
        (|| -> AnyResult<Vec<(NodeId, NodeId, String, String)>> {
            let mut stmt = self
                .conn
                .prepare("SELECT src, dst, kind, provenance FROM edges")
                .context("preparing all_edges query")?;
            let rows = stmt
                .query_map([], |row| {
                    let src = i64_to_node_id(row.get::<_, i64>(0)?);
                    let dst = i64_to_node_id(row.get::<_, i64>(1)?);
                    let kind: String = row.get(2)?;
                    let provenance: String = row.get(3)?;
                    Ok((src, dst, kind, provenance))
                })
                .context("executing all_edges query")?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.context("decoding all_edges row")?);
            }
            Ok(out)
        })()
        .map_err(|e| StoreError::Database(e.to_string()))
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>, StoreError> {
        self.conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![key],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .context("reading meta key")
            .map_err(|e| StoreError::Database(e.to_string()))
    }

    pub fn set_meta(&mut self, key: &str, value: &str) -> Result<(), StoreError> {
        self.conn
            .execute(
                "INSERT INTO meta(key, value) VALUES(?1, ?2) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .context("writing meta key")
            .map_err(|e| StoreError::Database(e.to_string()))?;
        Ok(())
    }

    /// Return the VName signature format version recorded in this database.
    ///
    /// Returns `0` for legacy databases (pre-RFC-002) that have no such row,
    /// or databases migrated from schema v2 where the row was just added with
    /// the default value `'0'`.
    pub fn get_signature_format_version(&self) -> Result<u8, StoreError> {
        (|| -> AnyResult<u8> {
            let raw = self
                .get_meta("signature_format_version")
                .context("reading signature_format_version")?
                .unwrap_or_else(|| "0".to_string());
            raw.parse::<u8>()
                .with_context(|| format!("invalid signature_format_version in meta: {raw}"))
        })()
        .map_err(|e| StoreError::Database(e.to_string()))
    }

    /// Write the VName signature format version. Called by the daemon after a
    /// successful full re-index to stamp the active format version.
    pub fn set_signature_format_version(&mut self, v: u8) -> Result<(), StoreError> {
        self.set_meta("signature_format_version", &v.to_string())
            .context("writing signature_format_version")
            .map_err(|e| StoreError::Database(e.to_string()))
    }

    /// Persist an edge with LSIF (semantic) provenance.
    ///
    /// LSIF always wins: if an identical (src, dst, kind) row already exists
    /// with `provenance='tree-sitter'`, it is upgraded to `'lsif'`. This
    /// implements the LSIF-beats-tree-sitter precedence policy (ADR-002).
    pub fn put_edge_lsif(&mut self, edge: &Edge) -> Result<(), StoreError> {
        self.conn
            .execute(
                "INSERT INTO edges(src, dst, kind, provenance) VALUES(?1, ?2, ?3, 'lsif')
                 ON CONFLICT(src, dst, kind) DO UPDATE SET provenance = 'lsif'",
                params![
                    node_id_to_i64(edge.src),
                    node_id_to_i64(edge.dst),
                    edge.kind.as_str(),
                ],
            )
            .context("inserting lsif edge")
            .map_err(|e| StoreError::Database(e.to_string()))?;
        Ok(())
    }
}

// ── StoreMigratable (T3) ──────────────────────────────────────────────────────

impl StoreMigratable for SqliteStore {
    fn exec_ddl(&mut self, ddl: &str) -> anyhow::Result<()> {
        self.conn
            .execute_batch(ddl)
            .context("SqliteStore::exec_ddl")
    }

    fn schema_version(&self) -> anyhow::Result<u32> {
        let raw = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .context("reading schema_version from meta")?;
        Ok(raw.and_then(|s| s.parse().ok()).unwrap_or(0))
    }

    fn set_schema_version(&mut self, v: u32) -> anyhow::Result<()> {
        self.conn
            .execute(
                "INSERT INTO meta(key, value) VALUES('schema_version', ?1) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![v.to_string()],
            )
            .context("writing schema_version to meta")?;
        Ok(())
    }

    fn column_exists(&self, table: &str, column: &str) -> anyhow::Result<bool> {
        // `pragma_table_info(table)` returns one row per column; name is col 1.
        let count: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = ?2",
                params![table, column],
                |row| row.get(0),
            )
            .context("checking column existence via pragma_table_info")?;
        Ok(count > 0)
    }
}

// ── Store ─────────────────────────────────────────────────────────────────────

impl Store for SqliteStore {
    fn put_node(&mut self, node: &Node) -> Result<NodeId, StoreError> {
        let id_i64 = node_id_to_i64(node.id);
        self.conn
            .execute(
                "INSERT INTO nodes(id, corpus, root, path, language, signature, kind, package) \
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                 ON CONFLICT(id) DO UPDATE SET kind = excluded.kind, package = excluded.package",
                params![
                    id_i64,
                    node.vname.corpus,
                    node.vname.root,
                    node.vname.path,
                    node.vname.language,
                    node.vname.signature,
                    node.kind,
                    node.package,
                ],
            )
            .context("inserting node")
            .map_err(|e| StoreError::Database(e.to_string()))?;
        Ok(node.id)
    }

    fn put_edge(&mut self, edge: &Edge) -> Result<(), StoreError> {
        // Tree-sitter edges use DO NOTHING on conflict: they must never demote
        // an existing 'lsif' row (ADR-002). DO NOTHING is equivalent to the
        // verbose CASE expression but is explicit about intent.
        self.conn
            .execute(
                "INSERT INTO edges(src, dst, kind, provenance) VALUES(?1, ?2, ?3, 'tree-sitter')
                 ON CONFLICT(src, dst, kind) DO NOTHING",
                params![
                    node_id_to_i64(edge.src),
                    node_id_to_i64(edge.dst),
                    edge.kind.as_str(),
                ],
            )
            .context("inserting tree-sitter edge")
            .map_err(|e| StoreError::Database(e.to_string()))?;
        Ok(())
    }

    fn get_node(&self, id: NodeId) -> Result<Option<Node>, StoreError> {
        (|| -> AnyResult<Option<Node>> {
            let row = self
                .conn
                .query_row(
                    "SELECT corpus, root, path, language, signature, kind, package \
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
                        let package: String = row.get(6)?;
                        Ok(Node {
                            id,
                            vname,
                            kind,
                            package,
                        })
                    },
                )
                .optional()
                .context("querying node by id")?;
            Ok(row)
        })()
        .map_err(|e| StoreError::Database(e.to_string()))
    }

    fn iter_edges_from(&self, src: NodeId) -> Result<Vec<Edge>, StoreError> {
        let _span = tracing::debug_span!("store.iter_edges_from", src = src.0).entered();
        (|| -> AnyResult<Vec<Edge>> {
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
            tracing::debug!(edges_returned = out.len());
            Ok(out)
        })()
        .map_err(|e| StoreError::Database(e.to_string()))
    }

    /// Indexed variant — uses `WHERE src = ?1 AND kind = ?2` so SQLite can
    /// satisfy the query from the `(src, dst, kind)` primary-key index without
    /// a full `src`-partition scan. Overrides the trait default.
    fn iter_edges_from_kind(&self, src: NodeId, kind: EdgeKind) -> Result<Vec<Edge>, StoreError> {
        let _span =
            tracing::debug_span!("store.iter_edges_from_kind", src = src.0, kind = ?kind).entered();
        (|| -> AnyResult<Vec<Edge>> {
            let mut stmt = self
                .conn
                .prepare("SELECT dst FROM edges WHERE src = ?1 AND kind = ?2")
                .context("preparing iter_edges_from_kind query")?;
            let rows = stmt
                .query_map(params![node_id_to_i64(src), kind.as_str()], |row| {
                    row.get::<_, i64>(0)
                })
                .context("executing iter_edges_from_kind query")?;

            let mut out = Vec::new();
            for row in rows {
                let dst_i64 = row.context("decoding iter_edges_from_kind row")?;
                out.push(Edge::new(src, i64_to_node_id(dst_i64), kind));
            }
            tracing::debug!(edges_returned = out.len());
            Ok(out)
        })()
        .map_err(|e| StoreError::Database(e.to_string()))
    }

    fn iter_edges_to(&self, dst: NodeId) -> Result<Vec<Edge>, StoreError> {
        let _span = tracing::debug_span!("store.iter_edges_to", dst = dst.0).entered();
        (|| -> AnyResult<Vec<Edge>> {
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
            tracing::debug!(edges_returned = out.len());
            Ok(out)
        })()
        .map_err(|e| StoreError::Database(e.to_string()))
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
        // Re-running the migration runner on an already-migrated store must be a no-op.
        let runner = sqlite_migration_runner();
        runner.run(&mut store).unwrap();
        runner.run(&mut store).unwrap();
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

    // Critical: an existing v1 database (no provenance column) must gain the
    // column with DEFAULT 'tree-sitter' when opened by the v2 code, and all
    // pre-existing edges must read back with provenance='tree-sitter'.
    #[test]
    fn v1_to_v2_migration_adds_provenance_with_default() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("v1.db");

        // Simulate a v1 DB: create schema manually without the provenance column.
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO meta VALUES('schema_version', '1');
                 CREATE TABLE nodes (
                   id INTEGER PRIMARY KEY, corpus TEXT NOT NULL, root TEXT NOT NULL,
                   path TEXT NOT NULL, language TEXT NOT NULL,
                   signature TEXT NOT NULL, kind TEXT NOT NULL
                 );
                 CREATE TABLE edges (
                   src INTEGER NOT NULL, dst INTEGER NOT NULL, kind TEXT NOT NULL,
                   PRIMARY KEY (src, dst, kind)
                 );
                 CREATE TABLE files (
                   path TEXT PRIMARY KEY, sha256 TEXT NOT NULL,
                   last_indexed_at INTEGER NOT NULL
                 );
                 INSERT INTO nodes VALUES(1,'','','a.ts','typescript','fn:a','function');
                 INSERT INTO nodes VALUES(2,'','','b.ts','typescript','fn:b','function');
                 INSERT INTO edges VALUES(1, 2, 'ref-call');",
            )
            .unwrap();
        }

        // Open with the v2 store — migration must succeed.
        let store = SqliteStore::open(&db_path).expect("v1→v2 migration must succeed");

        // Pre-existing edge must have provenance='tree-sitter' (the column DEFAULT).
        let edges = store.all_edges().unwrap();
        assert_eq!(edges.len(), 1, "pre-existing edge must survive migration");
        assert_eq!(
            edges[0].3, "tree-sitter",
            "migrated edge must default to tree-sitter provenance"
        );
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
    fn lsif_provenance_wins_over_tree_sitter() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let a = sample_node("fn:a");
        let b = sample_node("fn:b");
        store.put_node(&a).unwrap();
        store.put_node(&b).unwrap();
        let edge = Edge::new(a.id, b.id, EdgeKind::RefCall);

        // Tree-sitter inserts first
        store.put_edge(&edge).unwrap();
        // LSIF inserts same edge — must win
        store.put_edge_lsif(&edge).unwrap();

        assert_eq!(store.edge_count().unwrap(), 1, "must be exactly one row");
        let edges = store.all_edges().unwrap();
        assert_eq!(edges[0].3, "lsif", "provenance must be upgraded to lsif");
    }

    #[test]
    fn tree_sitter_cannot_demote_lsif_provenance() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let a = sample_node("fn:a");
        let b = sample_node("fn:b");
        store.put_node(&a).unwrap();
        store.put_node(&b).unwrap();
        let edge = Edge::new(a.id, b.id, EdgeKind::RefCall);

        // LSIF inserts first
        store.put_edge_lsif(&edge).unwrap();
        // Tree-sitter tries to insert same edge — must NOT demote
        store.put_edge(&edge).unwrap();

        let edges = store.all_edges().unwrap();
        assert_eq!(
            edges[0].3, "lsif",
            "tree-sitter must not demote lsif provenance"
        );
    }

    #[test]
    fn tree_sitter_only_edge_has_tree_sitter_provenance() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let a = sample_node("fn:a");
        let b = sample_node("fn:b");
        store.put_node(&a).unwrap();
        store.put_node(&b).unwrap();
        store
            .put_edge(&Edge::new(a.id, b.id, EdgeKind::Depends))
            .unwrap();

        let edges = store.all_edges().unwrap();
        assert_eq!(edges[0].3, "tree-sitter");
    }

    #[test]
    fn lsif_only_edge_has_lsif_provenance() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let a = sample_node("fn:a");
        let b = sample_node("fn:b");
        store.put_node(&a).unwrap();
        store.put_node(&b).unwrap();
        store
            .put_edge_lsif(&Edge::new(a.id, b.id, EdgeKind::RefCall))
            .unwrap();

        let edges = store.all_edges().unwrap();
        assert_eq!(edges[0].3, "lsif");
    }

    #[test]
    fn iter_edges_from_kind_filters_correctly() {
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

        // Only RefCall edges
        let ref_call = store.iter_edges_from_kind(a.id, EdgeKind::RefCall).unwrap();
        assert_eq!(ref_call.len(), 1);
        assert_eq!(ref_call[0].dst, b.id);
        assert_eq!(ref_call[0].kind, EdgeKind::RefCall);

        // Only Depends edges
        let depends = store.iter_edges_from_kind(a.id, EdgeKind::Depends).unwrap();
        assert_eq!(depends.len(), 1);
        assert_eq!(depends[0].dst, c.id);

        // Kind with no edges returns empty
        let exports = store.iter_edges_from_kind(a.id, EdgeKind::Exports).unwrap();
        assert!(exports.is_empty());
    }

    #[test]
    fn iter_edges_from_kind_consistent_with_iter_edges_from() {
        // The kind-filtered result must be a subset of the full result for the
        // same src, and every returned edge must have the requested kind.
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
            .put_edge(&Edge::new(a.id, c.id, EdgeKind::DefinesBinding))
            .unwrap();

        let all = store.iter_edges_from(a.id).unwrap();
        let filtered = store.iter_edges_from_kind(a.id, EdgeKind::RefCall).unwrap();
        assert!(filtered.len() <= all.len());
        assert!(filtered.iter().all(|e| e.kind == EdgeKind::RefCall));
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

    // V4 (no package column) → V5 migration must add package with empty default,
    // add language column to edges, and existing nodes must read back correctly.
    #[test]
    fn v4_to_v5_migration_adds_package_column() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("v4.db");

        // Simulate a v4 DB: schema version 4, nodes table without package column.
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO meta VALUES('schema_version', '4');
                 CREATE TABLE nodes (
                   id INTEGER PRIMARY KEY,
                   corpus TEXT NOT NULL, root TEXT NOT NULL, path TEXT NOT NULL,
                   language TEXT NOT NULL, signature TEXT NOT NULL, kind TEXT NOT NULL
                 );
                 CREATE TABLE edges (
                   src INTEGER NOT NULL, dst INTEGER NOT NULL, kind TEXT NOT NULL,
                   provenance TEXT NOT NULL DEFAULT 'tree-sitter',
                   PRIMARY KEY (src, dst, kind)
                 );
                 CREATE TABLE files (
                   path TEXT PRIMARY KEY, sha256 TEXT NOT NULL,
                   last_indexed_at INTEGER NOT NULL
                 );
                 INSERT INTO nodes VALUES(10,'','','src/main.rs','rust','fn:main','function');",
            )
            .unwrap();
        }

        // Open with the v5 store — migration must add package + edges.language.
        let store = SqliteStore::open(&db_path).expect("v4→v5 migration must succeed");

        // Pre-existing node must read back with package = '' (the column default).
        let nodes = store.all_nodes().unwrap();
        assert_eq!(nodes.len(), 1, "pre-existing node must survive migration");
        assert_eq!(
            nodes[0].package, "",
            "migrated node must default to empty package"
        );

        // Column must exist and be queryable.
        let has_pkg = store.column_exists("nodes", "package").unwrap();
        assert!(
            has_pkg,
            "nodes.package column must exist after v5 migration"
        );
        let has_lang = store.column_exists("edges", "language").unwrap();
        assert!(
            has_lang,
            "edges.language column must exist after v5 migration"
        );
    }

    // package field round-trips through put_node / get_node.
    #[test]
    fn node_package_round_trips() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let n = Node::new(
            VName::new(
                "github.com/a/b",
                "",
                "crates/foo/src/lib.rs",
                "rust",
                "fn:open",
            ),
            "function",
        )
        .with_package("foo-crate");
        let id = store.put_node(&n).unwrap();
        let back = store.get_node(id).unwrap().expect("node must exist");
        assert_eq!(back.package, "foo-crate");
    }

    // search_nodes_by_name returns package field.
    #[test]
    fn search_nodes_by_name_returns_package() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let n = Node::new(
            VName::new(
                "github.com/a/b",
                "",
                "crates/bar/src/lib.rs",
                "rust",
                "fn:bar",
            ),
            "function",
        )
        .with_package("bar-crate");
        store.put_node(&n).unwrap();
        let results = store.search_nodes_by_name("fn:bar").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].package, "bar-crate");
    }
}
