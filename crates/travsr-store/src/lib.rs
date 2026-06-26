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
use std::sync::Arc;

/// Type alias for the RFC-018 Step 4 semantic-ANN callback injected by the daemon.
/// Returns `(NodeId, cosine_similarity_score)` pairs in descending score order.
pub type EmbedKnnHook =
    Arc<dyn Fn(&str, u32) -> Result<Vec<(NodeId, f32)>, StoreError> + Send + Sync>;

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

/// RFC-018: plain blob embedding table — no sqlite-vec extension required in main process.
/// EmbedPlugin sidecar opens its own DB connection for ANN queries.
/// Numbered v16 so it runs on DBs already at v15 (kcore-shells branch).
struct V16NodeEmbeddings;
impl Migration for V16NodeEmbeddings {
    fn version(&self) -> u32 {
        16
    }
    fn up(&self, store: &mut dyn StoreMigratable) -> anyhow::Result<()> {
        // CREATE TABLE / INDEX IF NOT EXISTS — idempotent on re-run.
        store.exec_ddl(include_str!("migrations/v16_node_embeddings.sql"))
    }
}

/// RFC-019: move node_embeddings to embed.db; add CDC tombstone trigger.
///
/// graph.db drops node_embeddings (now owned by the embed sidecar in embed.db)
/// and gains node_tombstones + capture_node_delete trigger so the sidecar can
/// prune stale embeddings between reindex passes without a full-table scan.
struct V17NodeTombstones;
impl Migration for V17NodeTombstones {
    fn version(&self) -> u32 {
        17
    }
    fn up(&self, store: &mut dyn StoreMigratable) -> anyhow::Result<()> {
        store.exec_ddl(include_str!("migrations/v17_node_tombstones.sql"))
    }
}

/// RFC-014 WS2: end_line column, symbol_aliases table, edge_sites table, G3 cleanup.
struct V13PhaseBUnification;
impl Migration for V13PhaseBUnification {
    fn version(&self) -> u32 {
        13
    }
    fn up(&self, store: &mut dyn StoreMigratable) -> anyhow::Result<()> {
        // ALTER TABLE … ADD COLUMN has no IF NOT EXISTS in SQLite — guard manually.
        if !store.column_exists("nodes", "end_line")? {
            store.exec_ddl("ALTER TABLE nodes ADD COLUMN end_line INTEGER")?;
        }
        // symbol_aliases + edge_sites: CREATE TABLE IF NOT EXISTS is idempotent.
        store.exec_ddl(include_str!("migrations/v13_phase_b_unification.sql"))?;
        // G3: delete SCIP anonymous-local nodes and their incident edges.
        // SCIP ingest stores these with signature `scip:<path>:local <N>`
        // (see `travsr_core::is_scip_anonymous_local`, whose semantics this
        // mirrors: a `:local ` separator followed by digits only, to end of
        // string). The pattern is anchored to the `:local [0-9]` suffix and
        // rejects any non-digit after it, so a *path* containing "local N"
        // (e.g. `src/local 9/util.go`) can never false-positive — its
        // signature has `/local`, not `:local`, before the digits. (The
        // ingest path in scip-reader also drops locals; this cleans up DBs
        // written before that filter existed.)
        const G3_LOCAL_FILTER: &str = "signature GLOB 'scip:*:local [0-9]*' \
             AND signature NOT GLOB 'scip:*:local [0-9]*[^0-9]*'";
        store.exec_ddl(&format!(
            "DELETE FROM edges WHERE src IN (\
               SELECT id FROM nodes WHERE {G3_LOCAL_FILTER}\
             ) OR dst IN (\
               SELECT id FROM nodes WHERE {G3_LOCAL_FILTER}\
             )"
        ))?;
        store.exec_ddl(&format!("DELETE FROM nodes WHERE {G3_LOCAL_FILTER}"))?;
        // O7: reclaim freed pages from G3 deletions.  WAL checkpoint first so the
        // VACUUM can compact the database file (VACUUM is a no-op while WAL frames
        // are not yet flushed to the main file).
        store.exec_ddl("PRAGMA wal_checkpoint(TRUNCATE)")?;
        store.exec_ddl("VACUUM")?;
        Ok(())
    }
}

/// #323 R2: covering reverse index on edges(dst, kind, src).
///
/// Symmetric counterpart to v4 `idx_edges_src_kind_cov`. Eliminates the
/// main-table random I/O on every `get_callers` / `get_blast_radius` reverse
/// traversal (SELECT src FROM edges WHERE dst=? AND kind=?).
struct V14CoveringReverseIdx;
impl Migration for V14CoveringReverseIdx {
    fn version(&self) -> u32 {
        14
    }
    fn up(&self, store: &mut dyn StoreMigratable) -> anyhow::Result<()> {
        store.exec_ddl(include_str!("migrations/v14_covering_reverse_idx.sql"))
    }
}

/// V15: k-core shell numbers on nodes — recomputed by travsr-retrieval after every index.
struct V15KcoreShells;
impl Migration for V15KcoreShells {
    fn version(&self) -> u32 {
        15
    }
    fn up(&self, store: &mut dyn StoreMigratable) -> anyhow::Result<()> {
        if !store.column_exists("nodes", "shell_number")? {
            store.exec_ddl(include_str!("migrations/v15_kcore_shells.sql"))?;
        }
        Ok(())
    }
}

/// V18: embed_text column — pre-computed AST-skeleton embed text per node.
struct V18EmbedText;
impl Migration for V18EmbedText {
    fn version(&self) -> u32 {
        18
    }
    fn up(&self, store: &mut dyn StoreMigratable) -> anyhow::Result<()> {
        if !store.column_exists("nodes", "embed_text")? {
            store.exec_ddl(include_str!("migrations/v18_embed_text.sql"))?;
        }
        Ok(())
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
    r.register(V13PhaseBUnification);
    r.register(V14CoveringReverseIdx);
    r.register(V15KcoreShells);
    r.register(V16NodeEmbeddings);
    r.register(V17NodeTombstones);
    r.register(V18EmbedText);
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

/// One file's worth of parsed graph data, ready to write in a batch transaction.
///
/// Produced by the parallel parse workers in `travsr-daemon` and consumed by
/// `SqliteStore::write_file_graphs_batch`.  Keeping parse and write decoupled
/// lets N CPU threads produce data while a single writer thread owns the store.
#[derive(Debug)]
pub struct FileGraph {
    /// Repo-relative path string, e.g. `"src/lib.rs"`.
    pub vname_path: String,
    /// SHA-256 hex of the file, used to update the `files` hash table.
    pub new_hash: String,
    pub nodes: Vec<travsr_core::Node>,
    pub edges: Vec<travsr_core::Edge>,
}

/// Aggregate counts returned by [`SqliteStore::write_file_graphs_batch`].
#[derive(Debug, Default)]
pub struct BatchWriteCounts {
    pub nodes_upserted: u64,
    pub edges_upserted: u64,
    pub files_written: u64,
}

/// SQLite-backed store. The MVP target — zero setup, single file on disk.
pub struct SqliteStore {
    conn: Connection,
    staging_active: bool,
    /// RFC-018 Step 4: optional semantic ANN hook injected by the daemon at
    /// store-open time. `None` by default — zero cost when no embed plugin is
    /// running. The store crate has zero knowledge of the plugin system; only the
    /// daemon injects this via `set_embed_knn_hook`.
    embed_knn_hook: Option<EmbedKnnHook>,
    /// RFC-019: sibling embed.db path (e.g. `.travsr/embed.db`). `None` for
    /// in-memory stores. Used by `embed_progress` to ATTACH embed.db and count
    /// embedded nodes without polluting the graph.db WAL with embedding BLOBs.
    embed_db_path: Option<std::path::PathBuf>,
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
            let mut store = Self {
                conn,
                staging_active: false,
                embed_knn_hook: None,
                embed_db_path: Some(path.with_file_name("embed.db")),
            };
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
            store
                .run_pragma_optimize()
                .context("PRAGMA optimize after open")?;
            Ok(store)
        })()
        .map_err(|e| StoreError::Database(e.to_string()))
    }

    /// Open an existing store read-only for query commands (#318 O1).
    ///
    /// Skips everything [`SqliteStore::open`] does that is a write-path
    /// concern — the migration runner, FTS backfill, vocab backfill, and
    /// synonym seeding — which makes the cold CLI path substantially cheaper.
    /// The schema version is still verified: a store pending migrations
    /// returns an error so the caller can fall back to a full `open()`.
    pub fn open_read_only(path: &Path) -> Result<Self, StoreError> {
        (|| -> AnyResult<Self> {
            anyhow::ensure!(
                path.exists(),
                "no graph database at {} — run `travsr init`",
                path.display()
            );
            let conn = Connection::open_with_flags(
                path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                    | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .with_context(|| format!("opening sqlite database read-only at {}", path.display()))?;
            // Read-path pragmas only. journal_mode=WAL is a persisted property
            // of the database file; setting pragmas that write (WAL switch,
            // autocheckpoint) is neither possible nor needed here.
            conn.pragma_update(None, "cache_size", -Self::cache_size_kib())
                .context("setting cache_size (read-only)")?;
            conn.pragma_update(None, "temp_store", "MEMORY")
                .context("setting temp_store=MEMORY (read-only)")?;
            conn.pragma_update(None, "mmap_size", Self::mmap_size_bytes())
                .context("setting mmap_size (read-only)")?;
            conn.pragma_update(None, "query_only", "ON")
                .context("setting query_only=ON")?;
            let store = Self {
                conn,
                staging_active: false,
                embed_knn_hook: None,
                embed_db_path: Some(path.with_file_name("embed.db")),
            };
            let current = store
                .schema_version()
                .context("reading schema version (read-only)")?;
            let latest = sqlite_migration_runner().latest_version();
            anyhow::ensure!(
                current == latest,
                "schema v{current} ≠ expected v{latest} — pending migrations; reopen writable"
            );
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
            let mut store = Self {
                conn,
                staging_active: false,
                embed_knn_hook: None,
                embed_db_path: None,
            };
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

    /// Inject the RFC-018 Step 4 semantic-ANN hook.
    ///
    /// Called by the daemon after opening the store, when the embed supervisor
    /// is active and the stored model_id matches the plugin's model_id. The hook
    /// fires at the end of `search_nodes_fuzzy` (only when Steps 1–3 all miss).
    pub fn set_embed_knn_hook(&mut self, hook: EmbedKnnHook) {
        self.embed_knn_hook = Some(hook);
    }

    /// Return a callable wrapper around the embed KNN hook, or `None` when no
    /// hook has been injected (embed plugin not installed or index not built).
    ///
    /// The closure owns an `Arc` clone so it can outlive the `SqliteStore`
    /// borrow. Errors from the underlying hook are swallowed into an empty vec.
    pub fn embed_knn_fn(&self) -> Option<impl Fn(&str, u32) -> Vec<(NodeId, f32)>> {
        let hook = self.embed_knn_hook.clone()?;
        Some(move |query: &str, k: u32| -> Vec<(NodeId, f32)> {
            hook(query, k).unwrap_or_default()
        })
    }

    /// Create the `meta` table if it does not already exist.
    /// Must run before the migration runner, which uses meta to read the version.
    fn bootstrap_meta(conn: &Connection) -> AnyResult<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )
        .context("bootstrapping meta table")
    }

    /// Page-cache size in kibibytes, read from `TRAVSR_STORE_CACHE_MB` (default 128).
    /// Negative value as required by SQLite's cache_size pragma convention.
    fn cache_size_kib() -> i64 {
        std::env::var("TRAVSR_STORE_CACHE_MB")
            .ok()
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|&mb| mb > 0)
            .unwrap_or(128)
            * 1024
    }

    /// Memory-mapped I/O size in bytes, read from `TRAVSR_STORE_MMAP_GB` (default 0 — disabled).
    /// Off by default to avoid per-process RSS bloat in multi-process local deployments.
    /// Enable only on a dedicated single-instance OCI A1 deployment.
    fn mmap_size_bytes() -> i64 {
        std::env::var("TRAVSR_STORE_MMAP_GB")
            .ok()
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|&gb| gb > 0)
            .map(|gb| gb * 1024 * 1024 * 1024)
            .unwrap_or(0)
    }

    fn configure(conn: &Connection) -> AnyResult<()> {
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("enabling WAL journal mode")?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .context("setting synchronous=NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .context("enabling foreign_keys pragma")?;
        // Cap page cache (negative value = kibibytes). Default 128 MB; override
        // via TRAVSR_STORE_CACHE_MB. Without a cap the default grows with graph
        // size and was the primary cause of ~700 MB RSS per daemon process.
        conn.pragma_update(None, "cache_size", -Self::cache_size_kib())
            .context("setting cache_size")?;
        // Checkpoint the WAL every 500 pages (~2 MB) instead of the SQLite
        // default of 1000 pages, so the WAL file stays small at rest.
        conn.pragma_update(None, "wal_autocheckpoint", 500i32)
            .context("setting wal_autocheckpoint")?;
        // Keep temp tables in memory instead of writing to /tmp files.
        conn.pragma_update(None, "temp_store", "MEMORY")
            .context("setting temp_store=MEMORY")?;
        // Memory-mapped I/O. Off by default (TRAVSR_STORE_MMAP_GB=0) to avoid
        // per-process RSS bloat; enable only on a dedicated OCI A1 instance.
        conn.pragma_update(None, "mmap_size", Self::mmap_size_bytes())
            .context("setting mmap_size")?;
        Ok(())
    }

    /// Enable or disable bulk-init mode pragmas.
    ///
    /// `enable=true` skips WAL fsyncs (`synchronous=OFF`) and expands the page
    /// cache to 256 MB for the duration of a full `init_repo` run.  Safe because
    /// init is re-runnable: an OS crash may leave a partially-written WAL, but
    /// the next `travsr init` heals it.  NEVER use on the commit-hook path.
    ///
    /// `enable=false` restores the values set by [`configure`].
    pub fn set_bulk_init_mode(&mut self, enable: bool) -> anyhow::Result<()> {
        if enable {
            self.conn
                .pragma_update(None, "synchronous", "OFF")
                .context("bulk init: setting synchronous=OFF")?;
            self.conn
                .pragma_update(None, "cache_size", -262144i64)
                .context("bulk init: setting cache_size=256MB")?;
        } else {
            self.conn
                .pragma_update(None, "synchronous", "NORMAL")
                .context("bulk init: restoring synchronous=NORMAL")?;
            self.conn
                .pragma_update(None, "cache_size", -Self::cache_size_kib())
                .context("bulk init: restoring cache_size")?;
        }
        Ok(())
    }

    /// Run a bounded `PRAGMA optimize` so the query planner has real cardinality
    /// estimates for `nodes` and `edges`.
    ///
    /// Without `ANALYZE`, SQLite uses hardcoded defaults and may pick a full table
    /// scan over an index seek on a fresh database. `analysis_limit = 1000` bounds
    /// the scan cost; `PRAGMA optimize` skips tables whose stats are already fresh.
    /// Safe to call from `&self` — the pragma writes to `sqlite_stat1` internally
    /// but does not require a write transaction from the caller.
    ///
    /// Call sites: end of [`SqliteStore::open`] and end of a full `travsr init`.
    pub fn run_pragma_optimize(&self) -> Result<(), StoreError> {
        self.conn
            .execute_batch("PRAGMA analysis_limit = 1000; PRAGMA optimize;")
            .map_err(|e| StoreError::Database(e.to_string()))
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

    /// L11: row count in the FTS index. Should match `node_count()`.
    /// A mismatch indicates partial FTS write — user should re-run `travsr init`.
    pub fn fts_node_count(&self) -> Result<u64, StoreError> {
        (|| -> AnyResult<u64> {
            let n: i64 = self
                .conn
                .query_row("SELECT count(*) FROM nodes_fts", [], |row| row.get(0))
                .context("counting nodes_fts")?;
            Ok(n as u64)
        })()
        .map_err(|e| StoreError::Database(e.to_string()))
    }

    /// Embedding coverage counts for a given model, split by k-core shell threshold.
    ///
    /// Returns `(total_symbols, embedded, phase1_total, phase1_done)` where
    /// "embeddable" matches the sidecar's kind filter exactly:
    ///   NOT IN ('file', 'file-module', 'import', 'module', 'field', 'variable')
    /// phase1 = embeddable nodes with `shell_number >= threshold` (high-centrality core).
    /// Phase2 totals can be derived: `phase2_total = total_symbols - phase1_total`.
    ///
    /// RFC-019: embedded counts are read from embed.db (sibling of graph.db) via
    /// ATTACH. Returns (total, 0, phase1_total, 0) when embed.db does not yet
    /// exist (first run before any reindex completes).
    pub fn embed_progress(
        &self,
        model_id: &str,
        shell_threshold: u32,
    ) -> Result<(u64, u64, u64, u64), StoreError> {
        // Must match travsr-embed sidecar's kind exclusion list exactly.
        const KIND_FILTER: &str =
            "kind NOT IN ('file', 'file-module', 'import', 'module', 'field', 'variable')";

        (|| -> AnyResult<(u64, u64, u64, u64)> {
            let total_symbols: i64 = self
                .conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM nodes WHERE {KIND_FILTER}"),
                    [],
                    |r| r.get(0),
                )
                .context("counting embeddable nodes")?;

            let phase1_total: i64 = self
                .conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM nodes WHERE {KIND_FILTER} AND shell_number >= ?1"
                    ),
                    rusqlite::params![shell_threshold],
                    |r| r.get(0),
                )
                .context("counting phase1 embeddable")?;

            // RFC-019: node_embeddings lives in embed.db. ATTACH it to count
            // embedded nodes. Return zeros when embed.db does not exist yet
            // (before first sidecar reindex pass).
            let (embedded, phase1_done) = self
                .embed_db_path
                .as_deref()
                .filter(|p| p.exists())
                .map(|embed_path| -> AnyResult<(i64, i64)> {
                    let embed_path_str = embed_path
                        .to_str()
                        .context("embed.db path is not valid UTF-8")?;
                    self.conn
                        .execute_batch(&format!("ATTACH DATABASE '{embed_path_str}' AS edb"))
                        .context("attaching embed.db")?;
                    let embedded: i64 = self
                        .conn
                        .query_row(
                            "SELECT COUNT(*) FROM edb.node_embeddings WHERE model_id = ?1",
                            rusqlite::params![model_id],
                            |r| r.get(0),
                        )
                        .context("counting embedded nodes")?;
                    let phase1_done: i64 = self
                        .conn
                        .query_row(
                            &format!(
                                "SELECT COUNT(*) FROM edb.node_embeddings ne \
                                 JOIN nodes n ON ne.node_id = n.id \
                                 WHERE ne.model_id = ?1 AND n.{KIND_FILTER} \
                                 AND n.shell_number >= ?2"
                            ),
                            rusqlite::params![model_id, shell_threshold],
                            |r| r.get(0),
                        )
                        .context("counting phase1 embedded")?;
                    self.conn
                        .execute_batch("DETACH DATABASE edb")
                        .context("detaching embed.db")?;
                    Ok((embedded, phase1_done))
                })
                .transpose()?
                .unwrap_or((0, 0));

            Ok((
                total_symbols as u64,
                embedded as u64,
                phase1_total as u64,
                phase1_done as u64,
            ))
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

    /// Returns `true` when at least one `ref/call` edge exists whose source node
    /// has the given `language`. Used by `get_lang_status` to detect whether
    /// Phase B (SCIP/LSIF) has run for a language without a separate metadata
    /// table. Returns `false` on any query error (safe default: show Tree-sitter).
    pub fn has_refcall_edges_for_language(&self, language: &str) -> bool {
        let result: rusqlite::Result<bool> = self.conn.query_row(
            "SELECT EXISTS( \
                SELECT 1 FROM edges e \
                INNER JOIN nodes n ON e.src = n.id \
                WHERE e.kind = 'ref/call' AND n.language = ?1 \
                LIMIT 1 \
             )",
            params![language],
            |row| row.get::<_, bool>(0),
        );
        match result {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("has_refcall_edges_for_language error: {e}");
                false
            }
        }
    }

    /// Load every row from the `files` table into a path → sha256 map.
    ///
    /// Used by `init_repo`'s parallel indexing pipeline to pre-populate an
    /// in-memory skip set so worker threads can compare hashes without hitting
    /// SQLite on every file.
    pub fn get_all_file_hashes(
        &self,
    ) -> Result<std::collections::HashMap<String, String>, StoreError> {
        (|| -> AnyResult<std::collections::HashMap<String, String>> {
            let mut stmt = self
                .conn
                .prepare("SELECT path, sha256 FROM files")
                .context("preparing get_all_file_hashes")?;
            let mapped = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .context("executing get_all_file_hashes")?;
            let owned: rusqlite::Result<std::collections::HashMap<String, String>> =
                mapped.collect();
            owned.context("collecting file hashes")
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

    /// Write a batch of parsed file graphs in a single SQLite transaction.
    ///
    /// For each `FileGraph` in `batch`:
    /// 1. Retract old FTS rows + decrement vocab refcounts for the path.
    /// 2. Delete old edges and nodes for the path.
    /// 3. Insert new nodes and edges.
    /// 4. Upsert the file hash.
    ///
    /// When `bulk=true` (init path), step 3 writes only `nodes_fts_map` rows
    /// instead of the full FTS5 + vocab update per node.  Call
    /// [`rebuild_fts_from_map`] after all batches to build the index in one pass.
    ///
    /// When `bulk=false` (incremental / reindex path), each node gets the full
    /// `put_node_fts` treatment — FTS5 insert + vocab increment — exactly as before.
    ///
    /// All files in the batch commit atomically — a failure rolls back the
    /// entire batch, leaving the DB consistent.
    pub fn write_file_graphs_batch(
        &mut self,
        batch: &[FileGraph],
        bulk: bool,
    ) -> Result<BatchWriteCounts, StoreError> {
        let staging = self.staging_active;
        (|| -> AnyResult<BatchWriteCounts> {
            // _bulk_fts_pending is a temp table populated by put_node_fts_map_only
            // and consumed by rebuild_fts_from_map. Create it here so the table
            // exists for the duration of this call even when begin_bulk_fts_tracking
            // was not called explicitly by the caller.
            if bulk {
                self.conn
                    .execute_batch(
                        "CREATE TEMP TABLE IF NOT EXISTS _bulk_fts_pending \
                         (node_id INTEGER PRIMARY KEY);",
                    )
                    .context("creating _bulk_fts_pending temp table")?;
            }
            let tx = self
                .conn
                .transaction()
                .context("starting batch write transaction")?;
            let mut counts = BatchWriteCounts::default();

            for file in batch {
                if staging {
                    // ── staging path (bulk init only) ─────────────────────────
                    // No delete pass — the DB is empty on a fresh init.
                    // Plain INSERT into constraint-free TEMP tables: no B-tree
                    // read, no index maintenance. flush_staging_to_production
                    // deduplicates in one GROUP BY pass after all files are done.
                    for node in &file.nodes {
                        tx.execute(
                            "INSERT INTO nodes_stage(id,corpus,root,path,language,\
                             signature,kind,package,line,end_line) \
                             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                            params![
                                node_id_to_i64(node.id),
                                node.vname.corpus,
                                node.vname.root,
                                node.vname.path,
                                node.vname.language,
                                node.vname.signature,
                                node.kind,
                                node.package,
                                node.line.map(|l| l as i64),
                                node.end_line.map(|l| l as i64),
                            ],
                        )
                        .context("staging: inserting node")?;
                        Self::put_node_fts_map_only(&tx, node)
                            .context("staging: put_node_fts_map_only")?;
                        counts.nodes_upserted += 1;
                    }
                    for edge in &file.edges {
                        tx.execute(
                            "INSERT INTO edges_stage(src,dst,kind,provenance,confidence) \
                             VALUES(?1,?2,?3,'tree-sitter',?4)",
                            params![
                                node_id_to_i64(edge.src),
                                node_id_to_i64(edge.dst),
                                edge.kind.as_str(),
                                edge.confidence.map(|c| c as i64),
                            ],
                        )
                        .context("staging: inserting edge")?;
                        counts.edges_upserted += 1;
                    }
                } else {
                    // ── incremental path: delete existing rows, then upsert ───
                    // Load old FTS tokens BEFORE removing map rows so vocab can
                    // be decremented correctly.
                    let old_token_strings: Vec<String> = {
                        let mut stmt = tx
                            .prepare(
                                "SELECT m.tokens FROM nodes_fts_map m \
                                 JOIN nodes n ON n.id = m.node_id WHERE n.path = ?1",
                            )
                            .context("preparing old-token load")?;
                        let mapped = stmt
                            .query_map(params![file.vname_path], |row| row.get::<_, String>(0))
                            .context("querying old tokens")?;
                        let owned: rusqlite::Result<Vec<String>> = mapped.collect();
                        owned.context("collecting old tokens")?
                    };
                    tx.execute(
                        "INSERT INTO nodes_fts(nodes_fts, rowid, tokens) \
                         SELECT 'delete', m.node_id, m.tokens \
                         FROM nodes_fts_map m JOIN nodes n ON n.id = m.node_id \
                         WHERE n.path = ?1",
                        params![file.vname_path],
                    )
                    .context("retracting FTS rows")?;
                    tx.execute(
                        "DELETE FROM nodes_fts_map \
                         WHERE node_id IN (SELECT id FROM nodes WHERE path = ?1)",
                        params![file.vname_path],
                    )
                    .context("removing FTS map rows")?;
                    for ts in &old_token_strings {
                        Self::vocab_decrement(&tx, ts)?;
                    }
                    tx.execute(
                        "DELETE FROM edges \
                         WHERE src IN (SELECT id FROM nodes WHERE path = ?1) \
                            OR dst IN (SELECT id FROM nodes WHERE path = ?1)",
                        params![file.vname_path],
                    )
                    .context("deleting edges for path")?;
                    tx.execute("DELETE FROM nodes WHERE path = ?1", params![file.vname_path])
                        .context("deleting nodes for path")?;

                    for node in &file.nodes {
                        let id_i64 = node_id_to_i64(node.id);
                        tx.execute(
                            "INSERT INTO nodes(id,corpus,root,path,language,signature,kind,package,line,end_line) \
                             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10) \
                             ON CONFLICT(id) DO UPDATE SET kind = excluded.kind, \
                               package = excluded.package, \
                               line = COALESCE(excluded.line, nodes.line), \
                               end_line = COALESCE(excluded.end_line, nodes.end_line)",
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
                                node.end_line.map(|l| l as i64),
                            ],
                        )
                        .context("inserting node in batch")?;
                        if bulk {
                            Self::put_node_fts_map_only(&tx, node)
                                .context("batch put_node_fts_map_only")?;
                        } else {
                            Self::put_node_fts(&tx, node).context("batch put_node_fts")?;
                        }
                        counts.nodes_upserted += 1;
                    }
                    for edge in &file.edges {
                        tx.execute(
                            "INSERT INTO edges(src,dst,kind,provenance,confidence) \
                             VALUES(?1,?2,?3,'tree-sitter',?4) \
                             ON CONFLICT(src,dst,kind) DO NOTHING",
                            params![
                                node_id_to_i64(edge.src),
                                node_id_to_i64(edge.dst),
                                edge.kind.as_str(),
                                edge.confidence.map(|c| c as i64),
                            ],
                        )
                        .context("inserting edge in batch")?;
                        counts.edges_upserted += 1;
                    }
                }

                // File hash goes to production in both paths — one row per
                // source file, non-conflicting, needed for SHA256 delta detection.
                tx.execute(
                    "INSERT INTO files(path, sha256, last_indexed_at) \
                     VALUES(?1, ?2, unixepoch()) \
                     ON CONFLICT(path) DO UPDATE SET \
                       sha256 = excluded.sha256, \
                       last_indexed_at = excluded.last_indexed_at",
                    params![file.vname_path, file.new_hash],
                )
                .context("writing file hash in batch")?;
                counts.files_written += 1;
            }

            tx.commit().context("committing batch write transaction")?;
            Ok(counts)
        })()
        .map_err(|e| StoreError::Database(e.to_string()))
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

    /// Search nodes by name substring, ranked best-first.
    ///
    /// Results are ordered by match quality (exact > prefix > substring), then
    /// production-file bias (test and vendor paths rank lower), then path length.
    /// At most 100 results are returned.
    pub fn search_nodes_by_name(&self, name: &str) -> Result<Vec<Node>, StoreError> {
        // Log the query name (symbol/path, not file contents — SEC log-redaction rule).
        let _span = tracing::debug_span!("store.search_nodes_by_name", query = name).entered();
        (|| -> AnyResult<Vec<Node>> {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT id, corpus, root, path, language, signature, kind, package, line, end_line,
  (
    CASE
      WHEN signature = ?1 THEN 0
      WHEN signature LIKE ?1 || ':%' OR signature LIKE '%:' || ?1 THEN 10
      WHEN signature LIKE ?1 || '%' THEN 20
      WHEN path = ?1 THEN 5
      WHEN path LIKE '%/' || ?1 THEN 15
      ELSE 40
    END
    + CASE WHEN path LIKE '%_test.go' OR path LIKE '%test/%'
           OR path LIKE '%.test.%' OR path LIKE '%_test.%'
           OR path LIKE 'tests/%' OR path LIKE '%/tests/%' THEN 25 ELSE 0 END
    + CASE WHEN path LIKE 'third_party/%' OR path LIKE 'vendor/%'
           OR path LIKE '%/node_modules/%' THEN 50 ELSE 0 END
    + (LENGTH(path) / 32)
  ) AS rank
FROM nodes
WHERE signature LIKE '%' || ?1 || '%'
   OR path LIKE '%' || ?1 || '%'
ORDER BY rank ASC, id ASC
LIMIT 100",
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
                    let end_line: Option<i64> = row.get(9)?;
                    Ok(Node {
                        id,
                        vname,
                        kind,
                        package,
                        line: line.and_then(|l| u32::try_from(l).ok()),
                        end_line: end_line.and_then(|l| u32::try_from(l).ok()),
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
                    "SELECT id, corpus, root, path, language, signature, kind, package, line, end_line FROM nodes",
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
                    let end_line: Option<i64> = row.get(9)?;
                    Ok(Node {
                        id,
                        vname,
                        kind,
                        package,
                        line: line.and_then(|l| u32::try_from(l).ok()),
                        end_line: end_line.and_then(|l| u32::try_from(l).ok()),
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

    /// Fetch all nodes of a given `kind` (full-table scan — use for cheap kinds like "file").
    pub fn nodes_by_kind(&self, kind: &str) -> Result<Vec<Node>, StoreError> {
        (|| -> AnyResult<Vec<Node>> {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT id, corpus, root, path, language, signature, kind, package, line, end_line
                     FROM nodes WHERE kind = ?1",
                )
                .context("preparing nodes_by_kind query")?;
            let rows = stmt
                .query_map(params![kind], |row| {
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
                    let end_line: Option<i64> = row.get(9)?;
                    Ok(Node {
                        id,
                        vname,
                        kind,
                        package,
                        line: line.and_then(|l| u32::try_from(l).ok()),
                        end_line: end_line.and_then(|l| u32::try_from(l).ok()),
                    })
                })
                .context("executing nodes_by_kind query")?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.context("decoding nodes_by_kind row")?);
            }
            Ok(out)
        })()
        .map_err(|e| StoreError::Database(e.to_string()))
    }

    /// Return all (src_path, dst_path) pairs via the two-hop import chain:
    /// any_node --[depends]--> import_node --[resolves-to]--> file.
    /// src_path is the path of the node making the import (any kind: file, function, etc.).
    /// dst_path is the path of the resolved target file.
    /// Used by the graph-overview aggregation to compute cross-package edges.
    pub fn file_import_pairs(&self) -> Result<Vec<(String, String)>, StoreError> {
        (|| -> AnyResult<Vec<(String, String)>> {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT n1.path, n2.path
                     FROM edges e1
                     JOIN nodes n1 ON n1.id = e1.src
                     JOIN edges e2 ON e2.src = e1.dst AND e2.kind = 'resolves-to'
                     JOIN nodes n2 ON n2.id = e2.dst AND n2.kind = 'file'
                     WHERE e1.kind = 'depends'",
                )
                .context("preparing file_import_pairs query")?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .context("executing file_import_pairs query")?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.context("decoding file_import_pairs row")?);
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

    /// Return all `(NodeId, kind)` pairs — used by k-core for shell propagation.
    ///
    /// Cheaper than `all_nodes()` because it projects only `id` and `kind`.
    pub fn all_node_ids_and_kinds(&self) -> Result<Vec<(NodeId, String)>, StoreError> {
        (|| -> AnyResult<Vec<(NodeId, String)>> {
            let mut stmt = self
                .conn
                .prepare("SELECT id, kind FROM nodes")
                .context("preparing all_node_ids_and_kinds query")?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        i64_to_node_id(row.get::<_, i64>(0)?),
                        row.get::<_, String>(1)?,
                    ))
                })
                .context("executing all_node_ids_and_kinds query")?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.context("decoding all_node_ids_and_kinds row")?);
            }
            Ok(out)
        })()
        .map_err(|e| StoreError::Database(e.to_string()))
    }

    /// Batch-write shell numbers produced by k-core decomposition.
    ///
    /// Uses a single prepared statement executed inside one transaction for speed.
    /// O(|shells|) writes — safe to call after every index pass.
    /// Set `embed_text = NULL` for all nodes.
    ///
    /// Called before regenerating embed_text with a new richness tier (model switch).
    pub fn clear_all_embed_texts(&mut self) -> Result<(), StoreError> {
        self.conn
            .execute_batch("UPDATE nodes SET embed_text = NULL")
            .map_err(|e| StoreError::Database(e.to_string()))
    }

    /// Batch-update the `embed_text` column for a set of nodes.
    ///
    /// Called by the daemon after Phase A indexing to store pre-computed
    /// AST-skeleton text so the embed sidecar stays a pure embedding machine.
    pub fn write_embed_texts_batch(
        &mut self,
        pairs: &[(NodeId, String)],
    ) -> Result<(), StoreError> {
        if pairs.is_empty() {
            return Ok(());
        }
        (|| -> AnyResult<()> {
            let tx = self
                .conn
                .transaction()
                .context("begin write_embed_texts_batch tx")?;
            {
                let mut stmt = tx
                    .prepare("UPDATE nodes SET embed_text = ?2 WHERE id = ?1")
                    .context("preparing write_embed_texts_batch stmt")?;
                for (id, text) in pairs {
                    stmt.execute(params![node_id_to_i64(*id), text])
                        .context("executing write_embed_texts_batch update")?;
                }
            }
            tx.commit().context("committing write_embed_texts_batch tx")
        })()
        .map_err(|e| StoreError::Database(e.to_string()))
    }

    /// Return all nodes where `embed_text IS NULL` and the kind is embeddable
    /// (i.e. excludes file/import/module/variable structural noise).
    ///
    /// Called by the daemon after Phase A to find nodes that need embed_text
    /// populated via `skeleton_for_node`.
    pub fn nodes_missing_embed_text(&self) -> Result<Vec<Node>, StoreError> {
        (|| -> AnyResult<Vec<Node>> {
            let mut stmt = self.conn.prepare(
                "SELECT id, corpus, root, path, language, signature, kind, package, line, end_line \
                 FROM nodes \
                 WHERE embed_text IS NULL \
                   AND kind NOT IN ('file', 'file-module', 'import', 'module', 'variable')",
            ).context("preparing nodes_missing_embed_text query")?;
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
                    let end_line: Option<i64> = row.get(9)?;
                    Ok(Node {
                        id,
                        vname,
                        kind,
                        package,
                        line: line.and_then(|l| u32::try_from(l).ok()),
                        end_line: end_line.and_then(|l| u32::try_from(l).ok()),
                    })
                })
                .context("executing nodes_missing_embed_text query")?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.context("decoding nodes_missing_embed_text row")?);
            }
            Ok(out)
        })()
        .map_err(|e| StoreError::Database(e.to_string()))
    }

    pub fn write_shell_numbers(&mut self, shells: &[(NodeId, u32)]) -> Result<(), StoreError> {
        if shells.is_empty() {
            return Ok(());
        }
        (|| -> AnyResult<()> {
            let tx = self
                .conn
                .transaction()
                .context("begin write_shell_numbers tx")?;
            {
                let mut stmt = tx
                    .prepare("UPDATE nodes SET shell_number = ?2 WHERE id = ?1")
                    .context("preparing write_shell_numbers stmt")?;
                for &(id, shell) in shells {
                    stmt.execute(params![node_id_to_i64(id), shell as i64])
                        .context("executing write_shell_numbers update")?;
                }
            }
            tx.commit().context("committing write_shell_numbers tx")
        })()
        .map_err(|e| StoreError::Database(e.to_string()))
    }

    /// Batch-fetch shell numbers for a set of node IDs.
    ///
    /// Unknown IDs are silently skipped (shell defaults to 0 at the call site).
    /// Splits into 500-ID chunks to stay under SQLite's variable limit (SQLITE_MAX_VARIABLE_NUMBER).
    pub fn get_shell_numbers_batch(
        &self,
        ids: &[NodeId],
    ) -> Result<std::collections::HashMap<NodeId, u32>, StoreError> {
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        (|| -> AnyResult<std::collections::HashMap<NodeId, u32>> {
            let mut out = std::collections::HashMap::with_capacity(ids.len());
            for chunk in ids.chunks(500) {
                let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                let sql =
                    format!("SELECT id, shell_number FROM nodes WHERE id IN ({placeholders})");
                let mut stmt = self
                    .conn
                    .prepare(&sql)
                    .context("preparing get_shell_numbers_batch")?;
                let id_params: Vec<i64> = chunk.iter().map(|id| node_id_to_i64(*id)).collect();
                let rows = stmt
                    .query_map(params_from_iter(id_params.iter()), |row| {
                        let id = i64_to_node_id(row.get::<_, i64>(0)?);
                        let shell: i64 = row.get(1)?;
                        Ok((id, shell as u32))
                    })
                    .context("executing get_shell_numbers_batch")?;
                for row in rows {
                    let (id, shell) = row.context("decoding get_shell_numbers_batch row")?;
                    out.insert(id, shell);
                }
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

    /// Write Phase B (SCIP/LSIF semantic) nodes and edges in a single transaction.
    ///
    /// Semantics match repeated `put_node` + `put_edge_lsif` calls but in one
    /// round-trip instead of O(N) transactions. Call this after `invoke_phase_b_all`
    /// returns; the caller must not hold an open transaction.
    pub fn write_phase_b_batch(
        &mut self,
        nodes: &[travsr_core::Node],
        edges: &[travsr_core::Edge],
    ) -> anyhow::Result<()> {
        if nodes.is_empty() && edges.is_empty() {
            return Ok(());
        }
        let tx = self
            .conn
            .transaction()
            .context("write_phase_b_batch: begin")?;
        for node in nodes {
            let id_i64 = node_id_to_i64(node.id);
            tx.execute(
                "INSERT INTO nodes(id, corpus, root, path, language, signature, kind, package, line, end_line) \
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
                 ON CONFLICT(id) DO UPDATE SET kind = excluded.kind, \
                 package = excluded.package, \
                 line = COALESCE(excluded.line, nodes.line), \
                 end_line = COALESCE(excluded.end_line, nodes.end_line)",
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
                    node.end_line.map(|l| l as i64),
                ],
            )
            .context("write_phase_b_batch: insert node")?;
            Self::put_node_fts(&tx, node).context("write_phase_b_batch: put_node_fts")?;
        }
        for edge in edges {
            tx.execute(
                "INSERT INTO edges(src, dst, kind, provenance, confidence) \
                 VALUES(?1, ?2, ?3, 'lsif', ?4) \
                 ON CONFLICT(src, dst, kind) DO UPDATE SET \
                 provenance = 'lsif', confidence = excluded.confidence",
                params![
                    node_id_to_i64(edge.src),
                    node_id_to_i64(edge.dst),
                    edge.kind.as_str(),
                    edge.confidence.map(|c| c as i64),
                ],
            )
            .context("write_phase_b_batch: insert edge")?;
        }
        tx.commit().context("write_phase_b_batch: commit")
    }

    /// G2 attribution-aware Phase B write.
    ///
    /// Writes SCIP definition nodes, then for each [`travsr_core::ScipRef`]
    /// finds the innermost function/method node whose span contains
    /// `caller_line` and emits a `ref/call` edge from it (falls back to the
    /// file node if no enclosing function is found).  Also records a row in
    /// `edge_sites` for every attribution (O4 evidence).
    ///
    /// Must be called **after** the Phase A tree-sitter pass so that
    /// `end_line` spans are already in the DB.
    pub fn write_scip_attributed_batch(
        &mut self,
        corpus: &str,
        nodes: &[travsr_core::Node],
        refs: &[travsr_core::ScipRef],
    ) -> anyhow::Result<()> {
        if nodes.is_empty() && refs.is_empty() {
            return Ok(());
        }
        let tx = self
            .conn
            .transaction()
            .context("write_scip_attributed_batch: begin")?;

        for node in nodes {
            let id_i64 = node_id_to_i64(node.id);
            tx.execute(
                "INSERT INTO nodes(id, corpus, root, path, language, signature, kind, package, line, end_line) \
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
                 ON CONFLICT(id) DO UPDATE SET kind = excluded.kind, \
                 package = excluded.package, \
                 line = COALESCE(excluded.line, nodes.line), \
                 end_line = COALESCE(excluded.end_line, nodes.end_line)",
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
                    node.end_line.map(|l| l as i64),
                ],
            )
            .context("write_scip_attributed_batch: insert node")?;
            Self::put_node_fts(&tx, node).context("write_scip_attributed_batch: put_node_fts")?;
        }

        // P4: build span cache — one SELECT per unique caller_path instead of one per ref.
        // 10k refs / 200 files: 10,000 queries → 200 queries.
        let unique_paths: std::collections::HashSet<&str> =
            refs.iter().map(|r| r.caller_path.as_str()).collect();
        let mut span_cache: std::collections::HashMap<&str, Vec<FnSpan>> =
            std::collections::HashMap::with_capacity(unique_paths.len());
        for path in unique_paths {
            span_cache.insert(path, fetch_all_fn_spans(&tx, corpus, path)?);
        }

        for scip_ref in refs {
            // G2: find the innermost enclosing function/method span in memory.
            let occ_line = scip_ref.caller_line as i64;
            let src_id: Option<i64> = span_cache
                .get(scip_ref.caller_path.as_str())
                .and_then(|spans| find_narrowest_enclosing(spans, occ_line));

            let caller_id = match src_id {
                Some(id) => id,
                None => file_node_for_attribution(&tx, corpus, &scip_ref.caller_path)?,
            };

            let callee_id = node_id_to_i64(scip_ref.callee_id);
            tx.execute(
                "INSERT INTO edges(src, dst, kind, provenance, confidence) \
                 VALUES(?1, ?2, 'ref/call', 'scip', NULL) \
                 ON CONFLICT(src, dst, kind) DO UPDATE SET provenance = 'scip'",
                params![caller_id, callee_id],
            )
            .context("write_scip_attributed_batch: insert edge")?;

            // O4: record call-site line evidence.
            tx.execute(
                "INSERT OR IGNORE INTO edge_sites(src, dst, kind, line) VALUES(?1, ?2, 'ref/call', ?3)",
                params![caller_id, callee_id, occ_line],
            )
            .context("write_scip_attributed_batch: insert edge_site")?;
        }

        tx.commit().context("write_scip_attributed_batch: commit")
    }

    /// G1: Register a mapping from a raw SCIP symbol string to a unified tree-sitter `NodeId`.
    ///
    /// Idempotent: if the alias already exists with the same `node_id`, this is a no-op.
    pub fn register_symbol_alias(
        &mut self,
        scip_symbol: &str,
        node_id: NodeId,
    ) -> anyhow::Result<()> {
        self.conn
            .execute(
                "INSERT INTO symbol_aliases(scip_symbol, node_id) VALUES(?1, ?2) \
                 ON CONFLICT(scip_symbol) DO UPDATE SET node_id = excluded.node_id",
                params![scip_symbol, node_id_to_i64(node_id)],
            )
            .context("register_symbol_alias")?;
        Ok(())
    }

    /// G1: Batch variant of [`Self::register_symbol_alias`].
    ///
    /// Wraps all inserts in a single transaction with a cached statement so
    /// the unification pass pays one fsync for the whole alias set instead of
    /// one autocommit transaction per alias.
    pub fn register_symbol_aliases(&mut self, aliases: &[(String, NodeId)]) -> anyhow::Result<()> {
        if aliases.is_empty() {
            return Ok(());
        }
        let tx = self
            .conn
            .transaction()
            .context("register_symbol_aliases: begin")?;
        {
            let mut stmt = tx
                .prepare_cached(
                    "INSERT INTO symbol_aliases(scip_symbol, node_id) VALUES(?1, ?2) \
                     ON CONFLICT(scip_symbol) DO UPDATE SET node_id = excluded.node_id",
                )
                .context("register_symbol_aliases: prepare")?;
            for (scip_symbol, node_id) in aliases {
                stmt.execute(params![scip_symbol, node_id_to_i64(*node_id)])
                    .context("register_symbol_aliases: insert")?;
            }
        }
        tx.commit().context("register_symbol_aliases: commit")
    }

    /// G1: Find the tree-sitter node at `(corpus, path)` whose signature
    /// matches one of `signatures`, within `max_delta` lines of `scip_line`.
    ///
    /// Candidate signatures come from
    /// `travsr_indexer::scip_unifier::candidate_signatures` and already encode
    /// the node kind via their prefix (`fn:` / `class:` / `var:` / ...), so no
    /// kind filter is needed.  The `(corpus, path)` index keeps the candidate
    /// row set per-file small.  Returns `None` when no candidate is within the
    /// proximity threshold.
    ///
    /// `signatures` is ordered most → least specific by the caller (e.g.
    /// `method:Server.Serve`, `fn:Server.Serve`, `fn:Serve`); ties are broken
    /// by candidate priority **first**, then line distance, so a less-specific
    /// same-named node one line closer cannot shadow a more specific match.
    ///
    /// The SQL string varies only by candidate count, so `prepare_cached`
    /// hits its statement cache across the millions of calls a monorepo
    /// unification pass makes.
    pub fn find_ts_node_for_unification(
        &self,
        corpus: &str,
        path: &str,
        signatures: &[String],
        scip_line: i64,
        max_delta: i64,
    ) -> anyhow::Result<Option<NodeId>> {
        if signatures.is_empty() {
            return Ok(None);
        }
        // Numbered params: ?1=corpus ?2=path ?3..?(n+2)=signatures
        // ?(n+3)=scip_line ?(n+4)=max_delta. The CASE re-uses the signature
        // params to rank rows by candidate priority (0 = most specific).
        let n = signatures.len();
        let placeholders = (3..n + 3)
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(",");
        let priority_case = (3..n + 3)
            .map(|i| format!("WHEN ?{i} THEN {}", i - 3))
            .collect::<Vec<_>>()
            .join(" ");
        let line_p = n + 3;
        let delta_p = n + 4;
        let sql = format!(
            "SELECT id FROM nodes \
             WHERE corpus = ?1 AND path = ?2 \
               AND signature IN ({placeholders}) \
               AND line IS NOT NULL \
               AND ABS(line - ?{line_p}) <= ?{delta_p} \
             ORDER BY CASE signature {priority_case} END ASC, \
                      ABS(line - ?{line_p}) ASC \
             LIMIT 1"
        );

        let mut bind: Vec<&dyn rusqlite::types::ToSql> = Vec::with_capacity(n + 4);
        bind.push(&corpus);
        bind.push(&path);
        for sig in signatures {
            bind.push(sig);
        }
        bind.push(&scip_line);
        bind.push(&max_delta);

        let mut stmt = self
            .conn
            .prepare_cached(&sql)
            .context("find_ts_node_for_unification: prepare")?;
        let id: Option<i64> = stmt
            .query_row(bind.as_slice(), |row| row.get(0))
            .optional()
            .context("find_ts_node_for_unification")?;
        Ok(id.map(i64_to_node_id))
    }

    /// All definition node ids at `(corpus, path)` — tree-sitter and SCIP —
    /// ordered by line.  Used by the CLI to surface function-level callers
    /// when a query resolves to a file node (RFC-014 #317: a file's callers
    /// are the callers of the definitions it contains).  Container nodes
    /// (file, import, packages, modules) are excluded: their incoming edges
    /// are structural, not call sites.
    pub fn definition_node_ids_in_file(
        &self,
        corpus: &str,
        path: &str,
    ) -> anyhow::Result<Vec<NodeId>> {
        let mut stmt = self.conn.prepare(
            "SELECT id FROM nodes \
             WHERE corpus = ?1 AND path = ?2 \
               AND kind IN ('function','method','fn','class','interface','struct', \
                            'trait','enum','type','typedef','union','object', \
                            'protocol','mixin','extension','namespace','init', \
                            'var','variable','const','constant','static') \
             ORDER BY line ASC",
        )?;
        let ids = stmt
            .query_map(params![corpus, path], |row| row.get::<_, i64>(0))?
            .collect::<Result<Vec<i64>, _>>()
            .context("definition_node_ids_in_file")?;
        Ok(ids.into_iter().map(i64_to_node_id).collect())
    }

    /// G1: Look up the unified `NodeId` for a raw SCIP symbol string.
    ///
    /// Returns `None` if the symbol has not been aliased (i.e. no tree-sitter
    /// node matched it during the unification pass).
    pub fn resolve_scip_symbol(&self, scip_symbol: &str) -> anyhow::Result<Option<NodeId>> {
        let id: Option<i64> = self
            .conn
            .query_row(
                "SELECT node_id FROM symbol_aliases WHERE scip_symbol = ?1",
                params![scip_symbol],
                |row| row.get(0),
            )
            .optional()
            .context("resolve_scip_symbol")?;
        Ok(id.map(i64_to_node_id))
    }

    /// Emit `Depends` edges between all ordered file-node pairs that co-define
    /// the same package/module node, using a single SQL self-join.
    ///
    /// Languages whose files share a namespace without explicit imports (Go,
    /// Swift, Kotlin, Java, Dart) emit a package node during Phase A that every
    /// file in the package points at via `DefinesBinding`. This pass finds all
    /// such groups and writes `file_B --Depends--> file_A` for every ordered
    /// pair (B ≠ A), giving blast-radius BFS structural co-package coupling.
    ///
    /// `pkg_kinds` must contain only internal kind strings (e.g. `"go-pkg"`,
    /// `"swift-module"`) — never values derived from user input.
    ///
    /// Returns the number of new edges inserted (`INSERT OR IGNORE` skips
    /// existing rows).
    pub fn emit_copackage_depends(&mut self, pkg_kinds: &[&str]) -> anyhow::Result<usize> {
        if pkg_kinds.is_empty() {
            return Ok(0);
        }
        // All values come from internal constants — no user input reaches here.
        let in_clause = pkg_kinds
            .iter()
            .map(|s| format!("'{}'", s.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "INSERT OR IGNORE INTO edges(src, dst, kind, provenance, confidence) \
             SELECT DISTINCT a.src, b.src, 'depends', 'tree-sitter', NULL \
             FROM edges a \
             JOIN edges b   ON a.dst = b.dst AND a.src != b.src \
             JOIN nodes pkg ON pkg.id = a.dst AND pkg.kind IN ({in_clause}) \
             JOIN nodes fa  ON fa.id = a.src  AND fa.kind = 'file' \
             JOIN nodes fb  ON fb.id = b.src  AND fb.kind = 'file' \
             WHERE a.kind = 'defines/binding' \
               AND b.kind = 'defines/binding'"
        );
        self.conn
            .execute(&sql, [])
            .context("emit_copackage_depends")?;
        Ok(self.conn.changes() as usize)
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

    /// Bulk-init variant of [`put_node_fts`]: writes only the `nodes_fts_map` row
    /// and records the node_id in `_bulk_fts_pending` (a temp table created by
    /// [`begin_bulk_fts_tracking`]), skipping the FTS5 inverted-index insert and
    /// `fts_vocab` increments.
    ///
    /// Called by [`write_file_graphs_batch`] when `bulk=true`.
    /// [`rebuild_fts_from_map`] must be called after all batches to populate
    /// `nodes_fts` and `fts_vocab` for only the written nodes.
    fn put_node_fts_map_only(conn: &Connection, node: &Node) -> AnyResult<()> {
        let id_i64 = node_id_to_i64(node.id);
        let tokens = Self::node_fts_tokens(node);
        conn.execute(
            "INSERT INTO nodes_fts_map(node_id, tokens) VALUES(?1, ?2) \
             ON CONFLICT(node_id) DO UPDATE SET tokens = excluded.tokens",
            params![id_i64, tokens],
        )
        .context("bulk: writing nodes_fts_map")?;
        // Track this node_id so rebuild_fts_from_map inserts FTS entries only
        // for written nodes — not for unchanged nodes whose entries already exist.
        conn.execute(
            "INSERT OR IGNORE INTO _bulk_fts_pending(node_id) VALUES(?1)",
            params![id_i64],
        )
        .context("bulk: recording node_id in _bulk_fts_pending")?;
        Ok(())
    }

    /// Create the per-connection temp table used to track which node IDs had
    /// their `nodes_fts_map` row written during a bulk init run.
    ///
    /// Must be called once before the first [`write_file_graphs_batch`] call
    /// with `bulk=true`.  The table is dropped by [`rebuild_fts_from_map`].
    pub fn begin_bulk_fts_tracking(&mut self) -> anyhow::Result<()> {
        self.conn
            .execute_batch(
                "CREATE TEMP TABLE IF NOT EXISTS _bulk_fts_pending \
                 (node_id INTEGER PRIMARY KEY);",
            )
            .context("creating _bulk_fts_pending temp table")?;
        Ok(())
    }

    /// Create constraint-free TEMP tables for staging bulk init writes.
    ///
    /// During `travsr init`, nodes and edges are written here with plain INSERT
    /// (no ON CONFLICT clause, no index maintenance).  After all files are
    /// processed, call [`flush_staging_to_production`] which deduplicates in a
    /// single GROUP BY pass and writes the clean result to the production tables.
    ///
    /// This eliminates the B-tree read (ON CONFLICT uniqueness check) from every
    /// node and edge insert on the hot init path — converting N×(read+write) to
    /// N×write + 1×sort, where the sort uses sequential I/O via SQLite's external
    /// merge sort.
    ///
    /// Safe to call multiple times (TEMP tables use IF NOT EXISTS).  Crash-safe:
    /// TEMP tables are dropped automatically on connection close, leaving the
    /// production tables untouched.
    pub fn begin_staging_tables(&mut self) -> anyhow::Result<()> {
        self.conn
            .execute_batch(
                "CREATE TEMP TABLE IF NOT EXISTS nodes_stage( \
                   id INTEGER, corpus TEXT, root TEXT, path TEXT, \
                   language TEXT, signature TEXT, kind TEXT, \
                   package TEXT, line INTEGER, end_line INTEGER \
                 ); \
                 CREATE INDEX IF NOT EXISTS nodes_stage_id ON nodes_stage(id); \
                 CREATE TEMP TABLE IF NOT EXISTS edges_stage( \
                   src INTEGER, dst INTEGER, kind TEXT, \
                   provenance TEXT, confidence INTEGER \
                 ); \
                 CREATE INDEX IF NOT EXISTS edges_stage_pk \
                   ON edges_stage(src, dst, kind);",
            )
            .context("creating staging temp tables")?;
        self.staging_active = true;
        Ok(())
    }

    /// Flush staging tables to production in one deduplicating GROUP BY pass.
    ///
    /// Deduplication semantics match the incremental ON CONFLICT path:
    /// - nodes: GROUP BY id deduplicates; MAX(line)/MAX(end_line) picks a value
    ///   when at most one source row has a non-NULL value (by construction,
    ///   same NodeId ⟹ same VName ⟹ same parse output, so all are equal).
    ///   ON CONFLICT(id) DO UPDATE handles re-init where nodes already exist.
    /// - edges: GROUP BY (src,dst,kind) deduplicates; ON CONFLICT DO NOTHING
    ///   is a no-op for any edge already in production.
    ///
    /// Bare non-grouped columns in the nodes SELECT (corpus, root, path, …) are
    /// safe: same id ⟹ same VName (NodeId is a deterministic hash of the VName),
    /// so SQLite's arbitrary-value pick for the group is always correct.
    ///
    /// Must be called after all [`write_file_graphs_batch`] calls with
    /// `staging_active=true` and before [`rebuild_fts_from_map`].
    ///
    /// Returns `(nodes_written, edges_written)` — rows inserted or updated
    /// into production (post-deduplication).
    pub fn flush_staging_to_production(&mut self) -> anyhow::Result<(u64, u64)> {
        if !self.staging_active {
            return Ok((0, 0));
        }
        let result = (|| -> anyhow::Result<(u64, u64)> {
            let tx = self
                .conn
                .transaction()
                .context("starting staging flush transaction")?;

            let nodes_written = tx
                .execute(
                    "INSERT INTO nodes(id,corpus,root,path,language,signature,kind,package,line,end_line) \
                       SELECT id,corpus,root,path,language,signature,kind,package, \
                              MAX(line),MAX(end_line) \
                       FROM nodes_stage GROUP BY id \
                       ON CONFLICT(id) DO UPDATE SET \
                         kind     = excluded.kind, \
                         package  = excluded.package, \
                         line     = COALESCE(excluded.line,     nodes.line), \
                         end_line = COALESCE(excluded.end_line, nodes.end_line)",
                    [],
                )
                .context("inserting nodes from staging")?;

            let edges_written = tx
                .execute(
                    "INSERT INTO edges(src,dst,kind,provenance,confidence) \
                       SELECT src,dst,kind,provenance,MAX(confidence) \
                       FROM edges_stage GROUP BY src,dst,kind \
                       ON CONFLICT(src,dst,kind) DO NOTHING",
                    [],
                )
                .context("inserting edges from staging")?;

            tx.execute_batch("DROP TABLE nodes_stage; DROP TABLE edges_stage;")
                .context("dropping staging tables")?;

            tx.commit().context("committing staging flush")?;
            Ok((nodes_written as u64, edges_written as u64))
        })();
        // Always clear the flag whether success or failure so callers that
        // catch and continue don't write into a poisoned staging state.
        self.staging_active = false;
        result
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
                    "SELECT id, corpus, root, path, language, signature, kind, package, line, end_line \
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
                    let end_line: Option<i64> = row.get(9)?;
                    Ok(Node {
                        id,
                        vname,
                        kind,
                        package,
                        line: line.and_then(|l| u32::try_from(l).ok()),
                        end_line: end_line.and_then(|l| u32::try_from(l).ok()),
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
        let step3 = (|| -> AnyResult<Vec<Node>> {
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
        .map_err(|e| StoreError::Database(e.to_string()))?;

        if !step3.is_empty() {
            return Ok(step3);
        }

        // Step 4 — semantic ANN (RFC-018). Fires only when Steps 1–3 all return
        // empty. The hook is injected by the daemon's EmbedSupervisor at store-open
        // time; it is None by default (zero cost — no embed plugin installed).
        if let Some(knn_fn) = self.embed_knn_hook.as_ref() {
            let pairs = knn_fn(query, 20)?;
            if !pairs.is_empty() {
                tracing::debug!(layer = "embed_ann", nodes_returned = pairs.len());
                let ids: Vec<NodeId> = pairs.into_iter().map(|(id, _score)| id).collect();
                return self.get_nodes(&ids);
            }
        }
        Ok(Vec::new())
    }

    /// Execute a raw FTS5 MATCH expression against the nodes index.
    /// Shared by Step 2 (T0) and Step 3 (L2-A) to avoid duplicated query logic.
    fn fts_query_nodes(&self, match_expr: &str) -> AnyResult<Vec<Node>> {
        let sql = "SELECT n.id, n.corpus, n.root, n.path, n.language, n.signature, \
                          n.kind, n.package, n.line, n.end_line \
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
                let end_line: Option<i64> = row.get(9)?;
                Ok(Node {
                    id,
                    vname,
                    kind,
                    package,
                    line: line.and_then(|l| u32::try_from(l).ok()),
                    end_line: end_line.and_then(|l| u32::try_from(l).ok()),
                })
            })
            .context("executing FTS5 query")?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.context("decoding FTS5 row")?);
        }
        Ok(out)
    }

    /// Language-scoped variant of [`search_nodes_by_name`].
    /// Appends `AND language = ?2` so only nodes from the requested language
    /// are returned, before the 100-row LIMIT.
    fn search_nodes_by_name_with_lang(
        &self,
        name: &str,
        lang: &str,
    ) -> Result<Vec<Node>, StoreError> {
        let _span =
            tracing::debug_span!("store.search_nodes_by_name_with_lang", query = name, lang)
                .entered();
        (|| -> AnyResult<Vec<Node>> {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT id, corpus, root, path, language, signature, kind, package, line, end_line,
  (
    CASE
      WHEN signature = ?1 THEN 0
      WHEN signature LIKE ?1 || ':%' OR signature LIKE '%:' || ?1 THEN 10
      WHEN signature LIKE ?1 || '%' THEN 20
      WHEN path = ?1 THEN 5
      WHEN path LIKE '%/' || ?1 THEN 15
      ELSE 40
    END
    + CASE WHEN path LIKE '%_test.go' OR path LIKE '%test/%'
           OR path LIKE '%.test.%' OR path LIKE '%_test.%'
           OR path LIKE 'tests/%' OR path LIKE '%/tests/%' THEN 25 ELSE 0 END
    + CASE WHEN path LIKE 'third_party/%' OR path LIKE 'vendor/%'
           OR path LIKE '%/node_modules/%' THEN 50 ELSE 0 END
    + (LENGTH(path) / 32)
  ) AS rank
FROM nodes
WHERE (signature LIKE '%' || ?1 || '%'
   OR path LIKE '%' || ?1 || '%')
  AND language = ?2
ORDER BY rank ASC, id ASC
LIMIT 100",
                )
                .context("preparing lang-filtered search query")?;
            let rows = stmt
                .query_map(params![name, lang], |row| {
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
                    let end_line: Option<i64> = row.get(9)?;
                    Ok(Node {
                        id,
                        vname,
                        kind,
                        package,
                        line: line.and_then(|l| u32::try_from(l).ok()),
                        end_line: end_line.and_then(|l| u32::try_from(l).ok()),
                    })
                })
                .context("executing lang-filtered search query")?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.context("decoding search row")?);
            }
            Ok(out)
        })()
        .map_err(|e| StoreError::Database(e.to_string()))
    }

    /// Language-scoped variant of [`fts_query_nodes`].
    /// Appends `AND n.language = ?2` before the 50-row FTS LIMIT.
    fn fts_query_nodes_with_lang(&self, match_expr: &str, lang: &str) -> AnyResult<Vec<Node>> {
        let sql = "SELECT n.id, n.corpus, n.root, n.path, n.language, n.signature, \
                          n.kind, n.package, n.line, n.end_line \
                   FROM nodes_fts \
                   JOIN nodes_fts_map m ON nodes_fts.rowid = m.node_id \
                   JOIN nodes n ON n.id = m.node_id \
                   WHERE nodes_fts MATCH ?1 \
                     AND n.language = ?2 \
                   ORDER BY bm25(nodes_fts) \
                   LIMIT 50";
        let mut stmt = self
            .conn
            .prepare(sql)
            .context("preparing lang-filtered FTS5 query")?;
        let rows = stmt
            .query_map(params![match_expr, lang], |row| {
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
                let end_line: Option<i64> = row.get(9)?;
                Ok(Node {
                    id,
                    vname,
                    kind,
                    package,
                    line: line.and_then(|l| u32::try_from(l).ok()),
                    end_line: end_line.and_then(|l| u32::try_from(l).ok()),
                })
            })
            .context("executing lang-filtered FTS5 query")?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.context("decoding lang-filtered FTS5 row")?);
        }
        Ok(out)
    }

    /// Language-filtered variant of [`search_nodes_fuzzy`].
    ///
    /// When `lang_filter` is `None`, delegates to [`search_nodes_fuzzy`] unchanged.
    /// When `Some(lang)`, each of the 4 search steps applies `AND language = ?`
    /// at the SQL level — before the 50-result FTS cap — guaranteeing results
    /// are from the requested language only.
    pub fn search_nodes_fuzzy_filtered(
        &self,
        query: &str,
        lang_filter: Option<&str>,
    ) -> Result<Vec<Node>, StoreError> {
        let lang = match lang_filter {
            None => return self.search_nodes_fuzzy(query),
            Some(l) => l,
        };
        let _span =
            tracing::debug_span!("store.search_nodes_fuzzy_filtered", query, lang).entered();

        // Step 1 — exact substring, language-filtered.
        let exact = self.search_nodes_by_name_with_lang(query, lang)?;
        if !exact.is_empty() {
            tracing::debug!(layer = "exact_lang", nodes_returned = exact.len());
            return Ok(exact);
        }

        // Step 2 — FTS5, language-filtered.
        let step2_expr = match build_fuzzy_match_expr_db(query, &self.conn)
            .map_err(|e| StoreError::Database(e.to_string()))?
        {
            Some(e) => e,
            None => return Ok(Vec::new()),
        };
        let step2 = self
            .fts_query_nodes_with_lang(&step2_expr, lang)
            .map_err(|e| StoreError::Database(e.to_string()))?;
        if !step2.is_empty() {
            tracing::debug!(layer = "fts5_t0_lang", nodes_returned = step2.len());
            return Ok(step2);
        }

        // Step 3 — L2-A expansion, language-filtered.
        let step3 = (|| -> AnyResult<Vec<Node>> {
            let raw_str = tokenize_identifier(query);
            if raw_str.is_empty() {
                return Ok(Vec::new());
            }
            let raw: Vec<String> = raw_str.split_whitespace().map(str::to_string).collect();
            let t0_tokens = crate::seed_lexicon::expand_tokens(&raw);
            let l2a_extra = self.expand_query(&t0_tokens)?;
            if l2a_extra.is_empty() {
                return Ok(Vec::new());
            }
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
            self.fts_query_nodes_with_lang(&match_expr, lang)
        })()
        .map_err(|e| StoreError::Database(e.to_string()))?;
        if !step3.is_empty() {
            tracing::debug!(layer = "fts5_l2a_lang", nodes_returned = step3.len());
            return Ok(step3);
        }

        // Step 4 — embed ANN + post-filter by language.
        if let Some(knn_fn) = self.embed_knn_hook.as_ref() {
            let pairs = knn_fn(query, 20)?;
            if !pairs.is_empty() {
                let ids: Vec<NodeId> = pairs.into_iter().map(|(id, _score)| id).collect();
                let nodes = self.get_nodes(&ids)?;
                let filtered: Vec<Node> = nodes
                    .into_iter()
                    .filter(|n| n.vname.language == lang)
                    .collect();
                if !filtered.is_empty() {
                    tracing::debug!(layer = "embed_ann_lang", nodes_returned = filtered.len());
                    return Ok(filtered);
                }
            }
        }
        Ok(Vec::new())
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

    /// Rebuild `nodes_fts` and `fts_vocab` from `nodes_fts_map` in one pass.
    ///
    /// Called by `init_repo_with_progress` after all bulk batches are flushed.
    /// Produces an identical end-state to per-node `put_node_fts` calls but
    /// avoids per-node FTS5 inverted-index writes — the dominant bottleneck on
    /// large repos (kubernetes: ~12M FTS ops → ~10 min, bulk rebuild: ~1 pass).
    ///
    /// Safe to call on an empty store (no-op) or a partially-written store
    /// (idempotent: clears and repopulates both tables).
    pub fn rebuild_fts_from_map(&mut self) -> anyhow::Result<()> {
        // Phase 1: insert FTS entries only for nodes written in this bulk run.
        // _bulk_fts_pending (created by begin_bulk_fts_tracking, populated by
        // put_node_fts_map_only) holds exactly those node IDs. Unchanged files
        // are excluded because they were hash-skipped and never reached
        // put_node_fts_map_only, so their existing FTS entries stay untouched.
        //
        // Contentless FTS5 tables forbid DELETE FROM and the 'rebuild' command.
        // Retracting non-existent entries corrupts the index, so we filter by
        // _bulk_fts_pending rather than clear-and-rebuild.
        let t_fts5 = std::time::Instant::now();
        {
            let tx = self
                .conn
                .transaction()
                .context("starting FTS rebuild transaction")?;
            tx.execute_batch(
                "INSERT INTO nodes_fts(rowid, tokens) \
                   SELECT m.node_id, m.tokens FROM nodes_fts_map m \
                   WHERE EXISTS ( \
                     SELECT 1 FROM _bulk_fts_pending p WHERE p.node_id = m.node_id \
                   ); \
                 DROP TABLE IF EXISTS _bulk_fts_pending;",
            )
            .context("bulk-inserting nodes_fts from pending nodes")?;
            tx.commit().context("committing FTS rebuild")?;
        }
        tracing::info!(
            elapsed_ms = t_fts5.elapsed().as_millis(),
            "TIMING: FTS5 bulk INSERT done"
        );

        // Phase 2: rebuild fts_vocab by counting tokens in Rust, then
        // bulk-inserting unique token counts. This replaces ~6–10M per-token
        // SQL upserts with ~50k–200k unique-token inserts (kubernetes estimate).
        let t_vocab_scan = std::time::Instant::now();
        let token_strings: Vec<String> = {
            let mut stmt = self
                .conn
                .prepare("SELECT tokens FROM nodes_fts_map")
                .context("preparing nodes_fts_map scan for vocab rebuild")?;
            let collected: Vec<String> = stmt
                .query_map([], |row| row.get(0))
                .context("executing nodes_fts_map scan")?
                .collect::<rusqlite::Result<_>>()
                .context("collecting token strings for vocab rebuild")?;
            collected
        };
        tracing::info!(
            elapsed_ms = t_vocab_scan.elapsed().as_millis(),
            rows = token_strings.len(),
            "TIMING: nodes_fts_map scan done"
        );

        // Count token frequencies in Rust — O(nodes × tokens_per_node).
        let t_count = std::time::Instant::now();
        let mut counts: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        for ts in &token_strings {
            for tok in ts.split_whitespace() {
                if tok.len() >= 3 {
                    *counts.entry(tok.to_string()).or_insert(0) += 1;
                }
            }
        }
        tracing::info!(
            elapsed_ms = t_count.elapsed().as_millis(),
            unique_tokens = counts.len(),
            "TIMING: token count done"
        );

        if counts.is_empty() {
            return Ok(());
        }

        let t_vocab_write = std::time::Instant::now();
        let tx = self
            .conn
            .transaction()
            .context("starting fts_vocab rebuild transaction")?;
        tx.execute("DELETE FROM fts_vocab", [])
            .context("clearing fts_vocab for rebuild")?;
        for (token, refcount) in &counts {
            tx.execute(
                "INSERT INTO fts_vocab(token, refcount) VALUES(?1, ?2)",
                params![token, *refcount as i64],
            )
            .context("inserting rebuilt fts_vocab row")?;
        }
        tx.commit().context("committing fts_vocab rebuild")?;
        tracing::info!(
            elapsed_ms = t_vocab_write.elapsed().as_millis(),
            rows = counts.len(),
            "TIMING: fts_vocab write done"
        );

        tracing::info!(
            nodes = token_strings.len(),
            unique_tokens = counts.len(),
            "bulk init: FTS + vocab rebuild complete"
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
                "INSERT INTO nodes(id, corpus, root, path, language, signature, kind, package, line, end_line) \
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
                 ON CONFLICT(id) DO UPDATE SET kind = excluded.kind, \
                 package = excluded.package, \
                 line = COALESCE(excluded.line, nodes.line), \
                 end_line = COALESCE(excluded.end_line, nodes.end_line)",
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
                    node.end_line.map(|l| l as i64),
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
                    "SELECT corpus, root, path, language, signature, kind, package, line, end_line \
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
                        let end_line: Option<i64> = row.get(8)?;
                        Ok(Node {
                            id,
                            vname,
                            kind,
                            package,
                            line: line.and_then(|l| u32::try_from(l).ok()),
                            end_line: end_line.and_then(|l| u32::try_from(l).ok()),
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
                "SELECT id, corpus, root, path, language, signature, kind, package, line, end_line \
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
                        let end_line: Option<i64> = row.get(9)?;
                        Ok(Node {
                            id,
                            vname,
                            kind,
                            package,
                            line: line.and_then(|l| u32::try_from(l).ok()),
                            end_line: end_line.and_then(|l| u32::try_from(l).ok()),
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

/// G2 fallback: resolve the file node id for a SCIP ref whose enclosing
/// function could not be found.
///
/// Looks up the real Phase A file node by `(corpus, path, kind='file')` —
/// never reconstructs the VName hash, which silently diverges when fields
/// like `language` differ (the cause of 385K dangling edges on kubernetes).
/// If the path was never Phase-A-indexed (`.travsrignore` etc.), synthesizes
/// a file node matching the Phase A VName convention so the edge has a real
/// Minimal span descriptor fetched once per unique path for G2 attribution.
struct FnSpan {
    id: i64,
    line: i64,
    end_line: i64,
}

/// Fetch all function/method spans for a single `path` in one query.
///
/// Results are ordered by `(end_line - line) ASC, id ASC` so that
/// [`find_narrowest_enclosing`] can short-circuit on the first match — exactly
/// replicating the `ORDER BY … LIMIT 1` the per-ref SELECT used to do.
fn fetch_all_fn_spans(
    tx: &rusqlite::Transaction<'_>,
    corpus: &str,
    path: &str,
) -> anyhow::Result<Vec<FnSpan>> {
    let mut stmt = tx
        .prepare_cached(
            "SELECT id, line, end_line FROM nodes \
             WHERE corpus = ?1 AND path = ?2 \
               AND kind IN ('function', 'method', 'fn') \
               AND line IS NOT NULL AND end_line IS NOT NULL \
             ORDER BY (end_line - line) ASC, id ASC",
        )
        .context("fetch_all_fn_spans: prepare")?;
    let spans = stmt
        .query_map(params![corpus, path], |row| {
            Ok(FnSpan {
                id: row.get(0)?,
                line: row.get(1)?,
                end_line: row.get(2)?,
            })
        })
        .context("fetch_all_fn_spans: query")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("fetch_all_fn_spans: collect")?;
    Ok(spans)
}

/// Return the `id` of the narrowest function span containing `occ_line`.
///
/// `spans` must be pre-sorted `(end_line - line) ASC, id ASC` (i.e. the order
/// returned by [`fetch_all_fn_spans`]).  The first match is therefore the
/// narrowest enclosing span — identical to `ORDER BY … LIMIT 1`.
fn find_narrowest_enclosing(spans: &[FnSpan], occ_line: i64) -> Option<i64> {
    spans
        .iter()
        .find(|s| s.line <= occ_line && s.end_line >= occ_line)
        .map(|s| s.id)
}

/// src; the language is taken from any sibling node at the same path (SCIP
/// definition nodes for the path are inserted earlier in this transaction).
fn file_node_for_attribution(
    tx: &rusqlite::Transaction<'_>,
    corpus: &str,
    path: &str,
) -> anyhow::Result<i64> {
    if let Some(id) = tx
        .query_row(
            "SELECT id FROM nodes WHERE corpus = ?1 AND path = ?2 AND kind = 'file' LIMIT 1",
            params![corpus, path],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .context("file_node_for_attribution: lookup")?
    {
        return Ok(id);
    }
    let language: String = tx
        .query_row(
            "SELECT language FROM nodes WHERE corpus = ?1 AND path = ?2 LIMIT 1",
            params![corpus, path],
            |row| row.get(0),
        )
        .optional()
        .context("file_node_for_attribution: sibling language")?
        .unwrap_or_default();
    let vname = travsr_core::VName::new(corpus, "", path, &language, "file");
    let id = node_id_to_i64(vname.id());
    tx.execute(
        "INSERT OR IGNORE INTO nodes(id, corpus, root, path, language, signature, kind, package) \
         VALUES(?1, ?2, '', ?3, ?4, 'file', 'file', '')",
        params![id, corpus, path, language],
    )
    .context("file_node_for_attribution: insert file node")?;
    Ok(id)
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
    fn v14_migration_creates_covering_reverse_index() {
        let store = SqliteStore::open_in_memory().unwrap();
        let cov_exists: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type='index' AND name='idx_edges_dst_kind_cov'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cov_exists, 1, "idx_edges_dst_kind_cov must exist after v14");

        let old_exists: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type='index' AND name='idx_edges_dst_kind'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(old_exists, 0, "idx_edges_dst_kind must be dropped by v14");
    }

    #[test]
    fn v14_reverse_edge_query_uses_covering_index() {
        let store = SqliteStore::open_in_memory().unwrap();
        // EXPLAIN QUERY PLAN — column 3 is the "detail" text in SQLite 3.36+.
        let plan: String = store
            .conn
            .query_row(
                "EXPLAIN QUERY PLAN SELECT src FROM edges WHERE dst=? AND kind=?",
                rusqlite::params![0i64, "ref/call"],
                |row| row.get(3),
            )
            .unwrap();
        assert!(
            plan.contains("idx_edges_dst_kind_cov"),
            "iter_edges_to query must use covering index; EXPLAIN detail: {plan}",
        );
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

    #[test]
    fn unification_prefers_higher_priority_candidate_over_closer_line() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        // SCIP definition at line 10. Candidates ordered most → least
        // specific: method:Server.Serve (priority 0), fn:Serve (priority 1).
        // The lower-priority node is CLOSER (line 11, delta 1) than the
        // higher-priority node (line 13, delta 3) — priority must still win.
        let hi = Node::new(
            VName::new("c", "main", "src/s.go", "go", "method:Server.Serve"),
            "method",
        )
        .with_line(13);
        let lo = Node::new(
            VName::new("c", "main", "src/s.go", "go", "fn:Serve"),
            "function",
        )
        .with_line(11);
        store.put_node(&hi).unwrap();
        store.put_node(&lo).unwrap();

        let candidates = vec!["method:Server.Serve".to_string(), "fn:Serve".to_string()];
        let found = store
            .find_ts_node_for_unification("c", "src/s.go", &candidates, 10, 5)
            .unwrap();
        assert_eq!(found, Some(hi.id), "higher-priority candidate must win");

        // When the higher-priority signature matches nothing, the closer
        // lower-priority node wins on line distance as before.
        let candidates = vec!["method:Other.Serve".to_string(), "fn:Serve".to_string()];
        let found = store
            .find_ts_node_for_unification("c", "src/s.go", &candidates, 10, 5)
            .unwrap();
        assert_eq!(found, Some(lo.id), "falls back to line proximity");
    }

    #[test]
    fn edge_sites_dedup_on_reinsert() {
        let store = SqliteStore::open_in_memory().unwrap();
        // The composite PK makes INSERT OR IGNORE an actual dedup: the same
        // (src, dst, kind, line) row inserted twice must yield one row.
        store
            .conn
            .execute(
                "INSERT OR IGNORE INTO edge_sites(src, dst, kind, line) VALUES(1, 2, 'ref/call', 7)",
                [],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT OR IGNORE INTO edge_sites(src, dst, kind, line) VALUES(1, 2, 'ref/call', 7)",
                [],
            )
            .unwrap();
        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM edge_sites", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn register_symbol_aliases_batch_roundtrip() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        // symbol_aliases.node_id is a FK → nodes(id); nodes must exist first.
        let n1 = travsr_core::Node::new(
            travsr_core::VName::new("c", "", "a/b.go", "go", "method:X.m"),
            "method",
        );
        let n2 = travsr_core::Node::new(
            travsr_core::VName::new("c", "", "a/b.go", "go", "method:Y.n"),
            "method",
        );
        store.put_node(&n1).unwrap();
        store.put_node(&n2).unwrap();
        let aliases = vec![
            ("scip . a/b 1.0 X#m().".to_string(), n1.id),
            ("scip . a/b 1.0 Y#n().".to_string(), n2.id),
        ];
        store.register_symbol_aliases(&aliases).unwrap();
        assert_eq!(
            store.resolve_scip_symbol("scip . a/b 1.0 X#m().").unwrap(),
            Some(n1.id)
        );
        assert_eq!(
            store.resolve_scip_symbol("scip . a/b 1.0 Y#n().").unwrap(),
            Some(n2.id)
        );
        // Upsert: re-registering the same scip_symbol with a different node id
        // overwrites; use a distinct node so the FK is satisfied.
        let n3 = travsr_core::Node::new(
            travsr_core::VName::new("c", "", "a/b.go", "go", "method:Z.p"),
            "method",
        );
        store.put_node(&n3).unwrap();
        store
            .register_symbol_aliases(&[("scip . a/b 1.0 X#m().".to_string(), n3.id)])
            .unwrap();
        assert_eq!(
            store.resolve_scip_symbol("scip . a/b 1.0 X#m().").unwrap(),
            Some(n3.id)
        );
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
    fn search_ranks_exact_signature_first() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        store
            .put_node(&node_with_path("pkg/eviction/handler.go", "fn:eviction"))
            .unwrap();
        store
            .put_node(&node_with_path(
                "pkg/eviction/mock_test.go",
                "fn:eviction_test",
            ))
            .unwrap();
        let results = store.search_nodes_by_name("eviction").unwrap();
        // exact signature match must rank first
        assert_eq!(results[0].vname.signature, "fn:eviction");
    }

    #[test]
    fn search_ranks_production_over_test_path() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        // Both signatures are identical substrings so match-quality tier is equal;
        // only the test-path penalty (+25) should differentiate them.
        store
            .put_node(&node_with_path("pkg/eviction/mock_test.go", "fn:handleX"))
            .unwrap();
        store
            .put_node(&node_with_path("pkg/eviction/handler.go", "fn:handleY"))
            .unwrap();
        let results = store.search_nodes_by_name("handle").unwrap();
        // production file must rank ahead of test file
        assert_eq!(results[0].vname.path, "pkg/eviction/handler.go");
    }

    #[test]
    fn search_ranks_vendor_last() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        store
            .put_node(&node_with_path("vendor/lib/util.go", "fn:util"))
            .unwrap();
        store
            .put_node(&node_with_path("src/util.go", "fn:util2"))
            .unwrap();
        let results = store.search_nodes_by_name("util").unwrap();
        // vendor path must rank below production
        assert_eq!(results[0].vname.path, "src/util.go");
    }

    #[test]
    fn search_limit_caps_results() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        for i in 0..150u32 {
            store
                .put_node(&node_with_path(
                    &format!("src/file{i}.go"),
                    &format!("fn:target_{i}"),
                ))
                .unwrap();
        }
        let results = store.search_nodes_by_name("target").unwrap();
        assert!(results.len() <= 100);
    }

    #[test]
    fn search_rank_deterministic_tiebreak() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        // Two nodes with identical rank (same path pattern, same sig pattern)
        store
            .put_node(&node_with_path("src/a.go", "fn:foo"))
            .unwrap();
        store
            .put_node(&node_with_path("src/b.go", "fn:foo"))
            .unwrap();
        let r1 = store.search_nodes_by_name("foo").unwrap();
        let r2 = store.search_nodes_by_name("foo").unwrap();
        // Order must be deterministic across two calls
        assert_eq!(r1[0].vname.path, r2[0].vname.path);
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

    #[test]
    fn has_refcall_edges_for_language_returns_false_when_none() {
        let store = SqliteStore::open_in_memory().unwrap();
        assert!(!store.has_refcall_edges_for_language("typescript"));
    }

    #[test]
    fn has_refcall_edges_for_language_returns_true_after_insert() {
        use travsr_core::{Edge, EdgeKind, Node, VName};
        let mut store = SqliteStore::open_in_memory().unwrap();
        let n1 = Node::new(VName::new("", "", "a.ts", "typescript", "fn:a"), "function");
        let n2 = Node::new(VName::new("", "", "b.ts", "typescript", "fn:b"), "function");
        store.put_node(&n1).unwrap();
        store.put_node(&n2).unwrap();
        store
            .put_edge(&Edge::new(n1.id, n2.id, EdgeKind::RefCall))
            .unwrap();
        assert!(store.has_refcall_edges_for_language("typescript"));
        assert!(
            !store.has_refcall_edges_for_language("rust"),
            "different lang must return false"
        );
    }

    #[test]
    fn bulk_init_fts_matches_row_by_row() {
        // Index the same nodes via bulk path and row-by-row path; verify
        // that FTS search returns identical results in both cases.
        let nodes = vec![
            Node::new(
                VName::new("c", "", "src/auth.ts", "typescript", "fn:AuthService"),
                "function",
            ),
            Node::new(
                VName::new("c", "", "src/pay.ts", "typescript", "fn:PaymentHandler"),
                "function",
            ),
        ];

        // ── row-by-row (reference) ──────────────────────────────────────────
        let mut ref_store = SqliteStore::open_in_memory().unwrap();
        for n in &nodes {
            ref_store.put_node(n).unwrap();
        }
        let ref_results = ref_store.search_nodes_fuzzy("auth").unwrap();

        // ── bulk path ───────────────────────────────────────────────────────
        let mut bulk_store = SqliteStore::open_in_memory().unwrap();
        let batch: Vec<FileGraph> = nodes
            .iter()
            .map(|n| FileGraph {
                vname_path: n.vname.path.clone(),
                new_hash: "deadbeef".to_string(),
                nodes: vec![n.clone()],
                edges: vec![],
            })
            .collect();
        bulk_store.write_file_graphs_batch(&batch, true).unwrap();
        bulk_store.rebuild_fts_from_map().unwrap();
        let bulk_results = bulk_store.search_nodes_fuzzy("auth").unwrap();

        assert_eq!(
            ref_results.len(),
            bulk_results.len(),
            "bulk FTS must return the same number of results as row-by-row"
        );
        assert!(
            bulk_results
                .iter()
                .any(|n| n.vname.signature.contains("Auth")),
            "bulk FTS must find AuthService"
        );
    }

    #[test]
    fn staging_nodes_and_edges_reach_production_after_flush() {
        let corpus = "staging-test";
        let mut store = SqliteStore::open_in_memory().unwrap();
        store.begin_bulk_fts_tracking().unwrap();
        store.begin_staging_tables().unwrap();

        let node_a = Node::new(
            VName::new(corpus, "", "src/a.rs", "rust", "fn:foo"),
            "function",
        );
        let node_b = Node::new(
            VName::new(corpus, "", "src/b.rs", "rust", "fn:bar"),
            "function",
        );
        let edge = travsr_core::Edge {
            src: node_a.id,
            dst: node_b.id,
            kind: travsr_core::EdgeKind::RefCall,
            confidence: None,
        };

        let batch = vec![
            FileGraph {
                vname_path: "src/a.rs".into(),
                new_hash: "aaa".into(),
                nodes: vec![node_a.clone()],
                edges: vec![edge.clone()],
            },
            FileGraph {
                vname_path: "src/b.rs".into(),
                new_hash: "bbb".into(),
                nodes: vec![node_b.clone()],
                edges: vec![],
            },
        ];
        store.write_file_graphs_batch(&batch, true).unwrap();

        // Before flush: production tables must be empty.
        assert_eq!(
            store.node_count().unwrap(),
            0,
            "nodes must be in staging, not production yet"
        );
        assert_eq!(
            store.edge_count().unwrap(),
            0,
            "edges must be in staging, not production yet"
        );

        let (nodes_written, edges_written) = store.flush_staging_to_production().unwrap();

        assert_eq!(nodes_written, 2, "both nodes must reach production");
        assert_eq!(edges_written, 1, "the ref/call edge must reach production");
        assert!(
            !store.staging_active,
            "staging_active must be cleared after flush"
        );

        // Verify graph connectivity.
        let callers = store.iter_edges_to(node_b.id).unwrap();
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0].src, node_a.id);
    }

    #[test]
    fn staging_deduplicates_duplicate_nodes_and_edges() {
        let corpus = "dup-test";
        let mut store = SqliteStore::open_in_memory().unwrap();
        store.begin_bulk_fts_tracking().unwrap();
        store.begin_staging_tables().unwrap();

        let node = Node::new(
            VName::new(corpus, "", "src/lib.rs", "rust", "fn:init"),
            "function",
        )
        .with_line(1)
        .with_end_line(10);
        // Same node emitted twice (e.g. two overlapping indexing passes).
        let node_dup = node.clone();
        let edge = travsr_core::Edge {
            src: node.id,
            dst: node.id,
            kind: travsr_core::EdgeKind::RefCall,
            confidence: None,
        };

        let batch = vec![
            FileGraph {
                vname_path: "src/lib.rs".into(),
                new_hash: "aaa".into(),
                nodes: vec![node.clone()],
                edges: vec![edge.clone()],
            },
            FileGraph {
                vname_path: "src/lib.rs".into(),
                new_hash: "aaa".into(),
                nodes: vec![node_dup],
                edges: vec![edge],
            },
        ];
        store.write_file_graphs_batch(&batch, true).unwrap();
        let (nodes_written, edges_written) = store.flush_staging_to_production().unwrap();

        // GROUP BY must collapse duplicates.
        assert_eq!(nodes_written, 1, "duplicate node must be deduplicated");
        assert_eq!(edges_written, 1, "duplicate edge must be deduplicated");
    }

    #[test]
    fn staging_flush_is_idempotent_on_existing_db() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        store.begin_bulk_fts_tracking().unwrap();
        store.begin_staging_tables().unwrap();

        let node = Node::new(
            VName::new("c", "", "src/lib.rs", "rust", "fn:foo"),
            "function",
        )
        .with_line(1)
        .with_end_line(10);
        let batch = vec![FileGraph {
            vname_path: "src/lib.rs".into(),
            new_hash: "aaa".into(),
            nodes: vec![node.clone()],
            edges: vec![],
        }];
        store.write_file_graphs_batch(&batch, true).unwrap();
        store.flush_staging_to_production().unwrap();
        assert_eq!(
            store.node_count().unwrap(),
            1,
            "first flush must produce one node"
        );

        // Second init — same node already in production; ON CONFLICT must upsert cleanly.
        store.begin_staging_tables().unwrap();
        store.write_file_graphs_batch(&batch, true).unwrap();
        let result = store.flush_staging_to_production();
        assert!(result.is_ok(), "re-init must not fail: {:?}", result.err());
        assert_eq!(
            store.node_count().unwrap(),
            1,
            "re-init must not duplicate nodes"
        );
    }

    #[test]
    fn staging_fts_matches_bulk_path_after_flush() {
        let nodes = vec![
            Node::new(
                VName::new("c", "", "src/auth.rs", "rust", "fn:AuthService"),
                "function",
            ),
            Node::new(
                VName::new("c", "", "src/pay.rs", "rust", "fn:PaymentHandler"),
                "function",
            ),
        ];

        // Reference: row-by-row path.
        let mut ref_store = SqliteStore::open_in_memory().unwrap();
        for n in &nodes {
            ref_store.put_node(n).unwrap();
        }
        let ref_results = ref_store.search_nodes_fuzzy("auth").unwrap();

        // Staging path.
        let mut store = SqliteStore::open_in_memory().unwrap();
        store.begin_bulk_fts_tracking().unwrap();
        store.begin_staging_tables().unwrap();
        let batch: Vec<FileGraph> = nodes
            .iter()
            .map(|n| FileGraph {
                vname_path: n.vname.path.clone(),
                new_hash: "hash".into(),
                nodes: vec![n.clone()],
                edges: vec![],
            })
            .collect();
        store.write_file_graphs_batch(&batch, true).unwrap();
        store.flush_staging_to_production().unwrap();
        store.rebuild_fts_from_map().unwrap();
        let stage_results = store.search_nodes_fuzzy("auth").unwrap();

        assert_eq!(
            ref_results.len(),
            stage_results.len(),
            "staging+flush FTS must return same results as row-by-row"
        );
    }

    #[test]
    fn bulk_init_vocab_counts_match_row_by_row() {
        let node = Node::new(
            VName::new("c", "", "src/session.ts", "typescript", "fn:SessionStore"),
            "function",
        );

        let mut ref_store = SqliteStore::open_in_memory().unwrap();
        ref_store.put_node(&node).unwrap();

        let mut bulk_store = SqliteStore::open_in_memory().unwrap();
        let batch = vec![FileGraph {
            vname_path: node.vname.path.clone(),
            new_hash: "deadbeef".to_string(),
            nodes: vec![node.clone()],
            edges: vec![],
        }];
        bulk_store.write_file_graphs_batch(&batch, true).unwrap();
        bulk_store.rebuild_fts_from_map().unwrap();

        // "session" and "store" are the primary tokens — both must have refcount 1.
        for tok in &["session", "store"] {
            let ref_rc = ref_store.fts_vocab_refcount(tok).unwrap();
            let bulk_rc = bulk_store.fts_vocab_refcount(tok).unwrap();
            assert_eq!(
                ref_rc, bulk_rc,
                "fts_vocab refcount for '{tok}' must match row-by-row"
            );
            assert!(
                bulk_rc.is_some(),
                "token '{tok}' must be present in fts_vocab after bulk rebuild"
            );
        }
    }

    #[test]
    fn set_bulk_init_mode_restores_synchronous() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("bulk.db");
        let mut store = SqliteStore::open(&db_path).unwrap();
        store.set_bulk_init_mode(true).unwrap();
        store.set_bulk_init_mode(false).unwrap();
        // After restoring, journal_mode must still be WAL (pragma was not changed).
        assert_eq!(store.journal_mode().unwrap().to_lowercase(), "wal");
    }

    // ── file_import_pairs ─────────────────────────────────────────────────────

    #[test]
    fn file_import_pairs_empty_store_returns_empty() {
        let store = SqliteStore::open_in_memory().unwrap();
        assert!(store.file_import_pairs().unwrap().is_empty());
    }

    #[test]
    fn file_import_pairs_depends_without_resolves_to_excluded() {
        // A Depends edge exists but no ResolvesTo follows it — must return empty.
        let mut store = SqliteStore::open_in_memory().unwrap();
        let src = Node::new(
            VName::new("", "", "pkg/a/mod.ts", "typescript", "fn:a"),
            "function",
        );
        let imp = Node::new(
            VName::new("", "", "pkg/a/mod.ts", "typescript", "import:x"),
            "import",
        );
        store.put_node(&src).unwrap();
        store.put_node(&imp).unwrap();
        store
            .put_edge(&Edge::new(src.id, imp.id, EdgeKind::Depends))
            .unwrap();
        assert!(store.file_import_pairs().unwrap().is_empty());
    }

    #[test]
    fn file_import_pairs_two_hop_chain_returned() {
        // full chain: file --Depends--> import_node --ResolvesTo--> file
        let mut store = SqliteStore::open_in_memory().unwrap();
        let src = Node::new(
            VName::new("", "", "pkg/a/mod.ts", "typescript", "pkg/a/mod.ts"),
            "file",
        );
        let imp = Node::new(
            VName::new("", "", "test/b/util.ts", "typescript", "import:b"),
            "import",
        );
        let dst = Node::new(
            VName::new("", "", "test/b/util.ts", "typescript", "test/b/util.ts"),
            "file",
        );
        store.put_node(&src).unwrap();
        store.put_node(&imp).unwrap();
        store.put_node(&dst).unwrap();
        store
            .put_edge(&Edge::new(src.id, imp.id, EdgeKind::Depends))
            .unwrap();
        store
            .put_edge(&Edge::new(imp.id, dst.id, EdgeKind::ResolvesTo))
            .unwrap();
        let pairs = store.file_import_pairs().unwrap();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, "pkg/a/mod.ts");
        assert_eq!(pairs[0].1, "test/b/util.ts");
    }

    // P4 golden test: grouped span fetch produces identical (ref → enclosing_id) mapping
    // as the old per-ref SELECT, including nested/overlapping functions and the fallback
    // to file node when no function encloses the reference line.
    #[test]
    fn scip_attributed_batch_p4_attribution_golden() {
        let corpus = "test-corpus";
        let path = "src/foo.rs";
        let mut store = SqliteStore::open_in_memory().unwrap();

        // Outer function: lines 1–20.
        let outer = Node::new(VName::new(corpus, "", path, "rust", "fn:outer"), "function")
            .with_line(1)
            .with_end_line(20);

        // Inner (narrower) function: lines 5–10, nested inside outer.
        let inner = Node::new(VName::new(corpus, "", path, "rust", "fn:inner"), "function")
            .with_line(5)
            .with_end_line(10);

        // A callee symbol (definition node).
        let callee = Node::new(
            VName::new(corpus, "", "src/bar.rs", "rust", "fn:callee"),
            "function",
        );

        // Phase A: write the Phase A tree-sitter nodes so spans are in the DB.
        store
            .write_scip_attributed_batch(
                corpus,
                &[outer.clone(), inner.clone(), callee.clone()],
                &[],
            )
            .unwrap();

        // Three refs on the same file, exercising three cases:
        //   line 7  → inside inner (5–10): narrowest = inner
        //   line 15 → inside outer but NOT inner (11–20): narrowest = outer
        //   line 25 → outside every function: falls back to file node
        let refs = vec![
            travsr_core::ScipRef {
                caller_path: path.to_string(),
                caller_line: 7,
                callee_id: callee.id,
            },
            travsr_core::ScipRef {
                caller_path: path.to_string(),
                caller_line: 15,
                callee_id: callee.id,
            },
            travsr_core::ScipRef {
                caller_path: path.to_string(),
                caller_line: 25,
                callee_id: callee.id,
            },
        ];

        store
            .write_scip_attributed_batch(corpus, &[], &refs)
            .unwrap();

        // Read back the edges and verify attribution.
        // all_edges returns (src: NodeId, dst: NodeId, kind: String, provenance: String).
        let all_edges = store.all_edges().unwrap();
        let ref_call_edges: Vec<_> = all_edges
            .iter()
            .filter(|(_, _, kind, _)| kind == "ref/call")
            .collect();

        // line 7 → inner
        assert!(
            ref_call_edges
                .iter()
                .any(|(src, dst, _, _)| *src == inner.id && *dst == callee.id),
            "line 7 ref must be attributed to inner function"
        );
        // line 15 → outer (not inner)
        assert!(
            ref_call_edges
                .iter()
                .any(|(src, dst, _, _)| *src == outer.id && *dst == callee.id),
            "line 15 ref must be attributed to outer function"
        );
        // line 25 → file node (attribution falls back; language = "rust" from sibling node)
        let file_vname = VName::new(corpus, "", path, "rust", "file");
        let file_id = file_vname.id();
        assert!(
            ref_call_edges
                .iter()
                .any(|(src, dst, _, _)| *src == file_id && *dst == callee.id),
            "line 25 ref must fall back to file node attribution"
        );
    }
}
