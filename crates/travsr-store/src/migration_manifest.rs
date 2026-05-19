//! Migration integrity manifest for the SQLite → Kùzu backend transition.
//!
//! Implements SEC-008: SHA-256 manifest computation, integrity verification,
//! atomic swap, and rollback-on-mismatch for the one-way SQLite → Kùzu
//! migration.
//!
//! # Design
//! - The manifest types and [`compute_manifest_sqlite`] are always compiled so
//!   manifests can be read and verified without a Kùzu dependency.
//! - [`compute_manifest_kuzu`] and [`migrate_sqlite_to_kuzu`] are gated behind
//!   `#[cfg(feature = "kuzu")]` and require `KuzuStore` from the kuzu branch
//!   (issue #27) to be present in the crate.
//!
//! # Invariant
//! The SQLite database is **never** modified, renamed, or deleted during
//! migration.  On any verification failure the staging directory is removed
//! and the caller can continue using the SQLite store.

use std::collections::HashSet;
#[cfg(feature = "kuzu")]
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use sha2::{Digest, Sha256};

use crate::SqliteStore;

#[cfg(feature = "kuzu")]
use crate::KuzuStore;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// One structural edge entry in the integrity manifest.
///
/// Provenance is intentionally excluded — it is a SQLite-only metadata concept
/// and is not replicated to Kùzu.  The manifest captures graph topology only.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct ManifestEntry {
    /// Source node id.
    pub src: u64,
    /// Destination node id.
    pub dst: u64,
    /// Edge kind string (e.g. `"ref/call"`, `"depends"`).
    pub kind: String,
}

/// Integrity manifest written alongside a migration.
///
/// The `sha256` field covers the canonical serialisation of all edges and
/// isolated nodes — two manifests with the same `sha256` represent
/// structurally identical graphs regardless of insertion order or backend.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Manifest {
    /// ISO-8601 UTC timestamp when the manifest was produced.
    pub produced_at: String,
    /// Which backend was read (`"sqlite"` or `"kuzu"`).
    pub source_backend: String,
    /// Number of nodes in the graph.
    pub node_count: u64,
    /// Number of edges in the graph.
    pub edge_count: u64,
    /// SHA-256 hex digest of the canonical edge + isolated-node serialisation.
    pub sha256: String,
    /// Node ids with no incident edges, sorted ascending.
    pub isolated_node_ids: Vec<u64>,
}

/// Errors that can occur during a SQLite → Kùzu migration.
#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    /// The SHA-256 digest of the Kùzu graph does not match the SQLite source.
    #[error("manifest SHA-256 mismatch: sqlite={sqlite_sha}, kuzu={kuzu_sha}")]
    ManifestMismatch {
        sqlite_sha: String,
        kuzu_sha: String,
    },

    /// Node counts differ between backends after migration.
    #[error("node count mismatch: sqlite={sqlite_count}, kuzu={kuzu_count}")]
    NodeCountMismatch { sqlite_count: u64, kuzu_count: u64 },

    /// Edge counts differ between backends after migration.
    #[error("edge count mismatch: sqlite={sqlite_count}, kuzu={kuzu_count}")]
    EdgeCountMismatch { sqlite_count: u64, kuzu_count: u64 },

    /// The atomic rename that commits the migration failed at the OS level.
    #[error("atomic swap failed: {0}")]
    AtomicSwapFailed(std::io::Error),

    /// A store operation returned an error.
    #[error("store error: {0}")]
    Store(#[from] anyhow::Error),
}

// ---------------------------------------------------------------------------
// Manifest computation
// ---------------------------------------------------------------------------

/// Compute a [`Manifest`] from a [`SqliteStore`].
///
/// Reads all nodes and edges.  Provenance is dropped — the manifest captures
/// structural graph identity only.
#[must_use = "manifest result must be checked or used for integrity verification"]
pub fn compute_manifest_sqlite(store: &SqliteStore) -> Result<Manifest, MigrationError> {
    let nodes = store.all_nodes().context("reading all nodes from sqlite")?;
    let edges = store.all_edges().context("reading all edges from sqlite")?;

    let node_count = nodes.len() as u64;
    let edge_count = edges.len() as u64;

    let mut connected: HashSet<u64> = HashSet::new();
    let entries: Vec<ManifestEntry> = edges
        .iter()
        .map(|(src, dst, kind, _provenance)| {
            connected.insert(src.0);
            connected.insert(dst.0);
            ManifestEntry {
                src: src.0,
                dst: dst.0,
                kind: kind.clone(),
            }
        })
        .collect();

    let mut isolated: Vec<u64> = nodes
        .iter()
        .filter(|n| !connected.contains(&n.id.0))
        .map(|n| n.id.0)
        .collect();
    isolated.sort_unstable();

    // Pass `isolated` by value — `canonical_sha256` takes ownership and sorts
    // internally; cloning here would be a redundant allocation.
    let (sha256, sorted_isolated) = canonical_sha256(entries, isolated);

    Ok(Manifest {
        produced_at: utc_now(),
        source_backend: "sqlite".to_string(),
        node_count,
        edge_count,
        sha256,
        isolated_node_ids: sorted_isolated,
    })
}

/// Compute a [`Manifest`] from a [`KuzuStore`].
///
/// Requires the `kuzu` feature. Same algorithm as [`compute_manifest_sqlite`].
/// `KuzuStore` must expose `all_nodes() -> Result<Vec<Node>>` and
/// `all_edges() -> Result<Vec<(NodeId, NodeId, String)>>` (no provenance).
#[cfg(feature = "kuzu")]
pub fn compute_manifest_kuzu(store: &KuzuStore) -> Result<Manifest, MigrationError> {
    let nodes = store.all_nodes().context("reading all nodes from kuzu")?;
    let edges = store.all_edges().context("reading all edges from kuzu")?;

    let node_count = nodes.len() as u64;
    let edge_count = edges.len() as u64;

    let mut connected: HashSet<u64> = HashSet::new();
    let entries: Vec<ManifestEntry> = edges
        .iter()
        .map(|(src, dst, kind)| {
            connected.insert(src.0);
            connected.insert(dst.0);
            ManifestEntry {
                src: src.0,
                dst: dst.0,
                kind: kind.clone(),
            }
        })
        .collect();

    let mut isolated: Vec<u64> = nodes
        .iter()
        .filter(|n| !connected.contains(&n.id.0))
        .map(|n| n.id.0)
        .collect();
    isolated.sort_unstable();

    let (sha256, sorted_isolated) = canonical_sha256(entries, isolated);

    Ok(Manifest {
        produced_at: utc_now(),
        source_backend: "kuzu".to_string(),
        node_count,
        edge_count,
        sha256,
        isolated_node_ids: sorted_isolated,
    })
}

// ---------------------------------------------------------------------------
// Migration entry point
// ---------------------------------------------------------------------------

/// Migrate all data from `sqlite_store` to a new Kùzu database at `kuzu_dir`.
///
/// # Algorithm
/// 1. Compute SHA-256 manifest of the SQLite graph (pre-flight).
/// 2. Write all nodes and edges to a staging directory (`graph.kuzu.new`).
///    Any leftover staging dir from a previous crashed attempt is cleaned first.
/// 3. Compute SHA-256 manifest of the Kùzu staging graph (post-migration).
/// 4. Verify counts and SHA-256 match.  On mismatch: remove staging, return `Err`.
/// 5. Write the manifest JSON to the parent directory atomically.
/// 6. Rename the staging directory to `kuzu_dir` (atomic commit on POSIX).
///
/// The SQLite database is **never** touched.  On any failure the caller can
/// continue using the existing [`SqliteStore`].
///
/// # Returns
/// The path of the committed Kùzu directory (equal to `kuzu_dir`) on success.
#[cfg(feature = "kuzu")]
pub fn migrate_sqlite_to_kuzu(
    sqlite_store: &SqliteStore,
    kuzu_dir: &Path,
) -> Result<PathBuf, MigrationError> {
    use crate::Store as _;
    use travsr_core::{Edge, EdgeKind};

    // Step 1 — pre-migration manifest.
    let sqlite_manifest = compute_manifest_sqlite(sqlite_store)?;

    // Step 2 — staging directory (sibling of kuzu_dir).
    let staging_dir = kuzu_dir.with_file_name("graph.kuzu.new");
    if staging_dir.exists() {
        // Clean up any leftover from a previous crashed attempt.
        if let Err(e) = std::fs::remove_dir_all(&staging_dir) {
            tracing::warn!(
                "SEC-008: could not remove leftover staging dir {}: {e}",
                staging_dir.display()
            );
        }
    }

    // Step 3 — open staging KuzuStore and copy all nodes + edges.
    let mut kuzu_staging = KuzuStore::open(&staging_dir).context("opening kuzu staging store")?;

    for node in &sqlite_store
        .all_nodes()
        .context("reading nodes from sqlite")?
    {
        kuzu_staging
            .put_node(node)
            .context("writing node to kuzu staging")?;
    }

    for (src, dst, kind, _provenance) in &sqlite_store
        .all_edges()
        .context("reading edges from sqlite")?
    {
        let ek = EdgeKind::from_str(kind)
            .with_context(|| format!("unknown edge kind during migration: {kind}"))?;
        kuzu_staging
            .put_edge(&Edge::new(*src, *dst, ek))
            .context("writing edge to kuzu staging")?;
    }

    // Step 4 — post-migration manifest.
    let kuzu_manifest = compute_manifest_kuzu(&kuzu_staging)?;

    // Helper: best-effort staging cleanup; logs a warning if removal fails.
    let cleanup_staging = |staging: &std::path::Path| {
        if let Err(e) = std::fs::remove_dir_all(staging) {
            tracing::warn!(
                "SEC-008: could not remove staging dir {} after mismatch: {e}",
                staging.display()
            );
        }
    };

    // Step 5 — count-level sanity checks (fast path).
    if sqlite_manifest.node_count != kuzu_manifest.node_count {
        cleanup_staging(&staging_dir);
        return Err(MigrationError::NodeCountMismatch {
            sqlite_count: sqlite_manifest.node_count,
            kuzu_count: kuzu_manifest.node_count,
        });
    }
    if sqlite_manifest.edge_count != kuzu_manifest.edge_count {
        cleanup_staging(&staging_dir);
        return Err(MigrationError::EdgeCountMismatch {
            sqlite_count: sqlite_manifest.edge_count,
            kuzu_count: kuzu_manifest.edge_count,
        });
    }

    // Step 6 — SHA-256 structural integrity check (slow path).
    if sqlite_manifest.sha256 != kuzu_manifest.sha256 {
        let sqlite_sha = sqlite_manifest.sha256.clone();
        let kuzu_sha = kuzu_manifest.sha256.clone();
        cleanup_staging(&staging_dir);
        return Err(MigrationError::ManifestMismatch {
            sqlite_sha,
            kuzu_sha,
        });
    }

    // Step 7 — write manifest JSON atomically to the parent directory.
    // `kuzu_dir.parent()` is always `Some` for any non-root path; callers must
    // supply an absolute path with at least one parent component.
    let manifest_dir = kuzu_dir.parent().ok_or_else(|| {
        anyhow::anyhow!("kuzu_dir has no parent — must be an absolute path with a parent directory")
    })?;
    write_manifest_atomic(manifest_dir, &sqlite_manifest).context("writing migration manifest")?;

    // Step 8 — atomic rename: staging → final.
    // On POSIX, rename(2) is atomic on the same filesystem.
    std::fs::rename(&staging_dir, kuzu_dir).map_err(MigrationError::AtomicSwapFailed)?;

    Ok(kuzu_dir.to_path_buf())
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

#[cfg(feature = "kuzu")]
/// Write `manifest` as pretty JSON to `dir/migration_manifest.json` atomically.
///
/// Writes to a `.tmp` sibling first, then renames — same pattern as
/// `registry.rs`.  A crash between write and rename leaves a stale `.tmp` that
/// is harmless and cleaned up on the next attempt.
fn write_manifest_atomic(dir: &Path, manifest: &Manifest) -> anyhow::Result<()> {
    let json = serde_json::to_string_pretty(manifest).context("serialising manifest")?;
    let tmp = dir.join("migration_manifest.json.tmp");
    let dest = dir.join("migration_manifest.json");
    std::fs::write(&tmp, json.as_bytes()).context("writing manifest tmp file")?;
    std::fs::rename(&tmp, &dest).context("renaming manifest into place")?;
    Ok(())
}

/// Build the canonical byte string for `entries` + `isolated_nodes`, return
/// its SHA-256 as a lowercase hex string, and the sorted isolated-node ids.
///
/// Entries are sorted by `(src, dst, kind)` before hashing so the digest is
/// order-independent.  Format: `"{src},{dst},{kind}\n"` per edge;
/// `"node:{id}\n"` per isolated node.
///
/// The function takes ownership of both vectors to avoid a caller-side clone.
/// The sorted `isolated_nodes` is returned alongside the digest so callers can
/// embed it in the [`Manifest`] without re-sorting.
///
/// # Complexity
/// O(E log E + N) where E = edge count, N = isolated node count.
pub(crate) fn canonical_sha256(
    mut entries: Vec<ManifestEntry>,
    mut isolated_nodes: Vec<u64>,
) -> (String, Vec<u64>) {
    entries.sort_unstable();
    isolated_nodes.sort_unstable();

    let mut hasher = Sha256::new();
    for e in &entries {
        // Guard: EdgeKind enum values never contain commas or newlines.
        // A format-string injection via `kind` would allow two structurally
        // different graphs to produce the same digest.
        debug_assert!(
            !e.kind.contains(',') && !e.kind.contains('\n'),
            "edge kind must not contain commas or newlines: {:?}",
            e.kind
        );
        hasher.update(format!("{},{},{}\n", e.src, e.dst, e.kind).as_bytes());
    }
    for id in &isolated_nodes {
        hasher.update(format!("node:{id}\n").as_bytes());
    }
    let digest = hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut s, b| {
            use std::fmt::Write as _;
            write!(s, "{b:02x}").expect("writing to String is infallible");
            s
        });
    (digest, isolated_nodes)
}

/// Return the current UTC time as an ISO-8601 string (seconds precision).
///
/// Implemented using only `std` to avoid pulling in `chrono` or `time`.
/// Returns `"CLOCK_ERROR"` on systems with a clock set before the Unix epoch
/// (1970-01-01) — this is an explicit sentinel rather than a silent epoch
/// fallback so the metadata field is diagnosable.
fn utc_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs(),
        Err(_) => {
            tracing::warn!("SEC-008: system clock is before Unix epoch; using sentinel timestamp");
            return "CLOCK_ERROR".to_string();
        }
    };

    let tod = secs % 86400;
    let hh = tod / 3600;
    let mm = (tod % 3600) / 60;
    let ss = tod % 60;

    // Convert days-since-epoch to (year, month, day).
    let mut days = secs / 86400;
    let mut year = 1970u64;
    loop {
        let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
        let diy = if leap { 366 } else { 365 };
        if days < diy {
            break;
        }
        days -= diy;
        year += 1;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month: [u64; 12] = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1u64;
    for &dim in &days_in_month {
        if days < dim {
            break;
        }
        days -= dim;
        month += 1;
    }
    let day = days + 1;
    format!("{year:04}-{month:02}-{day:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store as _;
    use travsr_core::{Edge, EdgeKind, Node, VName};

    fn make_node(sig: &str) -> Node {
        Node::new(
            VName::new("corpus", "root", "src/foo.ts", "typescript", sig),
            "function",
        )
    }

    // -----------------------------------------------------------------------
    // Pure function tests — no store, no kuzu feature required.
    // -----------------------------------------------------------------------

    #[test]
    fn canonical_sha256_is_order_independent() {
        let e1 = ManifestEntry {
            src: 1,
            dst: 2,
            kind: "ref/call".to_string(),
        };
        let e2 = ManifestEntry {
            src: 3,
            dst: 4,
            kind: "depends".to_string(),
        };

        let (hash_ab, _) = canonical_sha256(vec![e1.clone(), e2.clone()], vec![]);
        let (hash_ba, _) = canonical_sha256(vec![e2, e1], vec![]);
        assert_eq!(hash_ab, hash_ba, "digest must be order-independent");
    }

    #[test]
    fn empty_graph_known_sha256() {
        // SHA-256 of the empty byte string is the well-known constant.
        let (digest, _) = canonical_sha256(vec![], vec![]);
        assert_eq!(
            digest, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "empty graph must produce SHA-256 of empty input"
        );
    }

    #[test]
    fn isolated_nodes_appear_in_manifest() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let a = make_node("fn:a");
        let b = make_node("fn:b");
        let c = make_node("fn:c"); // isolated — no incident edges
        store.put_node(&a).unwrap();
        store.put_node(&b).unwrap();
        store.put_node(&c).unwrap();
        store
            .put_edge(&Edge::new(a.id, b.id, EdgeKind::RefCall))
            .unwrap();

        let manifest = compute_manifest_sqlite(&store).unwrap();
        assert_eq!(manifest.isolated_node_ids.len(), 1);
        assert_eq!(manifest.isolated_node_ids[0], c.id.0);
    }

    #[test]
    fn manifest_json_round_trips() {
        let manifest = Manifest {
            produced_at: "2026-05-19T10:00:00Z".to_string(),
            source_backend: "sqlite".to_string(),
            node_count: 3,
            edge_count: 1,
            sha256: "abc123".to_string(),
            isolated_node_ids: vec![42, 99],
        };
        let json = serde_json::to_string(&manifest).unwrap();
        let back: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.source_backend, manifest.source_backend);
        assert_eq!(back.node_count, manifest.node_count);
        assert_eq!(back.edge_count, manifest.edge_count);
        assert_eq!(back.sha256, manifest.sha256);
        assert_eq!(back.isolated_node_ids, manifest.isolated_node_ids);
    }

    #[test]
    fn compute_manifest_sqlite_basic() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let a = make_node("fn:a");
        let b = make_node("fn:b");
        store.put_node(&a).unwrap();
        store.put_node(&b).unwrap();
        store
            .put_edge(&Edge::new(a.id, b.id, EdgeKind::Depends))
            .unwrap();

        let m = compute_manifest_sqlite(&store).unwrap();
        assert_eq!(m.node_count, 2);
        assert_eq!(m.edge_count, 1);
        assert_eq!(m.source_backend, "sqlite");
        assert!(!m.sha256.is_empty());
        assert!(m.isolated_node_ids.is_empty(), "both nodes have edges");
    }

    #[test]
    fn same_graph_same_sha256_regardless_of_insertion_order() {
        let mut store1 = SqliteStore::open_in_memory().unwrap();
        let mut store2 = SqliteStore::open_in_memory().unwrap();
        let a = make_node("fn:a");
        let b = make_node("fn:b");
        let c = make_node("fn:c");

        for s in [&mut store1, &mut store2] {
            s.put_node(&a).unwrap();
            s.put_node(&b).unwrap();
            s.put_node(&c).unwrap();
        }
        // Store 1: a→b then a→c
        store1
            .put_edge(&Edge::new(a.id, b.id, EdgeKind::RefCall))
            .unwrap();
        store1
            .put_edge(&Edge::new(a.id, c.id, EdgeKind::Depends))
            .unwrap();
        // Store 2: a→c then a→b (reversed)
        store2
            .put_edge(&Edge::new(a.id, c.id, EdgeKind::Depends))
            .unwrap();
        store2
            .put_edge(&Edge::new(a.id, b.id, EdgeKind::RefCall))
            .unwrap();

        let m1 = compute_manifest_sqlite(&store1).unwrap();
        let m2 = compute_manifest_sqlite(&store2).unwrap();
        assert_eq!(m1.sha256, m2.sha256);
    }

    // -----------------------------------------------------------------------
    // Kuzu-gated tests — compiled only with --features kuzu.
    // These require KuzuStore (issue #27 / branch feature/travsr-store-kuzu-s5-1)
    // to be present in the crate.
    // -----------------------------------------------------------------------

    #[cfg(feature = "kuzu")]
    #[test]
    fn manifest_matches_for_identical_graph() {
        use tempfile::tempdir;
        let tmp = tempdir().unwrap();
        let kuzu_dir = tmp.path().join("graph.kuzu");

        let mut sqlite = SqliteStore::open_in_memory().unwrap();
        let a = make_node("fn:a");
        let b = make_node("fn:b");
        let c = make_node("fn:c");
        sqlite.put_node(&a).unwrap();
        sqlite.put_node(&b).unwrap();
        sqlite.put_node(&c).unwrap();
        sqlite
            .put_edge(&Edge::new(a.id, b.id, EdgeKind::RefCall))
            .unwrap();
        sqlite
            .put_edge(&Edge::new(b.id, c.id, EdgeKind::Depends))
            .unwrap();

        let result = migrate_sqlite_to_kuzu(&sqlite, &kuzu_dir).unwrap();
        assert!(result.exists());

        let kuzu = crate::KuzuStore::open(&result).unwrap();
        let sm = compute_manifest_sqlite(&sqlite).unwrap();
        let km = compute_manifest_kuzu(&kuzu).unwrap();
        assert_eq!(sm.sha256, km.sha256);
        assert_eq!(sm.node_count, km.node_count);
        assert_eq!(sm.edge_count, km.edge_count);
    }

    #[cfg(feature = "kuzu")]
    #[test]
    fn atomic_swap_produces_final_path() {
        use tempfile::tempdir;
        let tmp = tempdir().unwrap();
        let kuzu_dir = tmp.path().join("graph.kuzu");

        let mut sqlite = SqliteStore::open_in_memory().unwrap();
        let a = make_node("fn:a");
        let b = make_node("fn:b");
        sqlite.put_node(&a).unwrap();
        sqlite.put_node(&b).unwrap();
        sqlite
            .put_edge(&Edge::new(a.id, b.id, EdgeKind::RefCall))
            .unwrap();

        let result = migrate_sqlite_to_kuzu(&sqlite, &kuzu_dir).unwrap();
        assert_eq!(result, kuzu_dir);
        assert!(kuzu_dir.exists(), "final kuzu dir must exist after swap");
        assert!(
            !tmp.path().join("graph.kuzu.new").exists(),
            "staging dir must be removed after successful swap"
        );
        let manifest_path = tmp.path().join("migration_manifest.json");
        assert!(manifest_path.exists(), "manifest JSON must be written");
        let raw = std::fs::read_to_string(&manifest_path).unwrap();
        let _: Manifest = serde_json::from_str(&raw).unwrap();
    }

    #[cfg(feature = "kuzu")]
    #[test]
    fn staging_leftover_cleaned_before_retry() {
        use tempfile::tempdir;
        let tmp = tempdir().unwrap();
        let kuzu_dir = tmp.path().join("graph.kuzu");
        let staging = tmp.path().join("graph.kuzu.new");
        // Simulate a previous crashed attempt by pre-creating the staging dir.
        std::fs::create_dir_all(&staging).unwrap();

        let mut sqlite = SqliteStore::open_in_memory().unwrap();
        let a = make_node("fn:a");
        sqlite.put_node(&a).unwrap();

        let result = migrate_sqlite_to_kuzu(&sqlite, &kuzu_dir);
        assert!(
            result.is_ok(),
            "migration must succeed despite leftover staging dir"
        );
    }
}
