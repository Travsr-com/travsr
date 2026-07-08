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
pub fn ingest(dump: &str, corpus: &str) -> anyhow::Result<ParseOutput> {
    let edges = ingest_raw(dump, corpus).context("ingesting LSIF dump")?;
    Ok(ParseOutput {
        nodes: Vec::new(), // LSIF adds edges only; Tree-sitter owns nodes
        edges,
        ffi_markers: Vec::new(),
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
    /// range-vertex id → 1-based `start.line` (DEBT travsr-126, issue #299).
    /// Populated so `ingest_g2` can recover the occurrence line of each
    /// `item/references` `inVs` entry instead of dropping it.
    range_lines: HashMap<u64, u32>,
}

fn parse_graph(dump: &str, corpus: &str) -> anyhow::Result<LsifGraph> {
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
                            .insert(id, VName::new(corpus, "", path, "typescript", sig));
                    }
                }
            }

            ("vertex", "range") => {
                // #299: record the 1-based start line so item/references inVs can
                // be resolved to occurrence lines. 0-based → +1, saturating.
                if let Some(l) = obj["start"]["line"].as_u64() {
                    graph.range_lines.insert(id, (l as u32).saturating_add(1));
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
pub fn ingest_raw(dump: &str, corpus: &str) -> anyhow::Result<Vec<Edge>> {
    let graph = parse_graph(dump, corpus).context("parsing LSIF graph metadata")?;

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
        let caller_id = VName::new(corpus, "", caller_path.as_str(), "typescript", "file").id();
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

/// Output of the occurrence-aware graph-LSIF ingest ([`ingest_g2`], issue #299).
///
/// Carries [`ScipRef`](travsr_core::ScipRef) occurrence records instead of
/// pre-built file-level edges, so the store's `write_scip_attributed_batch`
/// performs the same enclosing-function attribution + `edge_sites` write used
/// for SCIP-family languages. This is what makes `find_references` work for
/// TypeScript / JavaScript / Python.
#[derive(Debug, Default)]
pub struct LsifG2Output {
    /// Reference occurrences (`caller_path`, 1-based `caller_line`, `callee_id`)
    /// for G2 call-site attribution. `callee_id` is the callee's tree-sitter
    /// node id (built from the emitter's `travsr_vname` path + signature), so it
    /// reconciles directly with Phase A nodes — no alias pass required.
    pub refs: Vec<travsr_core::ScipRef>,
}

/// Occurrence-aware graph-LSIF ingestion (issue #299, resolves DEBT travsr-126).
///
/// Unlike [`ingest_raw`], which emits one file-level `RefCall` edge per
/// `item/references` edge and discards the `inVs` range ids, this walks each
/// `inVs` range to recover its `start.line` and emits one [`ScipRef`] per
/// occurrence. The caller routes these through `write_scip_attributed_batch`
/// which attributes each to its enclosing function and records an `edge_sites`
/// row — giving `find_references` exact `path:line` sites for the graph-LSIF
/// languages (TypeScript / JavaScript / Python).
///
/// O(N) over dump lines plus O(occurrences) for the emit pass.
pub fn ingest_g2(dump: &str, corpus: &str) -> anyhow::Result<LsifG2Output> {
    let graph = parse_graph(dump, corpus).context("parsing LSIF graph metadata")?;

    let mut out = LsifG2Output::default();

    for line in dump.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let obj: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

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
        let callee_id = callee_vname.id();

        // One occurrence per range id in `inVs`. Range ids whose vertex carried
        // no `start.line` are skipped (never fabricate a line). The store's
        // `edge_sites` PK dedups identical (caller-fn, callee, line) rows.
        let Some(in_vs) = obj["inVs"].as_array() else {
            continue;
        };
        for range in in_vs {
            let Some(range_id) = range.as_u64() else {
                continue;
            };
            let Some(&caller_line) = graph.range_lines.get(&range_id) else {
                continue;
            };
            out.refs.push(travsr_core::ScipRef {
                caller_path: caller_path.clone(),
                caller_line,
                callee_id,
            });
        }
    }

    Ok(out)
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
        let edges = ingest_raw(&dump, "").unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].kind, EdgeKind::RefCall);

        let caller_id = VName::new("", "", "caller.ts", "typescript", "file").id();
        let callee_id = VName::new("", "", "svc.ts", "typescript", "fn:charge").id();
        assert_eq!(edges[0].src, caller_id);
        assert_eq!(edges[0].dst, callee_id);
    }

    #[test]
    fn ingest_g2_recovers_occurrence_line_from_range() {
        // #299: the `range` vertex (start.line 0) must surface as caller_line 1,
        // and the ScipRef callee_id must match the callee's tree-sitter node id.
        let dump = minimal_dump("svc.ts", "fn:charge", "caller.ts");
        let out = ingest_g2(&dump, "").unwrap();
        assert_eq!(out.refs.len(), 1);
        assert_eq!(out.refs[0].caller_path, "caller.ts");
        assert_eq!(out.refs[0].caller_line, 1); // 0-based range line 0 → 1-based 1
        let callee_id = VName::new("", "", "svc.ts", "typescript", "fn:charge").id();
        assert_eq!(out.refs[0].callee_id, callee_id);
    }

    #[test]
    fn ingest_g2_emits_one_ref_per_inv_range() {
        // A single item edge with multiple inVs ranges must yield one ScipRef per
        // range at its own line — the DEBT-126 fix (was: one document-level edge).
        let dump = r#"
{"id":1,"type":"vertex","label":"metaData","version":"0.4.3","projectRoot":"file:///repo","positionEncoding":"utf-16","toolInfo":{"name":"t","version":"0"}}
{"id":2,"type":"vertex","label":"document","uri":"file:///repo/a.ts"}
{"id":3,"type":"vertex","label":"resultSet","travsr_vname":{"path":"b.ts","signature":"fn:foo"}}
{"id":4,"type":"vertex","label":"referenceResult"}
{"id":5,"type":"edge","label":"textDocument/references","outV":3,"inV":4}
{"id":9,"type":"vertex","label":"range","start":{"line":4,"character":2},"end":{"line":4,"character":5}}
{"id":10,"type":"vertex","label":"range","start":{"line":11,"character":0},"end":{"line":11,"character":3}}
{"id":11,"type":"edge","label":"item","outV":4,"inVs":[9,10],"document":2,"property":"references"}
"#;
        let out = ingest_g2(dump, "").unwrap();
        let mut lines: Vec<u32> = out.refs.iter().map(|r| r.caller_line).collect();
        lines.sort_unstable();
        assert_eq!(lines, vec![5, 12]); // 4→5, 11→12
        assert!(out.refs.iter().all(|r| r.caller_path == "a.ts"));
    }

    #[test]
    fn ingest_g2_skips_ranges_without_line() {
        // An inVs entry pointing at a missing range vertex is skipped, not faked.
        let dump = r#"
{"id":1,"type":"vertex","label":"metaData","version":"0.4.3","projectRoot":"file:///repo","positionEncoding":"utf-16","toolInfo":{"name":"t","version":"0"}}
{"id":2,"type":"vertex","label":"document","uri":"file:///repo/a.ts"}
{"id":3,"type":"vertex","label":"resultSet","travsr_vname":{"path":"b.ts","signature":"fn:foo"}}
{"id":4,"type":"vertex","label":"referenceResult"}
{"id":5,"type":"edge","label":"textDocument/references","outV":3,"inV":4}
{"id":11,"type":"edge","label":"item","outV":4,"inVs":[99],"document":2,"property":"references"}
"#;
        let out = ingest_g2(dump, "").unwrap();
        assert!(out.refs.is_empty());
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
        let edges = ingest_raw(dump, "").unwrap();
        assert_eq!(edges.len(), 2, "two item edges → two raw RefCall edges");
        assert!(edges.iter().all(|e| e.kind == EdgeKind::RefCall));
    }

    #[test]
    fn ingest_raw_keeps_intra_file_edges() {
        // Intra-file edges (caller file == callee definition file) are retained:
        // they become meaningful for blast-radius once caller precision upgrades
        // from FILE to METHOD level in a follow-on sprint.
        let dump = minimal_dump("same.ts", "fn:bar", "same.ts");
        let edges = ingest_raw(&dump, "").unwrap();
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
        let edges = ingest_raw(dump, "").unwrap();
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
        let out = ingest(&dump, "").unwrap();
        assert_eq!(out.nodes.len(), 0);
        assert_eq!(out.edges.len(), 1);
    }
}

// ── Rust LSIF ingestion (INDEX-212) ───────────────────────────────────────────
//
// Parses standard rust-analyzer LSIF 0.4.x dumps (no travsr_vname extension).
// Emits `RefCall` edges from `item/references` entries only.
// Nodes are never emitted — Tree-sitter owns structural node definitions.
//
// VName convention (Sprint 9):
//   Caller: VName { corpus, root="", path=<doc relative path>, language="rust",
//                   signature="file" }
//   Callee: VName { corpus, root="", path=<project_root>, language="rust",
//                   signature=<moniker identifier> }
//
// Sprint 10 will reconcile callee VNames with Tree-sitter signatures.

/// Intermediate graph built during a two-pass LSIF parse.
///
/// Pass 1 populates all fields by walking every line once.
/// Pass 2 resolves forward-referenced monikers (rust-analyzer can emit a
/// `moniker` edge before the moniker vertex it references).
#[derive(Debug, Default)]
struct RustLsifGraph {
    /// `file:///repo` → `"repo"` (stripped of `file://`).
    project_root: String,
    /// resultSet id → moniker identifier (e.g. `"simple::add"`).
    rs_monikers: HashMap<u64, String>,
    /// referenceResult id → resultSet id.
    ref_result_to_rs: HashMap<u64, u64>,
    /// document id → repo-relative file path.
    doc_paths: HashMap<u64, String>,
}

/// Parse a rust-analyzer LSIF dump into an intermediate graph.
///
/// Two-pass to handle forward-referenced monikers: rust-analyzer sometimes
/// emits a `moniker` edge (outV=resultSet, inV=moniker_vertex) before the
/// moniker vertex itself. Pass 1 builds what it can; pass 2 resolves
/// any pending entries.
fn parse_rust_graph(dump: &str) -> anyhow::Result<RustLsifGraph> {
    let mut g = RustLsifGraph::default();

    // (resultSet id, moniker vertex id) pairs encountered as edges but whose
    // moniker vertex had not yet appeared at edge-parse time.
    let mut pending: Vec<(u64, u64)> = Vec::new();

    for line in dump.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        match v.get("type").and_then(|t| t.as_str()) {
            Some("vertex") => match v.get("label").and_then(|l| l.as_str()) {
                Some("metaData") => {
                    if let Some(root) = v.get("projectRoot").and_then(|r| r.as_str()) {
                        g.project_root = root.strip_prefix("file://").unwrap_or(root).to_string();
                    }
                }
                Some("document") => {
                    if let (Some(id), Some(uri)) = (
                        v.get("id").and_then(|i| i.as_u64()),
                        v.get("uri").and_then(|u| u.as_str()),
                    ) {
                        let path = uri.strip_prefix("file://").unwrap_or(uri).to_string();
                        g.doc_paths.insert(id, path);
                    }
                }
                Some("moniker") => {
                    if let (Some(id), Some(ident)) = (
                        v.get("id").and_then(|i| i.as_u64()),
                        v.get("identifier").and_then(|i| i.as_str()),
                    ) {
                        // Resolve any pending forward-refs that were waiting for
                        // this moniker vertex (moniker edge arrived first).
                        for (rs_id, m_id) in &pending {
                            if *m_id == id {
                                g.rs_monikers.insert(*rs_id, ident.to_string());
                            }
                        }
                        // Store ident under sentinel key so moniker edges that
                        // arrive AFTER this vertex can look it up.
                        // Sentinel: u64::MAX - id avoids collision with resultSet
                        // ids, which are small sequential integers in all known
                        // rust-analyzer LSIF versions.
                        g.rs_monikers.insert(u64::MAX - id, ident.to_string());
                    }
                }
                Some("referenceResult") => {
                    // Nothing to store at vertex time; populated by edges below.
                    let _ = v.get("id");
                }
                _ => {}
            },
            Some("edge") => match v.get("label").and_then(|l| l.as_str()) {
                Some("moniker") => {
                    // outV = resultSet id, inV = moniker vertex id
                    if let (Some(rs_id), Some(m_id)) = (
                        v.get("outV").and_then(|i| i.as_u64()),
                        v.get("inV").and_then(|i| i.as_u64()),
                    ) {
                        // Check if the moniker vertex has already been seen.
                        let sentinel_key = u64::MAX - m_id;
                        if let Some(ident) = g.rs_monikers.remove(&sentinel_key) {
                            g.rs_monikers.insert(rs_id, ident);
                        } else {
                            // Moniker vertex not yet seen — defer.
                            pending.push((rs_id, m_id));
                        }
                    }
                }
                Some("textDocument/references") => {
                    // outV = resultSet id, inV = referenceResult id
                    if let (Some(rs_id), Some(rr_id)) = (
                        v.get("outV").and_then(|i| i.as_u64()),
                        v.get("inV").and_then(|i| i.as_u64()),
                    ) {
                        g.ref_result_to_rs.insert(rr_id, rs_id);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    // Pass 2: resolve remaining forward-referenced monikers.
    // (Any pending entries whose moniker vertex arrived after the moniker edge
    // AND before pass 2 are already resolved above. This handles the edge case
    // where the moniker vertex never appeared — those are silently dropped.)
    for (rs_id, m_id) in &pending {
        let sentinel_key = u64::MAX - m_id;
        if let Some(ident) = g.rs_monikers.remove(&sentinel_key) {
            g.rs_monikers.insert(*rs_id, ident);
        }
    }

    // Clean up any remaining sentinel entries.
    g.rs_monikers.retain(|k, _| *k < u64::MAX / 2);

    Ok(g)
}

/// Walk all `item` edges and emit `RefCall` edges for `property: "references"`.
fn ingest_rust_edges_from_dump(dump: &str, corpus: &str) -> Vec<Edge> {
    let g = match parse_rust_graph(dump) {
        Ok(g) => g,
        Err(_) => return Vec::new(),
    };

    let mut edges = Vec::new();

    for line in dump.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if v.get("type").and_then(|t| t.as_str()) != Some("edge") {
            continue;
        }
        if v.get("label").and_then(|l| l.as_str()) != Some("item") {
            continue;
        }
        if v.get("property").and_then(|p| p.as_str()) != Some("references") {
            continue;
        }

        let rr_id = match v.get("outV").and_then(|i| i.as_u64()) {
            Some(id) => id,
            None => continue,
        };
        let doc_id = match v.get("document").and_then(|i| i.as_u64()) {
            Some(id) => id,
            None => continue,
        };

        let rs_id = match g.ref_result_to_rs.get(&rr_id) {
            Some(id) => *id,
            None => continue,
        };
        let moniker_ident = match g.rs_monikers.get(&rs_id) {
            Some(m) => m,
            None => continue,
        };
        let doc_path = match g.doc_paths.get(&doc_id) {
            Some(p) => p,
            None => continue,
        };

        // DEBT(travsr-126): `inVs` may contain multiple reference ranges
        // (e.g. `"inVs": [9, 10, 11]`). We emit one RefCall edge per item
        // edge regardless of inVs count — document-level caller precision.
        // Method-level precision requires range-to-node containment tracking,
        // deferred to Sprint 10.

        // Caller: the file containing the reference site.
        let caller_path = make_relative(&g.project_root, doc_path);
        let caller = VName::new(corpus, "", caller_path, "rust", "file");

        // Callee: the symbol identified by the moniker.
        let callee = VName::new(corpus, "", &g.project_root, "rust", moniker_ident);

        edges.push(Edge::new(caller.id(), callee.id(), EdgeKind::RefCall));
    }

    edges
}

/// Parse a rust-analyzer LSIF dump and return Travsr graph records.
///
/// Only `RefCall` edges are emitted. Nodes are not emitted — Tree-sitter owns
/// structural node definitions (ADR-002). The `corpus` is stamped into every
/// caller VName so edges from different repos cannot collide.
///
/// # Errors
/// Returns an error only if the dump cannot be parsed at all. Individual
/// unrecognised lines are silently skipped for forward-compatibility.
pub fn ingest_rust(dump: &str, corpus: &str) -> anyhow::Result<ParseOutput> {
    let edges = ingest_rust_raw(dump, corpus)?;
    Ok(ParseOutput {
        nodes: Vec::new(),
        edges,
        ffi_markers: Vec::new(),
    })
}

/// Return only the `Edge` vec from a rust-analyzer LSIF dump.
///
/// Useful in tests that want to inspect edges directly without the
/// `ParseOutput` wrapper.
pub fn ingest_rust_raw(dump: &str, corpus: &str) -> anyhow::Result<Vec<Edge>> {
    Ok(ingest_rust_edges_from_dump(dump, corpus))
}

// ── Rust LSIF unit tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod rust_lsif_tests {
    use super::*;
    use travsr_core::EdgeKind;

    /// Minimal rust-analyzer LSIF dump with one symbol and one reference.
    fn rust_dump_one_ref() -> &'static str {
        r#"
{"id":1,"type":"vertex","label":"metaData","version":"0.4.3","projectRoot":"file:///r","positionEncoding":"utf-16","toolInfo":{"name":"rust-analyzer","version":"0"}}
{"id":2,"type":"vertex","label":"document","uri":"file:///r/src/main.rs","languageId":"rust"}
{"id":3,"type":"vertex","label":"resultSet"}
{"id":4,"type":"vertex","label":"moniker","scheme":"rust-analyzer","identifier":"crate::foo","unique":"workspace","kind":"export"}
{"id":5,"type":"edge","label":"moniker","outV":3,"inV":4}
{"id":6,"type":"vertex","label":"referenceResult"}
{"id":7,"type":"edge","label":"textDocument/references","outV":3,"inV":6}
{"id":8,"type":"edge","label":"item","outV":6,"inVs":[9],"document":2,"property":"references"}
"#
    }

    #[test]
    fn ingest_rust_raw_produces_ref_call_edges() {
        let edges = ingest_rust_raw(rust_dump_one_ref(), "corp").unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].kind, EdgeKind::RefCall);
    }

    #[test]
    fn ingest_rust_skips_non_references_items() {
        let dump = r#"
{"id":1,"type":"vertex","label":"metaData","version":"0.4.3","projectRoot":"file:///r","positionEncoding":"utf-16","toolInfo":{"name":"rust-analyzer","version":"0"}}
{"id":2,"type":"vertex","label":"document","uri":"file:///r/src/lib.rs","languageId":"rust"}
{"id":3,"type":"vertex","label":"resultSet"}
{"id":4,"type":"vertex","label":"moniker","scheme":"rust-analyzer","identifier":"crate::Foo","unique":"workspace","kind":"export"}
{"id":5,"type":"edge","label":"moniker","outV":3,"inV":4}
{"id":6,"type":"vertex","label":"referenceResult"}
{"id":7,"type":"edge","label":"textDocument/references","outV":3,"inV":6}
{"id":8,"type":"edge","label":"item","outV":6,"inVs":[9],"document":2,"property":"definitions"}
"#;
        let edges = ingest_rust_raw(dump, "corp").unwrap();
        assert_eq!(
            edges.len(),
            0,
            "definitions-only item edges must not emit RefCall"
        );
    }

    #[test]
    fn ingest_rust_wraps_raw() {
        let out = ingest_rust(rust_dump_one_ref(), "corp").unwrap();
        assert_eq!(out.nodes.len(), 0, "Rust LSIF must not emit nodes");
        assert_eq!(out.edges.len(), 1);
    }

    #[test]
    fn ingest_rust_raw_is_idempotent() {
        let mut first = ingest_rust_raw(rust_dump_one_ref(), "corp").unwrap();
        let mut second = ingest_rust_raw(rust_dump_one_ref(), "corp").unwrap();
        first.sort_by_key(|e| (e.src, e.dst, e.kind as u8));
        second.sort_by_key(|e| (e.src, e.dst, e.kind as u8));
        assert_eq!(first, second, "repeated ingest must be idempotent");
    }

    #[test]
    fn ingest_rust_raw_empty_dump_returns_empty_vec() {
        // Contract: an empty or whitespace-only dump must return Ok(vec![]),
        // never Err. Pinned so future header-validation changes don't silently
        // break callers that feed empty dumps (e.g. crates with no public API).
        let edges = ingest_rust_raw("", "corp").unwrap();
        assert!(edges.is_empty(), "empty dump must return Ok(vec![])");

        let edges_ws = ingest_rust_raw("   \n  \n", "corp").unwrap();
        assert!(
            edges_ws.is_empty(),
            "whitespace-only dump must return Ok(vec![])"
        );
    }

    #[test]
    fn ingest_rust_handles_forward_referenced_monikers() {
        // moniker edge appears BEFORE the moniker vertex — must still resolve.
        let dump = r#"
{"id":1,"type":"vertex","label":"metaData","version":"0.4.3","projectRoot":"file:///r","positionEncoding":"utf-16","toolInfo":{"name":"rust-analyzer","version":"0"}}
{"id":2,"type":"vertex","label":"document","uri":"file:///r/src/main.rs","languageId":"rust"}
{"id":3,"type":"vertex","label":"resultSet"}
{"id":5,"type":"edge","label":"moniker","outV":3,"inV":4}
{"id":4,"type":"vertex","label":"moniker","scheme":"rust-analyzer","identifier":"crate::bar","unique":"workspace","kind":"export"}
{"id":6,"type":"vertex","label":"referenceResult"}
{"id":7,"type":"edge","label":"textDocument/references","outV":3,"inV":6}
{"id":8,"type":"edge","label":"item","outV":6,"inVs":[9],"document":2,"property":"references"}
"#;
        let edges = ingest_rust_raw(dump, "corp").unwrap();
        assert_eq!(
            edges.len(),
            1,
            "forward-referenced moniker must still produce an edge"
        );
    }
}

// ── SCIP ingestion ────────────────────────────────────────────────────────────

/// Ingest a raw SCIP protobuf index (produced by `scip-python`, `scip-go`, etc.)
/// and return Travsr nodes + edges.
///
/// Pass 1: collect definition occurrences → `Node` records.
/// Pass 2: collect reference occurrences → `RefCall` edges (file node → definition node).
///
/// Nodes use VName signatures taken verbatim from SCIP symbol strings so they are
/// stable across re-runs.  Edges whose target symbol was not seen as a definition in
/// this index are silently dropped (the symbol lives in another corpus/package).
pub fn ingest_scip(bytes: &[u8], corpus: &str) -> anyhow::Result<ParseOutput> {
    use protobuf::Message as _;
    use std::collections::HashMap;
    use travsr_core::{Edge, EdgeKind, Node, VName};

    let index = scip::types::Index::parse_from_bytes(bytes).context("SCIP protobuf parse")?;

    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();
    // SCIP symbol string → NodeId for fast edge construction.
    let mut defs: HashMap<String, travsr_core::NodeId> = HashMap::new();

    // Pass 1: definition occurrences → nodes.
    for doc in &index.documents {
        let path = &doc.relative_path;
        for occ in &doc.occurrences {
            // SymbolRole::Definition = 1 (bit flag per SCIP proto).
            if occ.symbol_roles & 1 != 0 && !occ.symbol.is_empty() {
                // G3: skip SCIP anonymous locals at ingest — they are intra-function
                // SSA temporaries with zero developer-facing signal (RFC-014 G3).
                if travsr_core::is_scip_anonymous_local(&occ.symbol) {
                    continue;
                }
                let vname = VName::new(corpus, "", path.as_str(), "python", occ.symbol.as_str());
                let id = vname.id();
                // range: [start_line, start_col, end_col] (3-elem) or
                //        [start_line, start_col, end_line, end_col] (4-elem).
                // Clamp negatives before the i32→u32 cast so corrupt SCIP
                // input can neither overflow-panic (debug) nor become a huge
                // line number (release); then saturate the +1.
                let line = occ
                    .range
                    .first()
                    .copied()
                    .map(|l| (l.max(0) as u32).saturating_add(1));
                let end_line = if occ.range.len() >= 4 {
                    Some((occ.range[2].max(0) as u32).saturating_add(1))
                } else {
                    None
                };
                let mut node = Node::new(vname, scip_symbol_kind(&occ.symbol));
                if let Some(l) = line {
                    node = node.with_line(l);
                }
                if let Some(el) = end_line {
                    node = node.with_end_line(el);
                }
                // Noise-path guard: reject OS/build-cache nodes before they enter
                // the graph (durable fix — prevents hub-node PPR contamination).
                if travsr_core::is_noise_node(&node) {
                    continue;
                }
                defs.insert(occ.symbol.clone(), id);
                nodes.push(node);
            }
        }
    }

    // Pass 2: reference occurrences → RefCall edges.
    for doc in &index.documents {
        let path = &doc.relative_path;
        // File-level source node — same VName as tree-sitter emits for Python files.
        let file_id = VName::new(corpus, "", path.as_str(), "python", "").id();
        for occ in &doc.occurrences {
            if occ.symbol_roles & 1 == 0 && !occ.symbol.is_empty() {
                // G3: also skip references to anonymous locals.
                if travsr_core::is_scip_anonymous_local(&occ.symbol) {
                    continue;
                }
                if let Some(&dst) = defs.get(&occ.symbol) {
                    edges.push(Edge::new(file_id, dst, EdgeKind::RefCall));
                }
            }
        }
    }

    Ok(ParseOutput {
        nodes,
        edges,
        ffi_markers: vec![],
    })
}

/// Output from [`ingest_scip_g2`] — nodes plus attribution-ready reference data.
#[derive(Debug, Default)]
pub struct ScipIngestOutput {
    /// SCIP definition nodes (G3-filtered, with end_line where available).
    pub nodes: Vec<travsr_core::Node>,
    /// Reference occurrences for G2 call-site attribution.
    pub refs: Vec<travsr_core::ScipRef>,
    /// SCIP symbol string → NodeId (for G1 alias registration).
    pub symbol_map: HashMap<String, travsr_core::NodeId>,
}

/// G2-aware SCIP ingestion: returns nodes + [`ScipRef`] records instead of
/// pre-built edges.  The caller passes these to
/// [`SqliteStore::write_scip_attributed_batch`] which performs span lookup
/// and emits function-level `ref/call` edges.
///
/// Language string is caller-supplied (e.g. `"go"`, `"python"`) so the same
/// function handles all SCIP-emitting languages.
pub fn ingest_scip_g2(
    bytes: &[u8],
    corpus: &str,
    language: &str,
) -> anyhow::Result<ScipIngestOutput> {
    use protobuf::Message as _;

    let index = scip::types::Index::parse_from_bytes(bytes).context("SCIP protobuf parse")?;

    let mut out = ScipIngestOutput::default();

    // Pass 1: definition occurrences → nodes + symbol_map.
    for doc in &index.documents {
        let path = &doc.relative_path;
        for occ in &doc.occurrences {
            if occ.symbol_roles & 1 == 0 || occ.symbol.is_empty() {
                continue;
            }
            // G3: skip anonymous locals.
            if travsr_core::is_scip_anonymous_local(&occ.symbol) {
                continue;
            }
            let vname =
                travsr_core::VName::new(corpus, "", path.as_str(), language, occ.symbol.as_str());
            let id = vname.id();
            // Clamp negatives before the i32→u32 cast (corrupt SCIP input),
            // then saturate the +1 — see ingest_scip for rationale.
            let line = occ
                .range
                .first()
                .copied()
                .map(|l| (l.max(0) as u32).saturating_add(1));
            // 4-element range encodes a multi-line span; 3-element is single-line.
            let end_line = if occ.range.len() >= 4 {
                Some((occ.range[2].max(0) as u32).saturating_add(1))
            } else {
                None
            };
            let mut node = travsr_core::Node::new(vname, scip_symbol_kind(&occ.symbol));
            if let Some(l) = line {
                node = node.with_line(l);
            }
            if let Some(el) = end_line {
                node = node.with_end_line(el);
            }
            // Noise-path guard: mirror the ingest_scip G3 extension to g2 path.
            if travsr_core::is_noise_node(&node) {
                continue;
            }
            out.symbol_map.insert(occ.symbol.clone(), id);
            out.nodes.push(node);
        }
    }

    // Pass 2: reference occurrences → ScipRef records for G2 attribution.
    for doc in &index.documents {
        let path = &doc.relative_path;
        for occ in &doc.occurrences {
            if occ.symbol_roles & 1 != 0 || occ.symbol.is_empty() {
                continue;
            }
            if travsr_core::is_scip_anonymous_local(&occ.symbol) {
                continue;
            }
            if let Some(&callee_id) = out.symbol_map.get(&occ.symbol) {
                let caller_line = occ
                    .range
                    .first()
                    .copied()
                    .map(|l| (l.max(0) as u32).saturating_add(1))
                    .unwrap_or(1);
                out.refs.push(travsr_core::ScipRef {
                    caller_path: path.clone(),
                    caller_line,
                    callee_id,
                });
            }
        }
    }

    Ok(out)
}

/// Heuristic: derive a Travsr node-kind string from a SCIP symbol descriptor.
///
/// SCIP descriptor suffixes (per the spec):
///   `().` → method / function
///   `#`   → type / class member
///   `.`   → term / variable / property
///   `:`   → meta / annotation
fn scip_symbol_kind(symbol: &str) -> &'static str {
    if symbol.ends_with("().") || symbol.ends_with("()") {
        "fn"
    } else if symbol.contains('#') {
        "method"
    } else if symbol.ends_with('.') {
        "var"
    } else {
        "symbol"
    }
}
