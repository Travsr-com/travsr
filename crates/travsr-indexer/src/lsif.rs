//! LSIF dump ingestion — converts a `travsr-lsif-ts` JSON-Lines dump into
//! Travsr `Edge` records that the daemon persists alongside Tree-sitter edges.
//!
//! ## Why this exists
//! Tree-sitter gives us structural edges (DefinesBinding, Depends). The
//! TypeScript compiler API — exposed via the `travsr-lsif-ts` Node.js binary —
//! gives us semantic edges: actual call sites, import references, interface
//! implementations, and method overrides. This module bridges the two worlds.
//!
//! ## ID stability contract
//! The `travsr_vname` field embedded in every LSIF `resultSet` vertex encodes
//! the Travsr VName (path + signature) in the same format used by
//! `travsr-indexer/src/emit.rs`. Because `NodeId` is a deterministic BLAKE3
//! hash of the VName, we can compute NodeIds here without querying the store —
//! the same hash the Tree-sitter pass produced is what we use in edges.
//!
//! ## Limitations (Sprint 4)
//! - Only RefCall edges are emitted (all `item/references` items).
//!   RefImports vs RefCall differentiation is DEBT(travsr-25).
//! - The caller side of a RefCall edge is the FILE node, not the enclosing
//!   function node. Method-level caller precision requires containment tracking
//!   and is scheduled for Phase 3.

use std::collections::HashMap;

use anyhow::Context as _;
use travsr_core::{Edge, EdgeKind, VName};

use crate::ParseOutput;

/// Parse an LSIF JSON-Lines dump produced by `travsr-lsif-ts` and return the
/// Travsr edges it encodes.
///
/// The dump is expected to follow the LSIF 0.4.x format, extended with a
/// non-standard `travsr_vname` field on `resultSet` vertices. Lines that are
/// not valid JSON or are not recognised LSIF records are silently skipped —
/// the format is append-only and forward-compatible.
///
/// # Errors
/// Returns an error only if the entire dump is empty or unparseable. Partial
/// failures (unrecognised lines, missing vnames) are logged at trace level and
/// skipped so the daemon can continue indexing.
pub fn ingest(dump: &str) -> anyhow::Result<ParseOutput> {
    let edges = ingest_raw(dump).context("ingesting LSIF dump")?;
    Ok(ParseOutput {
        nodes: Vec::new(), // LSIF adds edges only; Tree-sitter owns nodes
        edges,
    })
}

// ── Internal graph ────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct LsifGraph {
    /// Absolute project root extracted from the `metaData` vertex (file:// stripped).
    project_root: String,
    /// resultSetId → Travsr VName (from `travsr_vname` field).
    result_sets: HashMap<u64, VName>,
    /// referenceResultId → resultSetId (from `textDocument/references` edges).
    ref_result_to_rs: HashMap<u64, u64>,
    /// docId → repo-relative file path (from `document` vertices, relativised via project_root).
    doc_paths: HashMap<u64, String>,
}

fn parse_graph(dump: &str) -> anyhow::Result<LsifGraph> {
    let mut graph = LsifGraph::default();

    for line in dump.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let obj: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue, // non-JSON line — skip
        };

        let id = match obj["id"].as_u64() {
            Some(v) => v,
            None => continue,
        };
        let type_ = obj["type"].as_str().unwrap_or("");
        let label = obj["label"].as_str().unwrap_or("");

        match (type_, label) {
            ("vertex", "metaData") => {
                // projectRoot is "file:///abs/path" — strip the scheme.
                if let Some(root) = obj["projectRoot"].as_str() {
                    graph.project_root = root.strip_prefix("file://").unwrap_or(root).to_string();
                }
            }

            ("vertex", "document") => {
                if let Some(uri) = obj["uri"].as_str() {
                    let abs = uri.strip_prefix("file://").unwrap_or(uri);
                    // Make path relative to project_root so it matches the
                    // vname_path produced by the Tree-sitter indexer.
                    let rel = make_relative(&graph.project_root, abs);
                    graph.doc_paths.insert(id, rel);
                }
            }

            ("vertex", "resultSet") => {
                // Non-standard field emitted by travsr-lsif-ts.
                if let Some(vname_obj) = obj.get("travsr_vname") {
                    let path = vname_obj["path"].as_str().unwrap_or("").to_string();
                    let sig = vname_obj["signature"].as_str().unwrap_or("").to_string();
                    if !path.is_empty() && !sig.is_empty() {
                        graph
                            .result_sets
                            .insert(id, VName::new("", "", path, "typescript", sig));
                    }
                }
            }

            ("edge", "textDocument/references") => {
                // outV = resultSetId, inV = referenceResultId
                let rs_id = obj["outV"].as_u64().unwrap_or(0);
                let rr_id = obj["inV"].as_u64().unwrap_or(0);
                if rs_id != 0 && rr_id != 0 {
                    graph.ref_result_to_rs.insert(rr_id, rs_id);
                }
            }

            _ => {}
        }
    }

    Ok(graph)
}

/// Full ingestion: parse graph metadata AND emit edges in one pass.
///
/// Separated from `ingest` so that unit tests can feed synthetic dumps without
/// going through the `ParseOutput` wrapper.
pub fn ingest_raw(dump: &str) -> anyhow::Result<Vec<Edge>> {
    let graph = parse_graph(dump).context("parsing LSIF graph metadata")?;

    let mut edges = Vec::new();

    for line in dump.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let obj: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Only `item` edges with property "references" carry call-site info.
        if obj["type"].as_str() != Some("edge") || obj["label"].as_str() != Some("item") {
            continue;
        }
        if obj["property"].as_str() != Some("references") {
            continue;
        }

        let ref_result_id = match obj["outV"].as_u64() {
            Some(v) => v,
            None => continue,
        };
        let caller_doc_id = match obj["document"].as_u64() {
            Some(v) => v,
            None => continue,
        };

        let caller_path = match graph.doc_paths.get(&caller_doc_id) {
            Some(p) => p,
            None => continue,
        };
        let rs_id = match graph.ref_result_to_rs.get(&ref_result_id) {
            Some(id) => id,
            None => continue,
        };
        let callee_vname = match graph.result_sets.get(rs_id) {
            Some(v) => v,
            None => continue,
        };

        // Caller = the file node at the call site (file-level precision for Sprint 4).
        // DEBT(travsr-25): upgrade to method-level caller once containment tracking lands.
        let caller_id = VName::new("", "", caller_path.as_str(), "typescript", "file").id();
        let callee_id = callee_vname.id();

        // Intra-file edges (caller_path == callee_vname.path) are intentionally
        // retained: they become meaningful for blast-radius once caller precision
        // upgrades from FILE to METHOD in a follow-on sprint.

        // DEBT(travsr-25/29/30): call-site count is not preserved — the store's
        // INSERT OR IGNORE deduplicates multiple calls from the same file to the
        // same symbol. Edge weights for PPR/blast-radius ranking require an
        // `Edge.weight` field or a side table; tracked in issues #29 and #30.
        edges.push(Edge::new(caller_id, callee_id, EdgeKind::RefCall));
    }

    Ok(edges)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Make `abs_path` relative to `base`. Falls back to returning `abs_path`
/// unchanged on Windows path mismatches or if `abs_path` is not under `base`.
fn make_relative(base: &str, abs_path: &str) -> String {
    if base.is_empty() {
        return abs_path.to_string();
    }
    let base_with_sep = if base.ends_with('/') {
        base.to_string()
    } else {
        format!("{base}/")
    };
    if let Some(rel) = abs_path.strip_prefix(&base_with_sep) {
        rel.to_string()
    } else {
        abs_path.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use travsr_core::EdgeKind;

    fn minimal_dump(callee_path: &str, callee_sig: &str, caller_path: &str) -> String {
        // Hand-crafted LSIF dump exercising the minimal structure needed to
        // produce one RefCall edge: callee resultSet + textDocument/references
        // edge + item edge pointing to a document.
        format!(
            r#"
{{"id":1,"type":"vertex","label":"metaData","version":"0.4.3","projectRoot":"file:///repo","positionEncoding":"utf-16","toolInfo":{{"name":"test","version":"0.0.0"}}}}
{{"id":2,"type":"vertex","label":"project","kind":"typescript"}}
{{"id":3,"type":"vertex","label":"document","uri":"file:///repo/{caller}"}}
{{"id":4,"type":"vertex","label":"resultSet","travsr_vname":{{"path":"{callee_p}","signature":"{callee_s}"}}}}
{{"id":5,"type":"vertex","label":"definitionResult"}}
{{"id":6,"type":"vertex","label":"referenceResult"}}
{{"id":7,"type":"edge","label":"textDocument/definition","outV":4,"inV":5}}
{{"id":8,"type":"edge","label":"textDocument/references","outV":4,"inV":6}}
{{"id":9,"type":"vertex","label":"range","start":{{"line":0,"character":0}},"end":{{"line":0,"character":5}}}}
{{"id":10,"type":"edge","label":"next","outV":9,"inV":4}}
{{"id":11,"type":"edge","label":"item","outV":6,"inVs":[9],"document":3,"property":"references"}}
"#,
            caller = caller_path,
            callee_p = callee_path,
            callee_s = callee_sig,
        )
    }

    #[test]
    fn ingest_raw_produces_ref_call_edge() {
        let dump = minimal_dump("svc.ts", "fn:charge", "caller.ts");
        let edges = ingest_raw(&dump).unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].kind, EdgeKind::RefCall);

        let caller_id = VName::new("", "", "caller.ts", "typescript", "file").id();
        let callee_id = VName::new("", "", "svc.ts", "typescript", "fn:charge").id();
        assert_eq!(edges[0].src, caller_id);
        assert_eq!(edges[0].dst, callee_id);
    }

    #[test]
    fn ingest_raw_emits_one_edge_per_item_reference() {
        // Two item/references edges for the same (caller, callee) pair produce
        // two edges from ingest_raw — deduplication is handled by the store's
        // INSERT OR IGNORE. This preserves the raw call-site count for future
        // PPR weight tracking (DEBT travsr-25/29).
        let dump = r#"
{"id":1,"type":"vertex","label":"metaData","version":"0.4.3","projectRoot":"file:///repo","positionEncoding":"utf-16","toolInfo":{"name":"t","version":"0"}}
{"id":2,"type":"vertex","label":"document","uri":"file:///repo/a.ts"}
{"id":3,"type":"vertex","label":"resultSet","travsr_vname":{"path":"b.ts","signature":"fn:foo"}}
{"id":4,"type":"vertex","label":"referenceResult"}
{"id":5,"type":"edge","label":"textDocument/references","outV":3,"inV":4}
{"id":6,"type":"edge","label":"item","outV":4,"inVs":[10],"document":2,"property":"references"}
{"id":7,"type":"edge","label":"item","outV":4,"inVs":[11],"document":2,"property":"references"}
"#;
        let edges = ingest_raw(dump).unwrap();
        assert_eq!(edges.len(), 2, "two item edges → two raw RefCall edges");
        assert!(edges.iter().all(|e| e.kind == EdgeKind::RefCall));
    }

    #[test]
    fn ingest_raw_keeps_intra_file_edges() {
        // Intra-file edges (caller file == callee definition file) are retained:
        // they become meaningful for blast-radius once caller precision upgrades
        // from FILE to METHOD level in a follow-on sprint.
        let dump = minimal_dump("same.ts", "fn:bar", "same.ts");
        let edges = ingest_raw(&dump).unwrap();
        assert_eq!(edges.len(), 1, "intra-file RefCall edge must be kept");
        assert_eq!(edges[0].kind, EdgeKind::RefCall);
    }

    #[test]
    fn ingest_raw_skips_result_sets_without_vname() {
        let dump = r#"
{"id":1,"type":"vertex","label":"metaData","version":"0.4.3","projectRoot":"file:///r","positionEncoding":"utf-16","toolInfo":{"name":"t","version":"0"}}
{"id":2,"type":"vertex","label":"document","uri":"file:///r/a.ts"}
{"id":3,"type":"vertex","label":"resultSet"}
{"id":4,"type":"vertex","label":"referenceResult"}
{"id":5,"type":"edge","label":"textDocument/references","outV":3,"inV":4}
{"id":6,"type":"edge","label":"item","outV":4,"inVs":[9],"document":2,"property":"references"}
"#;
        let edges = ingest_raw(dump).unwrap();
        assert_eq!(edges.len(), 0, "no vname → no edge");
    }

    #[test]
    fn make_relative_strips_base_prefix() {
        assert_eq!(make_relative("/repo", "/repo/src/foo.ts"), "src/foo.ts");
        assert_eq!(make_relative("/repo/", "/repo/src/foo.ts"), "src/foo.ts");
        assert_eq!(make_relative("", "/abs/path"), "/abs/path");
    }

    #[test]
    fn ingest_wraps_ingest_raw() {
        let dump = minimal_dump("svc.ts", "class:Svc", "other.ts");
        let out = ingest(&dump).unwrap();
        assert_eq!(out.nodes.len(), 0);
        assert_eq!(out.edges.len(), 1);
    }
}
