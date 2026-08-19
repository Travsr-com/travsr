//! Backend-agnostic schema migration framework for Travsr storage backends.
//!
//! ## Why this exists
//! The original `SqliteStore::migrate()` was a private method that applied
//! a static `&[&str]` of SQL strings. This module decouples migration logic
//! from the backend implementation so SQLite (and any future engine) registers
//! its own migration structs and shares the same versioned runner.
//!
//! ## Design
//! - `Migration` — one schema change, identified by a monotonic version number
//! - `StoreMigratable` — extends `Store` with DDL execution + version tracking
//! - `MigrationRunner` — applies pending migrations in version order; idempotent

use anyhow::{Context as _, Result};

use crate::Store;

// ── Public traits ─────────────────────────────────────────────────────────────

/// A single schema migration identified by a monotonically increasing version.
///
/// Implementations are backend-specific: SQLite migrations call `exec_ddl`
/// with SQL strings.
///
/// Version numbers start at 1. Version 0 means "no migrations applied yet"
/// (fresh database). Every `Migration` must have a unique, stable `version()`.
///
/// # Known limitation — atomicity gap
/// `MigrationRunner::run` applies DDL (`up()`) and records the new schema
/// version as two separate steps with no transaction wrapping them together.
/// If the process is killed between `up()` returning and `set_schema_version`
/// being written, the next startup will re-run `up()` against DDL that was
/// already partially applied.
///
/// **All `up()` implementations must therefore be idempotent under re-execution.
/// Use `CREATE TABLE IF NOT EXISTS`, `CREATE INDEX IF NOT EXISTS`, and equivalent
/// backend-specific guards. For `ALTER TABLE … ADD COLUMN`, call
/// `store.column_exists(table, column)?` first — SQLite does not support
/// `ADD COLUMN IF NOT EXISTS` syntax.** A migration that is not idempotent will
/// corrupt the schema on the re-run.
///
/// If strict atomicity is required in the future, extend `StoreMigratable` with
/// `begin_tx(&mut self) -> Result<()>` / `commit_tx(&mut self) -> Result<()>`
/// and wrap each migration in a DDL transaction.
pub trait Migration: Send + Sync {
    /// The schema version this migration upgrades TO.
    fn version(&self) -> u32;

    /// Apply this migration to `store`.
    ///
    /// Called only when `store.schema_version()? < self.version()`.
    /// After a successful return the runner writes the new version to meta —
    /// do not write the version inside `up()`.
    ///
    /// **Must be idempotent** — see the trait-level documentation on the
    /// atomicity gap between DDL application and version recording.
    fn up(&self, store: &mut dyn StoreMigratable) -> Result<()>;

    /// Return `false` when `up()` calls statements that cannot run inside an
    /// explicit transaction (e.g. VACUUM). The runner skips the BEGIN/COMMIT
    /// wrapping for these migrations. All individual DDL statements in `up()`
    /// must remain idempotent since re-runs cannot be rolled back atomically.
    fn is_transactional(&self) -> bool {
        true
    }
}

/// Extends [`Store`] with the DDL execution and version-tracking capabilities
/// required by [`MigrationRunner`].
///
/// Backend implementations:
/// - **SQLite** — `exec_ddl` calls `rusqlite::Connection::execute_batch`;
///   `schema_version`/`set_schema_version` read/write the `meta` table;
///   `column_exists` queries `pragma_table_info`.
///
/// # Note on supertrait bound
/// `StoreMigratable: Store` is broader than strictly required by the runner
/// (which only calls the three methods below), but it ensures every migratable
/// backend is also a full storage backend. Test doubles must implement all of
/// `Store`; see `FakeStore` in the test module for the minimal implementation.
pub trait StoreMigratable: Store {
    /// Execute a DDL statement against the backend (SQL, Cypher, etc.).
    /// Must be idempotent when called with `IF NOT EXISTS` guards.
    fn exec_ddl(&mut self, ddl: &str) -> Result<()>;

    /// Read the current schema version from the backend's meta store.
    /// Returns 0 when no version has been recorded yet (fresh database).
    fn schema_version(&self) -> Result<u32>;

    /// Persist the schema version to the backend's meta store.
    fn set_schema_version(&mut self, v: u32) -> Result<()>;

    /// Returns `true` if `column` exists in `table`.
    ///
    /// Used to guard `ALTER TABLE … ADD COLUMN` on backends (such as SQLite)
    /// that do not support `ADD COLUMN IF NOT EXISTS` syntax. The default
    /// implementation returns `false` — backends that cannot natively express
    /// idempotent column additions must override this.
    fn column_exists(&self, _table: &str, _column: &str) -> Result<bool> {
        Ok(false)
    }
}

// ── Runner ────────────────────────────────────────────────────────────────────

/// Applies pending [`Migration`]s in ascending version order.
///
/// # Idempotency
/// `run` reads the current schema version before applying any migration and
/// skips all migrations whose `version() <= current`. Calling `run` twice on
/// the same store is a safe no-op.
///
/// # Atomicity
/// Each migration is written to meta individually: if migration N succeeds but
/// N+1 panics, the database is left at version N rather than 0. This matches
/// the "apply-and-record" pattern used by most migration frameworks.
#[derive(Default)]
pub struct MigrationRunner {
    migrations: Vec<Box<dyn Migration>>,
}

impl MigrationRunner {
    /// Create an empty runner. Use [`register`] to add migrations before
    /// calling [`run`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a migration. Migrations are sorted by version before running so
    /// registration order does not matter.
    ///
    /// # Panics
    /// Panics if a migration with the same `version()` has already been
    /// registered. Duplicate versions would cause double DDL execution and
    /// schema corruption — this is a programmer error caught at startup
    /// (inside `SqliteStore::open`) so it fires in both debug and release.
    pub fn register(&mut self, migration: impl Migration + 'static) {
        let v = migration.version();
        assert!(
            !self.migrations.iter().any(|m| m.version() == v),
            "duplicate migration version {v} registered, each version must be unique"
        );
        self.migrations.push(Box::new(migration));
    }

    /// The highest registered migration version — i.e. the schema version a
    /// fully migrated store reports. Used by read-only opens to verify
    /// compatibility without running the (write-path) runner.
    pub fn latest_version(&self) -> u32 {
        self.migrations
            .iter()
            .map(|m| m.version())
            .max()
            .unwrap_or(0)
    }

    /// Apply all pending migrations to `store`.
    ///
    /// Migrations whose `version()` is already recorded in meta are skipped.
    /// After each successful migration the version is written to meta so that
    /// a crash mid-sequence leaves the store at the last successfully applied
    /// version.
    pub fn run(&self, store: &mut dyn StoreMigratable) -> Result<()> {
        let current = store.schema_version().context("reading schema version")?;

        let mut pending: Vec<&dyn Migration> = self
            .migrations
            .iter()
            .map(|m| m.as_ref())
            .filter(|m| m.version() > current)
            .collect();

        // Sort ascending so migrations are applied in the correct order
        // regardless of registration order.
        pending.sort_by_key(|m| m.version());

        for migration in pending {
            let v = migration.version();
            // SC-H2: wrap each migration's DDL + version bump in a single
            // transaction so a crash between `up()` and `set_schema_version()`
            // doesn't leave the schema version stale.
            // Exception: migrations that call VACUUM or other statements that
            // cannot run inside an explicit transaction override
            // `is_transactional()` to return `false`; the runner skips the
            // BEGIN/COMMIT for those and relies on per-statement idempotency.
            if migration.is_transactional() {
                store
                    .exec_ddl("BEGIN")
                    .with_context(|| format!("begin transaction for migration v{v}"))?;
                let apply_result = migration
                    .up(store)
                    .with_context(|| format!("applying migration v{v}"))
                    .and_then(|()| {
                        store
                            .set_schema_version(v)
                            .with_context(|| format!("recording migration v{v}"))
                    });
                if let Err(e) = apply_result {
                    let _ = store.exec_ddl("ROLLBACK");
                    return Err(e);
                }
                store
                    .exec_ddl("COMMIT")
                    .with_context(|| format!("commit transaction for migration v{v}"))?;
            } else {
                migration
                    .up(store)
                    .with_context(|| format!("applying migration v{v}"))?;
                store
                    .set_schema_version(v)
                    .with_context(|| format!("recording migration v{v}"))?;
            }
        }

        Ok(())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Edge, Node, NodeId, Store};
    use travsr_core::VName;
    use travsr_error::StoreError;

    // ── Minimal in-memory test backend ────────────────────────────────────────

    /// Fake store for testing the runner in isolation — no SQLite required.
    struct FakeStore {
        version: u32,
        ddl_log: Vec<String>,
    }

    impl FakeStore {
        fn new() -> Self {
            Self {
                version: 0,
                ddl_log: Vec::new(),
            }
        }
    }

    // Store trait — only version + ddl matter for migration tests.
    impl Store for FakeStore {
        fn put_node(&mut self, _: &Node) -> Result<NodeId, StoreError> {
            Ok(VName::new("", "", "", "", "").id())
        }
        fn put_edge(&mut self, _: &Edge) -> Result<(), StoreError> {
            Ok(())
        }
        fn get_node(&self, _: NodeId) -> Result<Option<Node>, StoreError> {
            Ok(None)
        }
        fn iter_edges_from(&self, _: NodeId) -> Result<Vec<Edge>, StoreError> {
            Ok(Vec::new())
        }
        fn iter_edges_to(&self, _: NodeId) -> Result<Vec<Edge>, StoreError> {
            Ok(Vec::new())
        }
        fn get_nodes(&self, _: &[NodeId]) -> Result<Vec<Node>, StoreError> {
            Ok(Vec::new())
        }
    }

    impl StoreMigratable for FakeStore {
        fn exec_ddl(&mut self, ddl: &str) -> Result<()> {
            self.ddl_log.push(ddl.to_string());
            Ok(())
        }
        fn schema_version(&self) -> Result<u32> {
            Ok(self.version)
        }
        fn set_schema_version(&mut self, v: u32) -> Result<()> {
            self.version = v;
            Ok(())
        }
    }

    // ── Migration stubs ───────────────────────────────────────────────────────

    struct FakeMigrationV1;
    impl Migration for FakeMigrationV1 {
        fn version(&self) -> u32 {
            1
        }
        fn up(&self, store: &mut dyn StoreMigratable) -> Result<()> {
            store.exec_ddl("CREATE TABLE v1")
        }
    }

    struct FakeMigrationV2;
    impl Migration for FakeMigrationV2 {
        fn version(&self) -> u32 {
            2
        }
        fn up(&self, store: &mut dyn StoreMigratable) -> Result<()> {
            store.exec_ddl("ALTER TABLE v1 ADD COLUMN v2")
        }
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    #[test]
    fn runner_applies_all_migrations_on_fresh_store() {
        let mut store = FakeStore::new();
        let mut runner = MigrationRunner::new();
        runner.register(FakeMigrationV1);
        runner.register(FakeMigrationV2);

        runner.run(&mut store).unwrap();

        assert_eq!(store.version, 2);
        // SC-H2: each migration is wrapped in BEGIN / COMMIT.
        assert_eq!(
            store.ddl_log,
            [
                "BEGIN",
                "CREATE TABLE v1",
                "COMMIT",
                "BEGIN",
                "ALTER TABLE v1 ADD COLUMN v2",
                "COMMIT",
            ]
        );
    }

    #[test]
    fn runner_is_idempotent() {
        let mut store = FakeStore::new();
        let mut runner = MigrationRunner::new();
        runner.register(FakeMigrationV1);
        runner.register(FakeMigrationV2);

        runner.run(&mut store).unwrap();
        let ddl_count_after_first = store.ddl_log.len();

        runner.run(&mut store).unwrap(); // second call — must be a no-op
        assert_eq!(
            store.ddl_log.len(),
            ddl_count_after_first,
            "second run must not apply any migration"
        );
    }

    #[test]
    fn runner_skips_already_applied_migrations() {
        let mut store = FakeStore::new();
        store.version = 1; // simulate V1 already applied

        let mut runner = MigrationRunner::new();
        runner.register(FakeMigrationV1);
        runner.register(FakeMigrationV2);

        runner.run(&mut store).unwrap();

        assert_eq!(store.version, 2);
        // Only V2 DDL must have been executed, wrapped in BEGIN / COMMIT.
        assert_eq!(
            store.ddl_log,
            ["BEGIN", "ALTER TABLE v1 ADD COLUMN v2", "COMMIT"]
        );
    }

    #[test]
    fn runner_applies_out_of_order_registrations_correctly() {
        let mut store = FakeStore::new();
        let mut runner = MigrationRunner::new();
        runner.register(FakeMigrationV2); // registered first
        runner.register(FakeMigrationV1); // registered second

        runner.run(&mut store).unwrap();

        // Must be applied V1 → V2 regardless of registration order.
        assert_eq!(
            store.ddl_log,
            [
                "BEGIN",
                "CREATE TABLE v1",
                "COMMIT",
                "BEGIN",
                "ALTER TABLE v1 ADD COLUMN v2",
                "COMMIT",
            ]
        );
    }

    #[test]
    fn runner_records_version_after_each_migration() {
        struct VersionCapture {
            inner: FakeStore,
            versions_at_set: Vec<u32>,
        }
        impl Store for VersionCapture {
            fn put_node(&mut self, n: &Node) -> Result<NodeId, StoreError> {
                self.inner.put_node(n)
            }
            fn put_edge(&mut self, e: &Edge) -> Result<(), StoreError> {
                self.inner.put_edge(e)
            }
            fn get_node(&self, id: NodeId) -> Result<Option<Node>, StoreError> {
                self.inner.get_node(id)
            }
            fn iter_edges_from(&self, id: NodeId) -> Result<Vec<Edge>, StoreError> {
                self.inner.iter_edges_from(id)
            }
            fn iter_edges_to(&self, id: NodeId) -> Result<Vec<Edge>, StoreError> {
                self.inner.iter_edges_to(id)
            }
            fn get_nodes(&self, ids: &[NodeId]) -> Result<Vec<Node>, StoreError> {
                self.inner.get_nodes(ids)
            }
        }
        impl StoreMigratable for VersionCapture {
            fn exec_ddl(&mut self, ddl: &str) -> Result<()> {
                self.inner.exec_ddl(ddl)
            }
            fn schema_version(&self) -> Result<u32> {
                self.inner.schema_version()
            }
            fn set_schema_version(&mut self, v: u32) -> Result<()> {
                self.versions_at_set.push(v);
                self.inner.set_schema_version(v)
            }
        }

        let mut store = VersionCapture {
            inner: FakeStore::new(),
            versions_at_set: Vec::new(),
        };
        let mut runner = MigrationRunner::new();
        runner.register(FakeMigrationV1);
        runner.register(FakeMigrationV2);

        runner.run(&mut store).unwrap();

        // Version must be set once per migration, in order.
        assert_eq!(store.versions_at_set, [1, 2]);
        // Verify DDL was applied *before* the version was recorded.
        // SC-H2: each migration adds BEGIN + DDL + COMMIT = 3 entries per migration.
        assert_eq!(
            store.inner.ddl_log.len(),
            6,
            "BEGIN, DDL, COMMIT for each of 2 migrations = 6 entries"
        );
    }

    #[test]
    fn runner_does_not_record_version_when_up_fails() {
        /// A migration whose `up()` always fails.
        struct FailingMigration;
        impl Migration for FailingMigration {
            fn version(&self) -> u32 {
                1
            }
            fn up(&self, _: &mut dyn StoreMigratable) -> Result<()> {
                anyhow::bail!("intentional DDL failure")
            }
        }

        let mut store = FakeStore::new();
        let mut runner = MigrationRunner::new();
        runner.register(FailingMigration);

        let result = runner.run(&mut store);

        assert!(result.is_err(), "run() must propagate the migration error");
        assert_eq!(
            store.version, 0,
            "schema version must not advance when up() fails"
        );
        // SC-H2: BEGIN is executed before up(); ROLLBACK is issued on failure.
        // The FailingMigration.up() bails before any exec_ddl, so log = [BEGIN, ROLLBACK].
        assert_eq!(
            store.ddl_log,
            ["BEGIN", "ROLLBACK"],
            "BEGIN + ROLLBACK recorded but no migration DDL"
        );
    }
}
