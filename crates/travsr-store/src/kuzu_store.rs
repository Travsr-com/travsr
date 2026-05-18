//! Kùzu storage backend — Production tier (< 2.5B edges).
//!
//! # Why Kùzu
//! SQLite is the MVP backend (< 75M nodes). Once a repo graph grows beyond
//! that threshold, Kùzu's native property-graph engine — 64× faster than
//! Neo4j on graph workloads — becomes the right choice. This module
//! implements the same `Store` trait behind a `kuzu` Cargo feature flag so
//! the rest of the codebase is unaffected until the feature is enabled.
//!
//! # Interior mutability
//! The `Store` trait requires `&self` for read operations
//! (`get_node`, `iter_edges_from`, `iter_edges_to`). Kùzu's
//! `Connection::prepare` and `Connection::execute` require `&mut Connection`.
//! We bridge the gap with `RefCell<Connection>` — safe because `KuzuStore`
//! is explicitly not `Sync` and the indexing pipeline is single-threaded.
//!
//! # Schema (Kùzu Cypher DDL)
//! ```cypher
//! CREATE NODE TABLE nodes(
//!     id INT64, corpus STRING, root STRING, path STRING,
//!     language STRING, signature STRING, kind STRING, PRIMARY KEY(id)
//! );
//! CREATE REL TABLE edges(FROM nodes TO nodes, kind STRING);
//! ```
//! `id` is the BLAKE3-derived `NodeId` reinterpreted as `i64` (bitcast of
//! the `u64`), identical to the SQLite backend's storage strategy.

use std::cell::RefCell;
use std::path::Path;

use anyhow::{bail, Context, Result};
use travsr_core::{Edge, EdgeKind, Node, NodeId, VName};

use crate::Store;

// ── Public type ───────────────────────────────────────────────────────────────

/// Kùzu-backed graph store. Not `Sync` — one instance per indexing thread.
pub struct KuzuStore {
    /// Kept alive so the Connection's internal C-level database pointer
    /// remains valid for the lifetime of the store.
    _db: kuzu::Database,
    /// `RefCell` enables `&self` read methods to obtain `&mut Connection`
    /// without requiring the `Store` trait to use `&mut self` for reads.
    conn: RefCell<kuzu::Connection>,
}

impl KuzuStore {
    /// Open (or create) a Kùzu database at `path` and initialise the schema.
    /// Safe to call on an already-initialised database — `init_schema` is
    /// idempotent.
    pub fn open(path: &Path) -> Result<Self> {
        let db = kuzu::Database::new(path, kuzu::SystemConfig::default())
            .with_context(|| format!("opening Kùzu database at {}", path.display()))?;
        let conn = kuzu::Connection::new(&db).context("creating Kùzu connection")?;
        let store = Self {
            _db: db,
            conn: RefCell::new(conn),
        };
        store.init_schema()?;
        Ok(store)
    }

    /// Create the node and edge tables. Kùzu has no `CREATE … IF NOT EXISTS`
    /// for node/rel tables, so we attempt each DDL statement and swallow
    /// "already exists" errors — propagating everything else.
    fn init_schema(&self) -> Result<()> {
        let ddl_stmts = [
            "CREATE NODE TABLE nodes(\
                id INT64, \
                corpus STRING, \
                root STRING, \
                path STRING, \
                language STRING, \
                signature STRING, \
                kind STRING, \
                PRIMARY KEY(id)\
            )",
            "CREATE REL TABLE edges(FROM nodes TO nodes, kind STRING)",
        ];

        for ddl in ddl_stmts {
            match self.conn.borrow_mut().query(ddl) {
                Ok(_) => {}
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("already exists") || msg.contains("has been created") {
                        continue;
                    }
                    return Err(e).context("Kùzu schema initialisation failed");
                }
            }
        }
        Ok(())
    }
}

// ── Store trait ───────────────────────────────────────────────────────────────

impl Store for KuzuStore {
    /// Upsert a node — creates it if absent, updates `kind` if already present.
    fn put_node(&mut self, node: &Node) -> Result<NodeId> {
        let mut conn = self.conn.borrow_mut();
        let mut stmt = conn
            .prepare(
                "MERGE (n:nodes {id: $id}) \
                 ON CREATE SET \
                     n.corpus = $corpus, n.root = $root, n.path = $path, \
                     n.language = $language, n.signature = $signature, n.kind = $kind \
                 ON MATCH SET n.kind = $kind",
            )
            .context("preparing put_node")?;
        conn.execute(
            &mut stmt,
            vec![
                ("id", kuzu::Value::Int64(node_id_to_i64(node.id))),
                ("corpus", kuzu::Value::String(node.vname.corpus.clone())),
                ("root", kuzu::Value::String(node.vname.root.clone())),
                ("path", kuzu::Value::String(node.vname.path.clone())),
                ("language", kuzu::Value::String(node.vname.language.clone())),
                (
                    "signature",
                    kuzu::Value::String(node.vname.signature.clone()),
                ),
                ("kind", kuzu::Value::String(node.kind.clone())),
            ],
        )
        .context("executing put_node")?;
        Ok(node.id)
    }

    /// Insert a directed edge. Idempotent — duplicate (src, dst, kind) tuples
    /// are silently ignored via MERGE.
    fn put_edge(&mut self, edge: &Edge) -> Result<()> {
        let mut conn = self.conn.borrow_mut();
        let mut stmt = conn
            .prepare(
                "MATCH (src:nodes {id: $src}), (dst:nodes {id: $dst}) \
                 MERGE (src)-[e:edges {kind: $kind}]->(dst)",
            )
            .context("preparing put_edge")?;
        conn.execute(
            &mut stmt,
            vec![
                ("src", kuzu::Value::Int64(node_id_to_i64(edge.src))),
                ("dst", kuzu::Value::Int64(node_id_to_i64(edge.dst))),
                ("kind", kuzu::Value::String(edge.kind.as_str().to_string())),
            ],
        )
        .context("executing put_edge")?;
        Ok(())
    }

    fn get_node(&self, id: NodeId) -> Result<Option<Node>> {
        let mut conn = self.conn.borrow_mut();
        let mut stmt = conn
            .prepare(
                "MATCH (n:nodes {id: $id}) \
                 RETURN n.corpus, n.root, n.path, n.language, n.signature, n.kind",
            )
            .context("preparing get_node")?;
        let mut result = conn
            .execute(
                &mut stmt,
                vec![("id", kuzu::Value::Int64(node_id_to_i64(id)))],
            )
            .context("executing get_node")?;

        let row = match result.next() {
            Some(r) => r,
            None => return Ok(None),
        };

        let corpus = extract_string(&row, 0, "corpus")?;
        let root = extract_string(&row, 1, "root")?;
        let path = extract_string(&row, 2, "path")?;
        let language = extract_string(&row, 3, "language")?;
        let signature = extract_string(&row, 4, "signature")?;
        let kind = extract_string(&row, 5, "kind")?;

        Ok(Some(Node {
            id,
            vname: VName::new(corpus, root, path, language, signature),
            kind,
        }))
    }

    /// O(out-degree(src))
    fn iter_edges_from(&self, src: NodeId) -> Result<Vec<Edge>> {
        let mut conn = self.conn.borrow_mut();
        let mut stmt = conn
            .prepare(
                "MATCH (s:nodes {id: $src})-[e:edges]->(d:nodes) \
                 RETURN d.id, e.kind",
            )
            .context("preparing iter_edges_from")?;
        let mut result = conn
            .execute(
                &mut stmt,
                vec![("src", kuzu::Value::Int64(node_id_to_i64(src)))],
            )
            .context("executing iter_edges_from")?;

        let mut edges = Vec::new();
        while let Some(row) = result.next() {
            let dst_raw = extract_i64(&row, 0, "d.id")?;
            let kind_str = extract_string(&row, 1, "e.kind")?;
            let kind = EdgeKind::from_str(&kind_str)
                .with_context(|| format!("unknown edge kind in Kùzu: {kind_str}"))?;
            edges.push(Edge::new(src, i64_to_node_id(dst_raw), kind));
        }
        Ok(edges)
    }

    /// O(in-degree(dst))
    fn iter_edges_to(&self, dst: NodeId) -> Result<Vec<Edge>> {
        let mut conn = self.conn.borrow_mut();
        let mut stmt = conn
            .prepare(
                "MATCH (s:nodes)-[e:edges]->(d:nodes {id: $dst}) \
                 RETURN s.id, e.kind",
            )
            .context("preparing iter_edges_to")?;
        let mut result = conn
            .execute(
                &mut stmt,
                vec![("dst", kuzu::Value::Int64(node_id_to_i64(dst)))],
            )
            .context("executing iter_edges_to")?;

        let mut edges = Vec::new();
        while let Some(row) = result.next() {
            let src_raw = extract_i64(&row, 0, "s.id")?;
            let kind_str = extract_string(&row, 1, "e.kind")?;
            let kind = EdgeKind::from_str(&kind_str)
                .with_context(|| format!("unknown edge kind in Kùzu: {kind_str}"))?;
            edges.push(Edge::new(i64_to_node_id(src_raw), dst, kind));
        }
        Ok(edges)
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn node_id_to_i64(id: NodeId) -> i64 {
    id.0 as i64
}

fn i64_to_node_id(v: i64) -> NodeId {
    NodeId(v as u64)
}

fn extract_string(row: &[Option<kuzu::Value>], idx: usize, field: &str) -> Result<String> {
    match row.get(idx).and_then(Option::as_ref) {
        Some(kuzu::Value::String(s)) => Ok(s.clone()),
        other => bail!("expected String for '{field}' at col {idx}, got {other:?}"),
    }
}

fn extract_i64(row: &[Option<kuzu::Value>], idx: usize, field: &str) -> Result<i64> {
    match row.get(idx).and_then(Option::as_ref) {
        Some(kuzu::Value::Int64(n)) => Ok(*n),
        other => bail!("expected Int64 for '{field}' at col {idx}, got {other:?}"),
    }
}

// ── Tests (ported from SqliteStore, run with --features kuzu) ─────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use travsr_core::{Edge, EdgeKind, VName};

    fn sample_node(sig: &str) -> Node {
        Node::new(
            VName::new("test-corpus", "main", "src/foo.ts", "typescript", sig),
            "function",
        )
    }

    fn open_store(dir: &Path) -> KuzuStore {
        KuzuStore::open(dir).expect("KuzuStore::open")
    }

    #[test]
    fn open_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        // Second open on same path must not fail — schema already exists.
        open_store(tmp.path());
        open_store(tmp.path());
    }

    #[test]
    fn put_and_get_node_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = open_store(tmp.path());
        let n = sample_node("fn:rt");
        let id = store.put_node(&n).unwrap();
        let back = store.get_node(id).unwrap().expect("node must exist");
        assert_eq!(back, n);
    }

    #[test]
    fn put_and_iter_edges() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = open_store(tmp.path());
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
        // Duplicate edge must be a no-op.
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
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        assert!(store.get_node(NodeId(123_456_789)).unwrap().is_none());
    }

    #[test]
    fn reopening_existing_db_keeps_data() {
        let tmp = tempfile::tempdir().unwrap();
        let n = sample_node("fn:persist");
        let id = {
            let mut store = open_store(tmp.path());
            store.put_node(&n).unwrap()
        };
        // Re-open — node must still be there.
        let store = open_store(tmp.path());
        assert_eq!(store.get_node(id).unwrap().as_ref(), Some(&n));
    }

    #[test]
    fn put_node_upsert_updates_kind() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = open_store(tmp.path());
        let mut n = sample_node("fn:upsert");
        store.put_node(&n).unwrap();
        n.kind = "class".to_string();
        store.put_node(&n).unwrap();
        let back = store.get_node(n.id).unwrap().expect("must exist");
        assert_eq!(back.kind, "class", "ON MATCH SET must update kind");
    }

    #[test]
    fn put_edge_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = open_store(tmp.path());
        let a = sample_node("fn:a3");
        let b = sample_node("fn:b3");
        store.put_node(&a).unwrap();
        store.put_node(&b).unwrap();
        store
            .put_edge(&Edge::new(a.id, b.id, EdgeKind::Depends))
            .unwrap();
        store
            .put_edge(&Edge::new(a.id, b.id, EdgeKind::Depends))
            .unwrap();
        let edges = store.iter_edges_from(a.id).unwrap();
        assert_eq!(
            edges.len(),
            1,
            "duplicate (src, dst, kind) must collapse to one edge"
        );
    }

    #[test]
    fn distinct_edge_kinds_between_same_nodes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = open_store(tmp.path());
        let a = sample_node("fn:ax");
        let b = sample_node("fn:bx");
        store.put_node(&a).unwrap();
        store.put_node(&b).unwrap();
        store
            .put_edge(&Edge::new(a.id, b.id, EdgeKind::Depends))
            .unwrap();
        store
            .put_edge(&Edge::new(a.id, b.id, EdgeKind::RefCall))
            .unwrap();
        let edges = store.iter_edges_from(a.id).unwrap();
        assert_eq!(
            edges.len(),
            2,
            "different kinds between same pair must both exist"
        );
    }

    #[test]
    fn iter_edges_to_returns_incoming_edges() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = open_store(tmp.path());
        let a = sample_node("fn:aa");
        let b = sample_node("fn:bb");
        let c = sample_node("fn:cc");
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
        assert_eq!(edges.len(), 2, "both A→C and B→C must appear");
        assert!(srcs.contains(&a.id), "A must appear as caller");
        assert!(srcs.contains(&b.id), "B must appear as caller");
        assert!(edges.iter().all(|e| e.dst == c.id));
    }

    #[test]
    fn iter_edges_from_empty_node_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = open_store(tmp.path());
        let a = sample_node("fn:lone");
        store.put_node(&a).unwrap();
        assert!(store.iter_edges_from(a.id).unwrap().is_empty());
    }

    #[test]
    fn all_edge_kinds_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = open_store(tmp.path());
        let a = sample_node("fn:kind-src");
        let b = sample_node("fn:kind-dst");
        store.put_node(&a).unwrap();
        store.put_node(&b).unwrap();
        for kind in [
            EdgeKind::Depends,
            EdgeKind::RefCall,
            EdgeKind::DefinesBinding,
            EdgeKind::Exports,
            EdgeKind::ResolvesTo,
        ] {
            store.put_edge(&Edge::new(a.id, b.id, kind)).unwrap();
        }
        let edges = store.iter_edges_from(a.id).unwrap();
        assert_eq!(edges.len(), 5);
        for kind in [
            EdgeKind::Depends,
            EdgeKind::RefCall,
            EdgeKind::DefinesBinding,
            EdgeKind::Exports,
            EdgeKind::ResolvesTo,
        ] {
            assert!(
                edges.iter().any(|e| e.kind == kind),
                "missing kind: {kind:?}"
            );
        }
    }
}
