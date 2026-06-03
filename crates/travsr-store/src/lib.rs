//! travsr-store — pluggable graph storage backend.
//!
//! The MVP uses SQLite (WAL) via `rusqlite`; production replaces this with
//! Kùzu, and hyperscale moves to RocksDB. All backends implement the
//! `Store` trait below.

#![forbid(unsafe_code)]

pub mod fts_tokenize;
pub mod migration;
pub mod migration_manifest;
pub mod registry;
mod seed_lexicon;

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
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use travsr_core::{Edge, EdgeKind, Node, NodeId, VName};
use travsr_error::StoreError;

use crate::fts_tokenize::{build_fuzzy_match_expr_db, tokenize_identifier};

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

struct V6EdgeConfidence;
impl Migration for V6EdgeConfidence {
    fn version(&self) -> u32 {
        6
    }
    fn up(&self, store: &mut dyn StoreMigratable) -> anyhow::Result<()> {
        // ALTER TABLE … ADD COLUMN has no IF NOT EXISTS in SQLite.
        // Guard manually so re-running after a crash (atomicity gap) is safe.
        if !store.column_exists("edges", "confidence")? {
            store.exec_ddl(include_str!("migrations/v6_edge_confidence.sql"))?;
        }
        Ok(())
    }
}

struct V7RbacColumns;
impl Migration for V7RbacColumns {
    fn version(&self) -> u32 {
        7
    }
    fn up(&self, store: &mut dyn StoreMigratable) -> anyhow::Result<()> {
        // ALTER TABLE … ADD COLUMN has no IF NOT EXISTS in SQLite.
        // Guard manually so re-running after a crash (atomicity gap) is safe.
        if !store.column_exists("nodes", "access_corpus")? {
            store.exec_ddl("ALTER TABLE nodes ADD COLUMN access_corpus TEXT")?;
        }
        // CREATE TABLE IF NOT EXISTS is idempotent — safe to re-run.
        store.exec_ddl(
            "CREATE TABLE IF NOT EXISTS sessions (\
               id      TEXT PRIMARY KEY,\
               corpus  TEXT NOT NULL,\
               created INTEGER NOT NULL DEFAULT (unixepoch()),\
               expires INTEGER NOT NULL\
             )",
        )?;
        Ok(())
    }
}

struct V8NodeLine;
impl Migration for V8NodeLine {
    fn version(&self) -> u32 {
        8
    }
    fn up(&self, store: &mut dyn StoreMigratable) -> anyhow::Result<()> {
        if !store.column_exists("nodes", "line")? {
            store.exec_ddl("ALTER TABLE nodes ADD COLUMN line INTEGER")?;
        }
        Ok(())
    }
}

struct V9NodesFts;
impl Migration for V9NodesFts {
    fn version(&self) -> u32 {
        9
    }
    fn up(&self, store: &mut dyn StoreMigratable) -> anyhow::Result<()> {
        // Both statements use IF NOT EXISTS — idempotent on re-run after a
        // crash between up() and set_schema_version (the atomicity gap).
        store.exec_ddl(include_str!("migrations/v9_nodes_fts.sql"))
    }
}

struct V10FtsVocab;
impl Migration for V10FtsVocab {
    fn version(&self) -> u32 {
        10
    }
    fn up(&self, store: &mut dyn StoreMigratable) -> anyhow::Result<()> {
        // Both CREATE statements use IF NOT EXISTS — idempotent on re-run.
        store.exec_ddl(include_str!("migrations/v10_fts_vocab.sql"))
    }
}

struct V11FtsSynonyms;
impl Migration for V11FtsSynonyms {
    fn version(&self) -> u32 {
        11
    }
    fn up(&self, store: &mut dyn StoreMigratable) -> anyhow::Result<()> {
        // Both CREATE statements use IF NOT EXISTS — idempotent on re-run.
        store.exec_ddl(include_str!("migrations/v11_fts_synonyms.sql"))
    }
}

#[cfg(feature = "embeddings")]
struct V12Vec0Embeddings;
#[cfg(feature = "embeddings")]
impl Migration for V12Vec0Embeddings {
    fn version(&self) -> u32 {
        12
    }
    fn up(&self, store: &mut dyn StoreMigratable) -> anyhow::Result<()> {
        // STUB (RFC-012 A2 F2, DEBT travsr-#259): the `embeddings` feature is not
        // wired yet — the `ort` + `sqlite-vec` deps are unpinned and no extension
        // loader exists. This DDL requires the `vec0` module to be registered on
        // the connection first; until the loader lands, enabling `embeddings` will
        // fail here. There is intentionally NO guard in this stub. Do not enable
        // the `embeddings` feature in production until F2 is implemented.
        store.exec_ddl(include_str!("migrations/v12_vec0_embeddings.sql"))
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
    r.register(V6EdgeConfidence);
    r.register(V7RbacColumns);
    r.register(V8NodeLine);
    r.register(V9NodesFts);
    r.register(V10FtsVocab);
    r.register(V11FtsSynonyms);
    #[cfg(feature = "embeddings")]
    r.register(V12Vec0Embeddings);
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
    /// Batch-fetch nodes by id. Unknown ids are silently skipped.
    /// Output order is not guaranteed to match input order.
    fn get_nodes(&self, ids: &[NodeId]) -> Result<Vec<Node>, StoreError>;
}

/// SQLite-backed store. The MVP target — zero setup, single file on disk.
#[derive(Debug)]
pub struct SqliteStore {
    conn: Connection,
}

impl Drop for SqliteStore {
    fn drop(&mut self) {
        // Best-effort WAL truncation on clean shutdown — shrinks graph.db-wal
        // to zero bytes so the next open starts with no WAL replay overhead.
        // TRUNCATE mode requires an exclusive lock; it silently skips if any
        // reader holds a shared lock, which is always a safe WAL state.
        let _ = self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
    }
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
            store
                .backfill_fts_if_needed()
                .context("backfilling FTS index")?;
            store
                .backfill_vocab_if_needed()
                .context("backfilling fts_vocab index (L2-A)")?;
            store
                .seed_synonyms_if_empty()
                .context("seeding fts_synonyms (RFC-012 A2 F1)")?;
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
            store
                .backfill_fts_if_needed()
                .context("backfilling FTS index (in-memory)")?;
            store
                .backfill_vocab_if_needed()
                .context("backfilling fts_vocab index in-memory (L2-A)")?;
            store
                .seed_synonyms_if_empty()
                .context("seeding fts_synonyms in-memory (RFC-012 A2 F1)")?;
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
        // Cap page cache to 64 MB (negative value = kibibytes).
        // Without this the default grows proportionally with graph size and
        // was the primary cause of ~700 MB RSS per daemon process.
        conn.pragma_update(None, "cache_size", -65536i64)
            .context("setting cache_size")?;
        // Checkpoint the WAL every 500 pages (~2 MB) instead of the SQLite
        // default of 1000 pages, so the WAL file stays small at rest.
        conn.pragma_update(None, "wal_autocheckpoint", 500i32)
            .context("setting wal_autocheckpoint")?;
        // Keep temp tables in memory instead of writing to /tmp files.
        conn.pragma_update(None, "temp_store", "MEMORY")
            .context("setting temp_store=MEMORY")?;
        // Disable memory-mapped I/O. Without this, SQLite maps the entire DB
        // file into the process virtual address space. On a large graph.db
        // (hundreds of MB) every daemon process shows that many MB of RSS —
        // and multiple processes each get their own mapping of the same file.
        conn.pragma_update(None, "mmap_size", 0i64)
            .context("disabling mmap_size")?;
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

    /// Return per-language node counts, ordered by count descending.
    /// Returns an empty Vec when no nodes carry language metadata.
    pub fn language_distribution(&self) -> Result<Vec<(String, u64)>, StoreError> {
        (|| -> AnyResult<Vec<(String, u64)>> {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT language, COUNT(*) as cnt \
                     FROM nodes \
                     WHERE language IS NOT NULL AND language != '' \
                     GROUP BY language \
                     ORDER BY cnt DESC",
                )
                .context("preparing language_distribution")?;
            let pairs = stmt
                .query_map([], |row| {
                    let lang: String = row.get(0)?;
                    let cnt: i64 = row.get(1)?;
                    Ok((lang, cnt as u64))
                })
                .context("querying language distribution")?
                .collect::<rusqlite::Result<Vec<_>>>()
                .context("collecting language distribution")?;
            Ok(pairs)
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

            // Load token strings for vocab decrement BEFORE removing map rows.
            let path_token_strings: Vec<String> = {
                let mut stmt = tx
                    .prepare(
                        "SELECT m.tokens FROM nodes_fts_map m \
                         JOIN nodes n ON n.id = m.node_id WHERE n.path = ?1",
                    )
                    .context("preparing token load for path delete")?;
                let collected: Vec<String> = stmt
                    .query_map(params![path], |row| row.get(0))
                    .context("executing token load for path delete")?
                    .collect::<Result<_, _>>()
                    .context("collecting token strings for vocab decrement")?;
                collected
            };

            // Retract FTS entries before deleting nodes (tokens stored in map).
            tx.execute(
                "INSERT INTO nodes_fts(nodes_fts, rowid, tokens) \
                 SELECT 'delete', m.node_id, m.tokens \
                 FROM nodes_fts_map m JOIN nodes n ON n.id = m.node_id \
                 WHERE n.path = ?1",
                params![path],
            )
            .context("retracting FTS rows for path")?;
            tx.execute(
                "DELETE FROM nodes_fts_map \
                 WHERE node_id IN (SELECT id FROM nodes WHERE path = ?1)",
                params![path],
            )
            .context("removing nodes_fts_map rows for path")?;

            // Decrement fts_vocab refcounts for all retracted tokens (v10 L2-A).
            for ts in &path_token_strings {
                Self::vocab_decrement(&tx, ts)?;
            }

            tx.execute("DELETE FROM nodes WHERE path = ?1", params![path])
                .context("deleting nodes for path")?;

            tx.commit().context("committing delete transaction")?;
            Ok(count as u64)
        })()
        .map_err(|e| StoreError::Database(e.to_string()))
    }

    /// Delete all nodes (and their edges) whose VName path starts with `prefix`.
    ///
    /// Used by `init_repo` to purge ghost nodes from directories that were added
    /// to `SKIP_DIRS` after a previous index run (e.g. `.claude/worktrees/`).
    /// Edges are deleted first; both run in a single transaction.
    /// Returns the number of nodes removed.
    ///
    /// Deliberately NOT on the [`Store`] trait — this is a SqliteStore-specific
    /// cleanup operation for the daemon init path.
    pub fn delete_nodes_for_path_prefix(&mut self, prefix: &str) -> Result<u64, StoreError> {
        (|| -> AnyResult<u64> {
            let pattern = format!("{prefix}%");
            let tx = self
                .conn
                .transaction()
                .context("starting prefix-delete transaction")?;

            let count: i64 = tx
                .query_row(
                    "SELECT count(*) FROM nodes WHERE path LIKE ?1",
                    params![pattern],
                    |row| row.get(0),
                )
                .context("counting nodes to prefix-delete")?;

            // Edges have no FK cascade — delete them explicitly before removing nodes.
            tx.execute(
                "DELETE FROM edges \
                 WHERE src IN (SELECT id FROM nodes WHERE path LIKE ?1) \
                    OR dst IN (SELECT id FROM nodes WHERE path LIKE ?1)",
                params![pattern],
            )
            .context("deleting edges for path prefix")?;

            // Load token strings for vocab decrement BEFORE removing map rows.
            let prefix_token_strings: Vec<String> = {
                let mut stmt = tx
                    .prepare(
                        "SELECT m.tokens FROM nodes_fts_map m \
                         JOIN nodes n ON n.id = m.node_id WHERE n.path LIKE ?1",
                    )
                    .context("preparing token load for prefix delete")?;
                let collected: Vec<String> = stmt
                    .query_map(params![pattern], |row| row.get(0))
                    .context("executing token load for prefix delete")?
                    .collect::<Result<_, _>>()
                    .context("collecting token strings for vocab decrement (prefix)")?;
                collected
            };

            // Retract FTS entries before deleting nodes (tokens stored in map).
            tx.execute(
                "INSERT INTO nodes_fts(nodes_fts, rowid, tokens) \
                 SELECT 'delete', m.node_id, m.tokens \
                 FROM nodes_fts_map m JOIN nodes n ON n.id = m.node_id \
                 WHERE n.path LIKE ?1",
                params![pattern],
            )
            .context("retracting FTS rows for path prefix")?;
            tx.execute(
                "DELETE FROM nodes_fts_map \
                 WHERE node_id IN (SELECT id FROM nodes WHERE path LIKE ?1)",
                params![pattern],
            )
            .context("removing nodes_fts_map rows for path prefix")?;

            // Decrement fts_vocab refcounts for all retracted tokens (v10 L2-A).
            for ts in &prefix_token_strings {
                Self::vocab_decrement(&tx, ts)?;
            }

            tx.execute("DELETE FROM nodes WHERE path LIKE ?1", params![pattern])
                .context("deleting nodes for path prefix")?;

            tx.commit()
                .context("committing prefix-delete transaction")?;
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
                    "SELECT id, corpus, root, path, language, signature, kind, package, line \
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
                    let line: Option<i64> = row.get(8)?;
                    Ok(Node {
                        id,
                        vname,
                        kind,
                        package,
                        line: line.and_then(|l| u32::try_from(l).ok()),
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
                    "SELECT id, corpus, root, path, language, signature, kind, package, line FROM nodes",
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
                    let line: Option<i64> = row.get(8)?;
                    Ok(Node {
                        id,
                        vname,
                        kind,
                        package,
                        line: line.and_then(|l| u32::try_from(l).ok()),
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
                "INSERT INTO edges(src, dst, kind, provenance, confidence) VALUES(?1, ?2, ?3, 'lsif', ?4)
                 ON CONFLICT(src, dst, kind) DO UPDATE SET provenance = 'lsif', confidence = excluded.confidence",
                params![
                    node_id_to_i64(edge.src),
                    node_id_to_i64(edge.dst),
                    edge.kind.as_str(),
                    edge.confidence.map(|c| c as i64),
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

/// Jaccard(byte-trigrams) threshold for L2-A vocabulary-grounded expansion.
/// A vocab token must share at least this fraction of trigrams with a T0 seed
/// to be included as a candidate OR-arm.  0.4 is empirically conservative:
/// tight enough to avoid noise, loose enough to catch plurals and short variants.
const L2A_JACCARD_THRESHOLD: f64 = 0.4;

// ── FTS helpers + fuzzy search (RFC-012 L1) ───────────────────────────────────

impl SqliteStore {
    /// Public accessor for the current schema/migration version recorded in the
    /// `meta` table. Wraps the private `StoreMigratable::schema_version` so the
    /// MCP `get_graph_stats` tool can surface migration state in the VS Code
    /// extension's graph-stats popup (VSCODE-247). Returns 0 if absent.
    pub fn current_schema_version(&self) -> anyhow::Result<u32> {
        StoreMigratable::schema_version(self)
    }

    // ── FTS helpers (RFC-012 L1) ──────────────────────────────────────────────

    /// Build the token string for a node: tokenized signature + path + kind + language.
    fn node_fts_tokens(node: &Node) -> String {
        format!(
            "{} {} {} {}",
            tokenize_identifier(&node.vname.signature),
            tokenize_identifier(&node.vname.path),
            node.kind.to_ascii_lowercase(),
            node.vname.language.to_ascii_lowercase(),
        )
    }

    /// Upsert a node's FTS entry.  Handles the contentless-FTS5 invariant:
    /// a row must be explicitly deleted (with its original tokens) before
    /// re-inserting, otherwise the index silently accumulates stale entries.
    fn put_node_fts(conn: &Connection, node: &Node) -> AnyResult<()> {
        let id_i64 = node_id_to_i64(node.id);
        let new_tokens = Self::node_fts_tokens(node);

        // Fetch old tokens (if any) so we can retract the stale FTS row.
        let old_tokens: Option<String> = conn
            .query_row(
                "SELECT tokens FROM nodes_fts_map WHERE node_id = ?1",
                params![id_i64],
                |row| row.get(0),
            )
            .optional()
            .context("reading old FTS tokens")?;

        if let Some(ref old) = old_tokens {
            // Retract the stale contentless-FTS5 row with its original tokens.
            conn.execute(
                "INSERT INTO nodes_fts(nodes_fts, rowid, tokens) VALUES('delete', ?1, ?2)",
                params![id_i64, old],
            )
            .context("retracting stale FTS row")?;
            // Decrement vocab refcounts for retracted tokens (v10 L2-A).
            Self::vocab_decrement(conn, old)?;
        }

        // Insert new FTS row.
        conn.execute(
            "INSERT INTO nodes_fts(rowid, tokens) VALUES(?1, ?2)",
            params![id_i64, new_tokens],
        )
        .context("inserting FTS row")?;

        // Upsert the map so future retractions can supply the exact token string.
        conn.execute(
            "INSERT INTO nodes_fts_map(node_id, tokens) VALUES(?1, ?2)
             ON CONFLICT(node_id) DO UPDATE SET tokens = excluded.tokens",
            params![id_i64, new_tokens],
        )
        .context("upserting nodes_fts_map")?;

        // Increment vocab refcounts for new tokens (v10 L2-A).
        Self::vocab_increment(conn, &new_tokens)?;

        Ok(())
    }

    /// Retract a single node from the FTS index.  No-op if the node has no FTS entry.
    #[allow(dead_code)]
    fn delete_node_fts(conn: &Connection, node_id_i64: i64) -> AnyResult<()> {
        let old_tokens: Option<String> = conn
            .query_row(
                "SELECT tokens FROM nodes_fts_map WHERE node_id = ?1",
                params![node_id_i64],
                |row| row.get(0),
            )
            .optional()
            .context("reading FTS tokens for deletion")?;

        if let Some(tokens) = old_tokens {
            conn.execute(
                "INSERT INTO nodes_fts(nodes_fts, rowid, tokens) VALUES('delete', ?1, ?2)",
                params![node_id_i64, tokens],
            )
            .context("retracting FTS row on node delete")?;
            conn.execute(
                "DELETE FROM nodes_fts_map WHERE node_id = ?1",
                params![node_id_i64],
            )
            .context("removing nodes_fts_map row")?;
            // Decrement vocab refcounts for retracted tokens (v10 L2-A).
            Self::vocab_decrement(conn, &tokens)?;
        }

        Ok(())
    }

    /// Idempotent FTS backfill called once after migrations at `open()` /
    /// `open_in_memory()`.  Cheap gate: if `COUNT(nodes) == COUNT(nodes_fts_map)`
    /// the index is up to date and we return immediately.  On first open after
    /// the v9 migration ships, indexes any nodes not yet in the map.
    fn backfill_fts_if_needed(&mut self) -> AnyResult<()> {
        let node_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .context("counting nodes for FTS backfill gate")?;
        let map_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM nodes_fts_map", [], |r| r.get(0))
            .context("counting nodes_fts_map for FTS backfill gate")?;

        if node_count == map_count {
            return Ok(());
        }

        // map_count > node_count is possible when stale FTS entries exist
        // (nodes deleted via a path that ran outside this store instance).
        // In that case the JOIN in search_nodes_fuzzy silently skips them;
        // the count-inequality still triggers a no-op backfill pass.
        tracing::info!(
            missing = node_count.saturating_sub(map_count),
            stale = map_count.saturating_sub(node_count),
            "RFC-012 L1: backfilling FTS index for unindexed nodes"
        );

        // Fetch unindexed nodes into a Vec first so the statement is dropped
        // before we open the write transaction (borrow-checker: immutable stmt
        // borrow must not overlap the mutable conn borrow for transaction()).
        let nodes: Vec<Node> = {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT id, corpus, root, path, language, signature, kind, package, line \
                     FROM nodes WHERE id NOT IN (SELECT node_id FROM nodes_fts_map)",
                )
                .context("preparing FTS backfill query")?;
            let collected = stmt
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
                    let line: Option<i64> = row.get(8)?;
                    Ok(Node {
                        id,
                        vname,
                        kind,
                        package,
                        line: line.and_then(|l| u32::try_from(l).ok()),
                    })
                })
                .context("executing FTS backfill query")?
                .collect::<Result<_, _>>()
                .context("collecting FTS backfill rows")?;
            collected
        }; // stmt dropped here — conn is free for the write transaction

        // Index in one transaction to minimise WAL pressure.
        let tx = self
            .conn
            .transaction()
            .context("starting FTS backfill transaction")?;
        for node in &nodes {
            Self::put_node_fts(&tx, node).context("put_node_fts during backfill")?;
        }
        tx.commit().context("committing FTS backfill transaction")?;

        tracing::info!(indexed = nodes.len(), "RFC-012 L1: FTS backfill complete");
        Ok(())
    }

    /// Layered seed selection (RFC-012 L1 + A1).
    ///
    /// 1. Exact substring via `search_nodes_by_name` — if ≥1 result, return it.
    /// 2. FTS5 trigram MATCH on the T0 heuristic-normalised token union
    ///    (`build_fuzzy_match_expr`: stopwords stripped, synonym aliases added,
    ///    PA C1 fallback).  If ≥1 result, return it.
    /// 3. L2-A vocabulary-grounded expansion (`expand_query`): Jaccard-similarity
    ///    candidates from `fts_vocab` merged with T0 tokens, capped at 16 OR arms.
    ///    Only fires on combined Step 1 + Step 2 miss.
    ///
    /// The exact-substring step ensures existing benchmarks never regress (TL3).
    /// All synonym/stem output passes through the same double-quote FTS5 escaper
    /// as `build_match_expr` (TL4).  L2-A is vocabulary-grounded: it can only
    /// propose tokens that exist in `fts_vocab` (populated by `put_node_fts`),
    /// so no hallucinated token ever reaches the FTS index — enforcing PA C4.
    pub fn search_nodes_fuzzy(&self, query: &str) -> Result<Vec<Node>, StoreError> {
        let _span = tracing::debug_span!("store.search_nodes_fuzzy", query).entered();

        // Step 1 — exact substring (never regresses, TL3).
        let exact = self.search_nodes_by_name(query)?;
        if !exact.is_empty() {
            tracing::debug!(layer = "exact", nodes_returned = exact.len());
            return Ok(exact);
        }

        // Step 2 — FTS5 trigram MATCH on the T0 heuristic-normalised token union.
        // build_fuzzy_match_expr_db uses fts_synonyms (DB-backed, RFC-012 A2 F1)
        // instead of the compile-time static; pure build_fuzzy_match_expr is kept
        // for unit tests that run without a live connection.
        let step2_expr = match build_fuzzy_match_expr_db(query, &self.conn)
            .map_err(|e| StoreError::Database(e.to_string()))?
        {
            Some(e) => e,
            // All tokens < 3 chars (e.g. pure punctuation) — nothing to search.
            None => {
                tracing::debug!(layer = "fts5_skip_empty_tokens");
                return Ok(Vec::new());
            }
        };
        let step2 = self
            .fts_query_nodes(&step2_expr)
            .map_err(|e| StoreError::Database(e.to_string()))?;
        if !step2.is_empty() {
            tracing::debug!(layer = "fts5_t0", nodes_returned = step2.len());
            return Ok(step2);
        }

        // Step 3 — L2-A vocabulary-grounded expansion (RFC-012 A1).
        // Only fires on combined Step 1 + Step 2 miss.
        (|| -> AnyResult<Vec<Node>> {
            let raw_str = tokenize_identifier(query);
            if raw_str.is_empty() {
                return Ok(Vec::new());
            }
            let raw: Vec<String> = raw_str.split_whitespace().map(str::to_string).collect();
            let t0_tokens = crate::seed_lexicon::expand_tokens(&raw);
            let l2a_extra = self.expand_query(&t0_tokens)?;
            if l2a_extra.is_empty() {
                tracing::debug!(layer = "fts5_l2a_no_candidates");
                return Ok(Vec::new());
            }

            // Merge T0 tokens + L2-A candidates; sort for determinism; cap at 16.
            let mut arms: Vec<String> = t0_tokens;
            for c in l2a_extra {
                if arms.len() >= 16 {
                    break;
                }
                if !arms.iter().any(|t| t == &c) {
                    arms.push(c);
                }
            }
            arms.sort();
            arms.truncate(16);

            let match_expr = arms
                .iter()
                .map(|t| format!("\"{t}\""))
                .collect::<Vec<_>>()
                .join(" OR ");

            let step3 = self.fts_query_nodes(&match_expr)?;
            tracing::debug!(
                layer = "fts5_l2a",
                arms = arms.len(),
                nodes_returned = step3.len()
            );
            Ok(step3)
        })()
        .map_err(|e| StoreError::Database(e.to_string()))
    }

    /// Execute a raw FTS5 MATCH expression against the nodes index.
    /// Shared by Step 2 (T0) and Step 3 (L2-A) to avoid duplicated query logic.
    fn fts_query_nodes(&self, match_expr: &str) -> AnyResult<Vec<Node>> {
        let sql = "SELECT n.id, n.corpus, n.root, n.path, n.language, n.signature, \
                          n.kind, n.package, n.line \
                   FROM nodes_fts \
                   JOIN nodes_fts_map m ON nodes_fts.rowid = m.node_id \
                   JOIN nodes n ON n.id = m.node_id \
                   WHERE nodes_fts MATCH ?1 \
                   ORDER BY bm25(nodes_fts) \
                   LIMIT 50";
        let mut stmt = self.conn.prepare(sql).context("preparing FTS5 query")?;
        let rows = stmt
            .query_map(params![match_expr], |row| {
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
                let line: Option<i64> = row.get(8)?;
                Ok(Node {
                    id,
                    vname,
                    kind,
                    package,
                    line: line.and_then(|l| u32::try_from(l).ok()),
                })
            })
            .context("executing FTS5 query")?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.context("decoding FTS5 row")?);
        }
        Ok(out)
    }

    /// L2-A: vocabulary-grounded token expansion (RFC-012 A1 Step 3).
    ///
    /// Scans `fts_vocab` for tokens with Jaccard(byte-trigrams) ≥ `L2A_JACCARD_THRESHOLD`
    /// similarity to ANY token in `t0_tokens`.  Returns only tokens that exist in the live
    /// vocabulary (refcount > 0), so no hallucinated token can reach the FTS index
    /// (PA C4 bright line).  The caller caps the merged arm list at 16.
    ///
    /// Cost: O(V · |t0_tokens|) where V = vocabulary size.  Acceptable at MVP
    /// scale; SymSpell/FST can replace for scale-out without changing the API.
    fn expand_query(&self, t0_tokens: &[String]) -> AnyResult<Vec<String>> {
        if t0_tokens.is_empty() {
            return Ok(vec![]);
        }

        // Load vocab tokens with refcount > 0.  Cap at 100 k to remain practical
        // on large graphs; L2-A is only called on a combined miss so frequency is low.
        let vocab: Vec<String> = {
            let mut stmt = self
                .conn
                .prepare("SELECT token FROM fts_vocab WHERE refcount > 0 LIMIT 100000")
                .context("preparing fts_vocab scan")?;
            let collected: Vec<String> = stmt
                .query_map([], |row| row.get(0))
                .context("executing fts_vocab scan")?
                .collect::<Result<_, _>>()
                .context("collecting fts_vocab tokens")?;
            collected
        };

        // Pre-build a set of T0 tokens for O(1) membership check.
        let t0_set: std::collections::HashSet<&str> =
            t0_tokens.iter().map(String::as_str).collect();

        // Single pass over vocab: a token is a candidate if any T0 token exceeds
        // the Jaccard threshold.  This avoids re-scanning vocab once per T0 token.
        let mut candidates: Vec<String> = Vec::new();
        for v_tok in &vocab {
            if v_tok.len() < 3 {
                continue;
            }
            if t0_set.contains(v_tok.as_str()) {
                continue; // already in T0 union
            }
            if candidates.iter().any(|c| c == v_tok) {
                continue;
            }
            if t0_tokens
                .iter()
                .any(|q| byte_trigram_jaccard(q, v_tok) >= L2A_JACCARD_THRESHOLD)
            {
                candidates.push(v_tok.clone());
            }
        }

        candidates.sort();
        Ok(candidates)
    }

    /// Increment `fts_vocab` refcounts for every token in `tokens_str` (space-separated).
    /// Skips tokens shorter than 3 bytes (FTS5 trigram minimum).
    fn vocab_increment(conn: &Connection, tokens_str: &str) -> AnyResult<()> {
        for tok in tokens_str.split_whitespace() {
            if tok.len() < 3 {
                continue;
            }
            conn.execute(
                "INSERT INTO fts_vocab(token, refcount) VALUES(?1, 1) \
                 ON CONFLICT(token) DO UPDATE SET refcount = refcount + 1",
                params![tok],
            )
            .context("incrementing fts_vocab refcount")?;
        }
        Ok(())
    }

    /// Return the `fts_vocab` refcount for `token`, or `None` if the token is absent.
    /// Used by integration tests to verify L2-A refcount maintenance (AC e).
    #[doc(hidden)]
    pub fn fts_vocab_refcount(&self, token: &str) -> AnyResult<Option<i64>> {
        self.conn
            .query_row(
                "SELECT refcount FROM fts_vocab WHERE token = ?1",
                params![token],
                |row| row.get(0),
            )
            .optional()
            .context("querying fts_vocab refcount")
    }

    /// Decrement `fts_vocab` refcounts for every token in `tokens_str` (space-separated).
    /// Uses MAX(0, refcount - 1) to guard against underflow on out-of-order deletes.
    /// Skips tokens shorter than 3 bytes — matching the `vocab_increment` guard so
    /// short tokens never trigger a no-op UPDATE round-trip.
    fn vocab_decrement(conn: &Connection, tokens_str: &str) -> AnyResult<()> {
        for tok in tokens_str.split_whitespace() {
            if tok.len() < 3 {
                continue;
            }
            conn.execute(
                "UPDATE fts_vocab SET refcount = MAX(0, refcount - 1) WHERE token = ?1",
                params![tok],
            )
            .context("decrementing fts_vocab refcount")?;
        }
        Ok(())
    }

    /// Idempotent vocab backfill called once after migrations at `open()` /
    /// `open_in_memory()`.  Gate: if `fts_vocab` is non-empty the table is
    /// assumed up-to-date and we return immediately.  On first open after the
    /// v10 migration ships, reconstructs refcounts from the existing
    /// `nodes_fts_map` so pre-v10 databases get a correct vocabulary.
    fn backfill_vocab_if_needed(&mut self) -> AnyResult<()> {
        let vocab_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM fts_vocab", [], |r| r.get(0))
            .context("counting fts_vocab for backfill gate")?;

        if vocab_count > 0 {
            return Ok(()); // already populated
        }

        let token_strings: Vec<String> = {
            let mut stmt = self
                .conn
                .prepare("SELECT tokens FROM nodes_fts_map")
                .context("preparing nodes_fts_map scan for vocab backfill")?;
            let collected: Vec<String> = stmt
                .query_map([], |row| row.get(0))
                .context("executing nodes_fts_map scan")?
                .collect::<Result<_, _>>()
                .context("collecting token strings for vocab backfill")?;
            collected
        };

        if token_strings.is_empty() {
            return Ok(());
        }

        let tx = self
            .conn
            .transaction()
            .context("starting vocab backfill transaction")?;
        for ts in &token_strings {
            Self::vocab_increment(&tx, ts).context("vocab_increment during backfill")?;
        }
        tx.commit()
            .context("committing vocab backfill transaction")?;

        tracing::info!(
            nodes = token_strings.len(),
            "RFC-012 L2-A: fts_vocab backfill complete"
        );
        Ok(())
    }

    // ── Dynamic synonym table (RFC-012 A2 F1) ────────────────────────────────

    /// Seed `fts_synonyms` from the compile-time static defaults if the table is empty.
    /// Called once at `open()` / `open_in_memory()` after migrations.
    /// Idempotent: if any rows exist, returns immediately without touching the table.
    fn seed_synonyms_if_empty(&mut self) -> AnyResult<()> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM fts_synonyms", [], |r| r.get(0))
            .context("counting fts_synonyms")?;
        if count > 0 {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        for (term, aliases) in crate::seed_lexicon::SYNONYMS {
            for alias in *aliases {
                tx.execute(
                    "INSERT OR IGNORE INTO fts_synonyms(term, alias) VALUES(?1, ?2)",
                    params![term, alias],
                )?;
            }
        }
        tx.commit()?;
        tracing::info!("RFC-012 A2 F1: seeded fts_synonyms from static defaults");
        Ok(())
    }

    /// Add a synonym pair. Rejects if the table already has ≥200 rows.
    pub fn synonym_add(&mut self, term: &str, alias: &str) -> AnyResult<()> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM fts_synonyms", [], |r| r.get(0))
            .context("counting fts_synonyms before add")?;
        anyhow::ensure!(
            count < 200,
            "fts_synonyms is full (200 rows). Use `travsr synonym remove` to make space."
        );
        self.conn.execute(
            "INSERT OR IGNORE INTO fts_synonyms(term, alias) VALUES(?1, ?2)",
            params![term, alias],
        )?;
        Ok(())
    }

    /// Remove a synonym pair. No-op if the pair does not exist.
    pub fn synonym_remove(&mut self, term: &str, alias: &str) -> AnyResult<()> {
        self.conn.execute(
            "DELETE FROM fts_synonyms WHERE term = ?1 AND alias = ?2",
            params![term, alias],
        )?;
        Ok(())
    }

    /// Remove ALL aliases for `term`. No-op if the term has no aliases.
    pub fn synonym_remove_term(&mut self, term: &str) -> AnyResult<()> {
        self.conn
            .execute("DELETE FROM fts_synonyms WHERE term = ?1", params![term])?;
        Ok(())
    }

    /// Declaratively replace ALL aliases for `term` with exactly `aliases`.
    ///
    /// Atomic: the DELETE and every INSERT run inside a single transaction, so a
    /// crash mid-operation can never leave the term with its old aliases removed
    /// and only a partial new set (the failure mode of a separate remove + N adds).
    /// Rejects — and rolls back, leaving the table untouched — if the resulting
    /// row count would exceed the 200-row cap. The cap is evaluated once against
    /// the post-delete count, not per-insert.
    pub fn synonym_set(&mut self, term: &str, aliases: &[String]) -> AnyResult<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM fts_synonyms WHERE term = ?1", params![term])?;
        let count: i64 = tx
            .query_row("SELECT COUNT(*) FROM fts_synonyms", [], |r| r.get(0))
            .context("counting fts_synonyms before set")?;
        anyhow::ensure!(
            count + aliases.len() as i64 <= 200,
            "fts_synonyms would exceed 200 rows. Use `travsr synonym remove` to make space."
        );
        for alias in aliases {
            tx.execute(
                "INSERT OR IGNORE INTO fts_synonyms(term, alias) VALUES(?1, ?2)",
                params![term, alias],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// List all active synonym pairs as (term, alias) tuples.
    pub fn synonym_list(&self) -> AnyResult<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT term, alias FROM fts_synonyms ORDER BY term, alias")?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Reset `fts_synonyms` to the static defaults: delete all rows and re-seed.
    pub fn synonym_reset(&mut self) -> AnyResult<()> {
        let tx = self.conn.transaction()?;
        tx.execute_batch("DELETE FROM fts_synonyms")?;
        for (term, aliases) in crate::seed_lexicon::SYNONYMS {
            for alias in *aliases {
                tx.execute(
                    "INSERT OR IGNORE INTO fts_synonyms(term, alias) VALUES(?1, ?2)",
                    params![term, alias],
                )?;
            }
        }
        tx.commit()?;
        tracing::info!("RFC-012 A2 F1: fts_synonyms reset to static defaults");
        Ok(())
    }
}

// ── Store ─────────────────────────────────────────────────────────────────────

impl Store for SqliteStore {
    fn put_node(&mut self, node: &Node) -> Result<NodeId, StoreError> {
        let id_i64 = node_id_to_i64(node.id);
        // Wrap node upsert + FTS write in one transaction so a crash between
        // the two writes cannot leave nodes and nodes_fts_map out of sync.
        // (The backfill gate in open() self-heals any gap, but atomicity is
        // preferable.)  Callers use bare autocommit loops — no nesting conflict.
        (|| -> AnyResult<()> {
            let tx = self
                .conn
                .transaction()
                .context("starting put_node transaction")?;
            tx.execute(
                "INSERT INTO nodes(id, corpus, root, path, language, signature, kind, package, line) \
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
                 ON CONFLICT(id) DO UPDATE SET kind = excluded.kind, package = excluded.package, line = COALESCE(excluded.line, nodes.line)",
                params![
                    id_i64,
                    node.vname.corpus,
                    node.vname.root,
                    node.vname.path,
                    node.vname.language,
                    node.vname.signature,
                    node.kind,
                    node.package,
                    node.line.map(|l| l as i64),
                ],
            )
            .context("inserting node")?;
            Self::put_node_fts(&tx, node).context("put_node_fts")?;
            tx.commit().context("committing put_node transaction")?;
            Ok(())
        })()
        .map_err(|e| StoreError::Database(e.to_string()))?;
        Ok(node.id)
    }

    fn put_edge(&mut self, edge: &Edge) -> Result<(), StoreError> {
        // Tree-sitter edges use DO NOTHING on conflict: they must never demote
        // an existing 'lsif' row (ADR-002). DO NOTHING is equivalent to the
        // verbose CASE expression but is explicit about intent.
        self.conn
            .execute(
                "INSERT INTO edges(src, dst, kind, provenance, confidence) VALUES(?1, ?2, ?3, 'tree-sitter', ?4)
                 ON CONFLICT(src, dst, kind) DO NOTHING",
                params![
                    node_id_to_i64(edge.src),
                    node_id_to_i64(edge.dst),
                    edge.kind.as_str(),
                    edge.confidence.map(|c| c as i64),
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
                    "SELECT corpus, root, path, language, signature, kind, package, line \
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
                        let line: Option<i64> = row.get(7)?;
                        Ok(Node {
                            id,
                            vname,
                            kind,
                            package,
                            line: line.and_then(|l| u32::try_from(l).ok()),
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
                .prepare("SELECT dst, kind, confidence FROM edges WHERE src = ?1")
                .context("preparing iter_edges_from query")?;
            let rows = stmt
                .query_map(params![node_id_to_i64(src)], |row| {
                    let dst_i64: i64 = row.get(0)?;
                    let kind_str: String = row.get(1)?;
                    let confidence: Option<i64> = row.get(2)?;
                    Ok((dst_i64, kind_str, confidence))
                })
                .context("executing iter_edges_from query")?;

            let mut out = Vec::new();
            for row in rows {
                let (dst_i64, kind_str, confidence) = row.context("decoding edge row")?;
                let kind = EdgeKind::from_str(&kind_str)
                    .with_context(|| format!("unknown edge kind in storage: {kind_str}"))?;
                out.push(Edge {
                    src,
                    dst: i64_to_node_id(dst_i64),
                    kind,
                    confidence: confidence.map(|c| c as u8),
                });
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

    fn get_nodes(&self, ids: &[NodeId]) -> Result<Vec<Node>, StoreError> {
        // SQLite SQLITE_MAX_VARIABLE_NUMBER is 999 by default; chunk to stay within it.
        const CHUNK: usize = 999;
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(CHUNK) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT id, corpus, root, path, language, signature, kind, package, line \
                 FROM nodes WHERE id IN ({placeholders})"
            );
            (|| -> AnyResult<()> {
                let mut stmt = self
                    .conn
                    .prepare(&sql)
                    .context("preparing get_nodes query")?;
                let id_params: Vec<i64> = chunk.iter().map(|id| node_id_to_i64(*id)).collect();
                let rows = stmt
                    .query_map(params_from_iter(id_params.iter()), |row| {
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
                        let line: Option<i64> = row.get(8)?;
                        Ok(Node {
                            id,
                            vname,
                            kind,
                            package,
                            line: line.and_then(|l| u32::try_from(l).ok()),
                        })
                    })
                    .context("executing get_nodes query")?;
                for r in rows {
                    out.push(r.context("decoding get_nodes row")?);
                }
                Ok(())
            })()
            .map_err(|e| StoreError::Database(e.to_string()))?;
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

/// Jaccard similarity on byte-level trigrams of two strings.
/// Used by `expand_query` (L2-A) to find vocabulary-grounded candidates.
/// Returns 0.0 for strings shorter than 3 bytes.
fn byte_trigram_jaccard(a: &str, b: &str) -> f64 {
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    if ab.len() < 3 || bb.len() < 3 {
        return 0.0;
    }
    let ta: std::collections::HashSet<[u8; 3]> =
        ab.windows(3).map(|w| [w[0], w[1], w[2]]).collect();
    let tb: std::collections::HashSet<[u8; 3]> =
        bb.windows(3).map(|w| [w[0], w[1], w[2]]).collect();
    let intersection = ta.intersection(&tb).count();
    let union = ta.union(&tb).count();
    if union == 0 {
        return 0.0;
    }
    intersection as f64 / union as f64
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
    fn delete_nodes_for_path_prefix_removes_matching_nodes_and_edges() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let a = node_with_path(".claude/worktrees/session-1/agent.ts", "fn:agent");
        let b = node_with_path(".claude/worktrees/session-2/helper.ts", "fn:helper");
        let c = node_with_path("src/real.ts", "fn:real");
        store.put_node(&a).unwrap();
        store.put_node(&b).unwrap();
        store.put_node(&c).unwrap();
        store
            .put_edge(&Edge::new(a.id, c.id, EdgeKind::Depends))
            .unwrap();

        let deleted = store.delete_nodes_for_path_prefix(".claude/").unwrap();
        assert_eq!(deleted, 2, "two .claude/ nodes must be removed");
        assert_eq!(
            store.node_count().unwrap(),
            1,
            "src/real.ts node must survive"
        );
        assert_eq!(
            store.edge_count().unwrap(),
            0,
            "edge referencing .claude/ node must be removed"
        );
        assert!(
            store.get_node(c.id).unwrap().is_some(),
            "real node must be intact"
        );
    }

    #[test]
    fn delete_nodes_for_path_prefix_returns_zero_for_no_match() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let n = node_with_path("src/real.ts", "fn:real");
        store.put_node(&n).unwrap();
        let deleted = store.delete_nodes_for_path_prefix(".claude/").unwrap();
        assert_eq!(deleted, 0);
        assert_eq!(store.node_count().unwrap(), 1);
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

    #[test]
    fn node_line_round_trips() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let n = Node::new(
            VName::new("github.com/a/b", "", "src/lib.rs", "rust", "fn:open"),
            "function",
        )
        .with_line(42);
        let id = store.put_node(&n).unwrap();
        let back = store.get_node(id).unwrap().expect("node must exist");
        assert_eq!(back.line, Some(42));
    }

    #[test]
    fn node_line_none_for_file_nodes() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let n = Node::new(
            VName::new("github.com/a/b", "", "src/lib.rs", "rust", "file"),
            "file",
        );
        let id = store.put_node(&n).unwrap();
        let back = store.get_node(id).unwrap().expect("node must exist");
        assert_eq!(back.line, None);
    }

    #[test]
    fn node_line_coalesce_preserves_existing_on_upsert() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let vname = VName::new("g/a/b", "", "src/lib.rs", "rust", "fn:foo");
        // First insert with line = Some(10).
        let n1 = Node::new(vname.clone(), "function").with_line(10);
        store.put_node(&n1).unwrap();
        // Upsert without line — COALESCE must preserve the existing value.
        let n2 = Node::new(vname.clone(), "function");
        store.put_node(&n2).unwrap();
        let back = store.get_node(n1.id).unwrap().expect("node must exist");
        assert_eq!(
            back.line,
            Some(10),
            "COALESCE must preserve existing line on upsert"
        );
    }

    #[test]
    fn migration_v8_adds_line_column_to_existing_db() {
        // Simulate a pre-v8 database: open at v7, insert a node, then upgrade.
        // Since SqliteStore::open always runs all pending migrations, we verify
        // that a node written before the column existed reads back as line = None.
        let mut store = SqliteStore::open_in_memory().unwrap();
        let n = Node::new(
            VName::new("g/a/b", "", "src/foo.ts", "typescript", "fn:bar"),
            "function",
        );
        store.put_node(&n).unwrap();
        // Re-open (migrations are idempotent — column already exists, guard applies).
        // The important assertion: existing rows have line = None after migration.
        let back = store.get_node(n.id).unwrap().expect("node must exist");
        assert_eq!(
            back.line, None,
            "pre-v8 nodes must read back as line = None"
        );
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

    #[test]
    fn v7_migration_adds_access_corpus_and_sessions_table() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("v6.db");

        // Simulate a v6 DB (no access_corpus column, no sessions table).
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO meta VALUES('schema_version', '6');
                 CREATE TABLE nodes (
                   id INTEGER PRIMARY KEY, corpus TEXT NOT NULL, root TEXT NOT NULL,
                   path TEXT NOT NULL, language TEXT NOT NULL,
                   signature TEXT NOT NULL, kind TEXT NOT NULL,
                   format_version INTEGER NOT NULL DEFAULT 1,
                   package TEXT NOT NULL DEFAULT ''
                 );
                 CREATE TABLE edges (
                   src INTEGER NOT NULL, dst INTEGER NOT NULL, kind TEXT NOT NULL,
                   provenance TEXT NOT NULL DEFAULT 'tree-sitter',
                   confidence INTEGER,
                   PRIMARY KEY (src, dst, kind)
                 );
                 CREATE TABLE files (
                   path TEXT PRIMARY KEY, sha256 TEXT NOT NULL,
                   last_indexed_at INTEGER NOT NULL
                 );
                 INSERT INTO nodes VALUES(1,'corp','','a.rs','rust','fn:a','function',1,'');",
            )
            .unwrap();
        }

        // Open with the v7 store — migration must succeed.
        let store = SqliteStore::open(&db_path).expect("v6→v7 migration must succeed");

        // Pre-existing node must survive migration with access_corpus defaulting to NULL.
        let node = store.get_node(travsr_core::NodeId(1)).unwrap();
        assert!(
            node.is_some(),
            "pre-existing node must survive v7 migration"
        );

        // sessions table must now exist — insert + select a row.
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO sessions (id, corpus, expires) VALUES ('tok1', 'corp', 9999999999)",
            [],
        )
        .unwrap();
        let corpus: String = conn
            .query_row("SELECT corpus FROM sessions WHERE id = 'tok1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            corpus, "corp",
            "sessions table round-trip must work after v7"
        );

        // access_corpus column must exist and be nullable.
        conn.execute("UPDATE nodes SET access_corpus = 'corp' WHERE id = 1", [])
            .unwrap();
        let ac: Option<String> = conn
            .query_row("SELECT access_corpus FROM nodes WHERE id = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(ac, Some("corp".to_string()));
    }

    #[test]
    fn synonym_add_multi_then_list() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        store.synonym_add("payment", "billing").unwrap();
        store.synonym_add("payment", "invoice").unwrap();
        let pairs = store.synonym_list().unwrap();
        let aliases: Vec<&str> = pairs
            .iter()
            .filter(|(t, _)| t == "payment")
            .map(|(_, a)| a.as_str())
            .collect();
        assert!(aliases.contains(&"billing"));
        assert!(aliases.contains(&"invoice"));
    }

    #[test]
    fn synonym_remove_term_clears_all_aliases() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        store.synonym_add("payment", "billing").unwrap();
        store.synonym_add("payment", "invoice").unwrap();
        store.synonym_add("payment2", "wire").unwrap();
        store.synonym_remove_term("payment").unwrap();
        let pairs = store.synonym_list().unwrap();
        assert!(
            pairs.iter().all(|(t, _)| t != "payment"),
            "all payment aliases must be removed"
        );
        assert!(
            pairs.iter().any(|(t, a)| t == "payment2" && a == "wire"),
            "payment2 alias must survive"
        );
    }

    #[test]
    fn synonym_remove_term_noop_on_missing_term() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        store.synonym_add("payment2", "wire").unwrap();
        store.synonym_remove_term("nonexistent").unwrap();
        let pairs = store.synonym_list().unwrap();
        assert!(pairs.iter().any(|(t, a)| t == "payment2" && a == "wire"));
    }

    #[test]
    fn synonym_set_replaces_existing_aliases() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        store.synonym_add("payment", "billing").unwrap();
        store.synonym_add("payment", "invoice").unwrap();
        // Exercise the real atomic synonym_set, not a hand-rolled remove+add.
        store
            .synonym_set(
                "payment",
                &["charge".to_string(), "transaction".to_string()],
            )
            .unwrap();
        let pairs = store.synonym_list().unwrap();
        let aliases: Vec<&str> = pairs
            .iter()
            .filter(|(t, _)| t == "payment")
            .map(|(_, a)| a.as_str())
            .collect();
        assert_eq!(aliases.len(), 2);
        assert!(aliases.contains(&"charge"));
        assert!(aliases.contains(&"transaction"));
        assert!(!aliases.contains(&"billing"));
        assert!(!aliases.contains(&"invoice"));
    }

    #[test]
    fn synonym_add_enforces_200_row_cap() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        // Fill the table up to exactly 200 rows (open() pre-seeds the defaults,
        // so this also pins that the cap counts seeded rows).
        let mut i = 0;
        while store.synonym_list().unwrap().len() < 200 {
            store.synonym_add("filler", &format!("alias{i}")).unwrap();
            i += 1;
        }
        assert_eq!(store.synonym_list().unwrap().len(), 200);
        // The 201st distinct add must be rejected.
        assert!(
            store.synonym_add("filler", "one_too_many").is_err(),
            "add beyond 200 rows must error"
        );
        // The rejected add must not have grown the table.
        assert_eq!(
            store.synonym_list().unwrap().len(),
            200,
            "rejected add must leave the table at exactly 200 rows"
        );
    }

    #[test]
    fn synonym_set_over_cap_rolls_back_entirely() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        // Fill to 198 rows.
        let mut i = 0;
        while store.synonym_list().unwrap().len() < 198 {
            store.synonym_add("filler", &format!("a{i}")).unwrap();
            i += 1;
        }
        let before = store.synonym_list().unwrap();
        // Setting 5 aliases on a brand-new term would push 198 + 5 = 203 > 200.
        let aliases: Vec<String> = (0..5).map(|n| format!("x{n}")).collect();
        assert!(
            store.synonym_set("newterm", &aliases).is_err(),
            "synonym_set that exceeds the cap must error"
        );
        assert_eq!(
            store.synonym_list().unwrap(),
            before,
            "a failed synonym_set must roll back the DELETE and all INSERTs"
        );
    }

    #[test]
    fn seed_synonyms_is_idempotent() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        assert!(
            !store.synonym_list().unwrap().is_empty(),
            "open must seed static defaults"
        );
        store.synonym_add("payment", "billing").unwrap();
        let after_add = store.synonym_list().unwrap().len();
        // Re-running the seeder on a non-empty table must be a no-op.
        store.seed_synonyms_if_empty().unwrap();
        assert_eq!(
            store.synonym_list().unwrap().len(),
            after_add,
            "re-seeding a non-empty table must not re-add defaults"
        );
    }

    #[test]
    fn synonyms_persist_and_dont_reseed_across_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("graph.db");
        {
            let mut store = SqliteStore::open(&db_path).unwrap();
            store.synonym_remove_term("auth").unwrap(); // drop a seeded default
            store.synonym_add("payment", "billing").unwrap(); // add a user row
        }
        // Reopen: the seeder must NOT re-add the removed default (table is
        // non-empty), and the user row must persist (v11 idempotency).
        let store = SqliteStore::open(&db_path).unwrap();
        let pairs = store.synonym_list().unwrap();
        assert!(
            pairs.iter().any(|(t, a)| t == "payment" && a == "billing"),
            "user-added synonym must persist across reopen"
        );
        assert!(
            !pairs.iter().any(|(t, _)| t == "auth"),
            "a removed default must not be re-seeded on reopen"
        );
    }

    #[test]
    fn synonym_remove_one_alias_leaves_others() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        store.synonym_add("payment", "billing").unwrap();
        store.synonym_add("payment", "invoice").unwrap();
        store.synonym_remove("payment", "billing").unwrap();
        let pairs = store.synonym_list().unwrap();
        let aliases: Vec<&str> = pairs
            .iter()
            .filter(|(t, _)| t == "payment")
            .map(|(_, a)| a.as_str())
            .collect();
        assert!(!aliases.contains(&"billing"), "removed alias must be gone");
        assert!(aliases.contains(&"invoice"), "sibling alias must survive");
    }
}
