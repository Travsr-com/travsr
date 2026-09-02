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
use travsr_core::{Edge, EdgeKind, Language, VName};

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
        workspace_dep_markers: Vec::new(),
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
    /// `item property:"references"` edges buffered during the single streaming
    /// pass (#736 item 7). The emit phases used to re-walk the raw dump for
    /// these — impossible on a streaming reader, and the reason callers had to
    /// materialize the whole dump as one `String`. Each record is a few dozen
    /// bytes vs the ~100+-byte JSON line it came from, and they are part of
    /// the same irreducible working set as the side tables above.
    ref_items: Vec<RefItem>,
}

/// One `item` edge with `property:"references"` — the minimal fields the emit
/// phases ([`ingest_raw_from_reader`], [`ingest_g2_from_reader`]) need.
#[derive(Debug)]
struct RefItem {
    /// `outV` — the `referenceResult` vertex id.
    ref_result_id: u64,
    /// `document` — id of the document containing the occurrence(s).
    caller_doc_id: u64,
    /// `inVs` — range vertex ids of the individual occurrences.
    range_ids: Vec<u64>,
}

fn parse_graph(dump: impl std::io::BufRead, corpus: &str) -> anyhow::Result<LsifGraph> {
    let mut graph = LsifGraph::default();

    for line in dump.lines() {
        let line = line.context("reading LSIF dump line")?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let obj: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue, // non-JSON line — skip
        };

        // Vertex arms key their side tables by `id`; edge arms are keyed by
        // outV/inV (plus `document` for item edges), so `id` is optional here.
        let id = obj["id"].as_u64();
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
                if let (Some(id), Some(uri)) = (id, obj["uri"].as_str()) {
                    let abs = uri.strip_prefix("file://").unwrap_or(uri);
                    // Make path relative to project_root so it matches the
                    // vname_path produced by the Tree-sitter indexer.
                    let rel = make_relative(&graph.project_root, abs);
                    graph.doc_paths.insert(id, rel);
                }
            }

            ("vertex", "resultSet") => {
                let Some(id) = id else {
                    continue;
                };
                // Non-standard field emitted by the travsr LSIF emitters
                // (travsr-lsif-ts, travsr-lsif-py).
                if let Some(vname_obj) = obj.get("travsr_vname") {
                    let path = vname_obj["path"].as_str().unwrap_or("").to_string();
                    let sig = vname_obj["signature"].as_str().unwrap_or("").to_string();
                    if !path.is_empty() && !sig.is_empty() {
                        // #299 P1: derive the node language from the file extension
                        // (not a hardcoded "typescript") so the resultSet VName id
                        // matches the Phase A node — otherwise Python (.py) refs
                        // computed a `typescript`-tagged id that matched no node and
                        // orphaned every occurrence. Mirrors Phase A's own
                        // extension→language mapping. Falls back to typescript for
                        // the graph-LSIF's original TS/JS producer.
                        let lang = std::path::Path::new(&path)
                            .extension()
                            .and_then(|e| e.to_str())
                            .and_then(Language::from_extension)
                            .map(Language::as_str)
                            .unwrap_or("typescript");
                        graph
                            .result_sets
                            .insert(id, VName::new(corpus, "", path, lang, sig));
                    }
                }
            }

            ("vertex", "range") => {
                // #299: record the 1-based start line so item/references inVs can
                // be resolved to occurrence lines. 0-based → +1, saturating.
                if let (Some(id), Some(l)) = (id, obj["start"]["line"].as_u64()) {
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

            ("edge", "item") => {
                // Only `item` edges with property "references" carry call-site
                // info — buffer them for the single-pass emit phases (#736).
                if obj["property"].as_str() != Some("references") {
                    continue;
                }
                let (Some(ref_result_id), Some(caller_doc_id)) =
                    (obj["outV"].as_u64(), obj["document"].as_u64())
                else {
                    continue;
                };
                let range_ids = obj["inVs"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|r| r.as_u64()).collect())
                    .unwrap_or_default();
                graph.ref_items.push(RefItem {
                    ref_result_id,
                    caller_doc_id,
                    range_ids,
                });
            }

            _ => {}
        }
    }

    Ok(graph)
}

/// Full ingestion: parse graph metadata AND emit edges in one pass.
///
/// Separated from `ingest` so that unit tests can feed synthetic dumps without
/// going through the `ParseOutput` wrapper. Thin wrapper over
/// [`ingest_raw_from_reader`] for callers that already hold the dump in memory.
pub fn ingest_raw(dump: &str, corpus: &str) -> anyhow::Result<Vec<Edge>> {
    ingest_raw_from_reader(std::io::Cursor::new(dump), corpus)
}

/// Streaming variant of [`ingest_raw`] (#736 item 7): consumes the LSIF
/// JSON-Lines dump incrementally from any `BufRead` — a capped child-stdout
/// buffer, a file, an in-memory cursor — so the raw dump text never has to be
/// held as one `String` alongside the side tables. The side tables (and the
/// buffered `item/references` records) are the irreducible working set.
pub fn ingest_raw_from_reader(
    dump: impl std::io::BufRead,
    corpus: &str,
) -> anyhow::Result<Vec<Edge>> {
    let graph = parse_graph(dump, corpus).context("parsing LSIF graph metadata")?;

    let mut edges = Vec::new();

    for item in &graph.ref_items {
        let caller_path = match graph.doc_paths.get(&item.caller_doc_id) {
            Some(p) => p,
            None => continue,
        };
        let rs_id = match graph.ref_result_to_rs.get(&item.ref_result_id) {
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
///
/// Thin wrapper over [`ingest_g2_from_reader`] for callers that already hold
/// the dump in memory.
pub fn ingest_g2(dump: &str, corpus: &str) -> anyhow::Result<LsifG2Output> {
    ingest_g2_from_reader(std::io::Cursor::new(dump), corpus)
}

/// Streaming variant of [`ingest_g2`] (#736 item 7): consumes the LSIF
/// JSON-Lines dump incrementally from any `BufRead` so the raw dump text never
/// has to be held as one `String` alongside the side tables.
pub fn ingest_g2_from_reader(
    dump: impl std::io::BufRead,
    corpus: &str,
) -> anyhow::Result<LsifG2Output> {
    let graph = parse_graph(dump, corpus).context("parsing LSIF graph metadata")?;

    let mut out = LsifG2Output::default();

    for item in &graph.ref_items {
        let caller_path = match graph.doc_paths.get(&item.caller_doc_id) {
            Some(p) => p,
            None => continue,
        };
        let rs_id = match graph.ref_result_to_rs.get(&item.ref_result_id) {
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
        for range_id in &item.range_ids {
            let Some(&caller_line) = graph.range_lines.get(range_id) else {
                continue;
            };
            out.refs.push(travsr_core::ScipRef {
                caller_path: caller_path.clone(),
                caller_line,
                callee_id,
                // Our bundled LSIF emitters (travsr-lsif-ts / -py) emit occurrence
                // ranges only for call expressions and imports, so these are
                // already call-scoped — flag as calls to preserve their edges.
                is_call: true,
                // RFC-027 #813 P2: this streaming graph parse carries no source
                // text, so the range's UTF-16 start character cannot be converted
                // to the byte column the occurrence store keeps. Leave it None
                // rather than record a wrong (non-ASCII) column; the daemon falls
                // back to its word-boundary search for these occurrences.
                caller_col: None,
            });
        }
    }

    Ok(out)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Make `abs_path` relative to `base`, returning a forward-slash repo-relative
/// path that matches the Tree-sitter indexer's `Node::vname.path`.
///
/// # Windows (issue #738)
///
/// The two operands arrive in different shapes on Windows and a naive
/// `strip_prefix` never matches, so every ref was dropped fail-closed:
/// - `base` is the daemon's `repo_root.to_string_lossy()`, an extended-length
///   backslash path: `\\?\D:\com.travsr\travsr`.
/// - `abs_path` comes from a rust-analyzer `file:///D:/...` URI decoded by
///   [`file_uri_to_path`] to a forward-slash, drive-letter path: `D:/com.travsr/...`.
///
/// [`normalize_path`] both operands before comparing: strip the `\\?\` /
/// `\\?\UNC\` verbatim prefix, unify separators to `/`, and lowercase a leading
/// drive letter (Windows drives are case-insensitive — rust-analyzer may report
/// `d:/` while `repo_root` is `\\?\D:\`). The comparison itself stays a plain,
/// case-sensitive `strip_prefix`, so on a Unix path — which has no drive letter
/// and no backslashes — normalization is a no-op and behaviour is byte-for-byte
/// identical to before (no cross-platform regression).
///
/// Falls back to the normalized `abs_path` when it is not under `base`.
fn make_relative(base: &str, abs_path: &str) -> String {
    if base.is_empty() {
        return abs_path.to_string();
    }
    relative_to_base(&normalized_base_prefix(base), abs_path)
}

/// Normalize `base` (see [`normalize_path`]) and ensure a single trailing `/`,
/// yielding a reusable strip prefix. Hoist this out of a loop that relativizes
/// many paths against one base (e.g. [`ingest_rust_positional`]) so the base is
/// normalized once per pass rather than once per emitted ref.
fn normalized_base_prefix(base: &str) -> String {
    let n = normalize_path(base);
    if n.ends_with('/') {
        n
    } else {
        format!("{n}/")
    }
}

/// Relativize `abs_path` against an already-[`normalized_base_prefix`], returning
/// a forward-slash repo-relative path. Falls back to the normalized `abs_path`
/// when it is not under the base.
fn relative_to_base(base_with_sep: &str, abs_path: &str) -> String {
    let abs_n = normalize_path(abs_path);
    match abs_n.strip_prefix(base_with_sep) {
        Some(rel) => rel.to_string(),
        None => abs_n,
    }
}

/// Normalize a path for prefix comparison: strip a Windows extended-length
/// prefix (`\\?\`, `\\?\UNC\`), unify separators to `/`, and lowercase a leading
/// `X:` drive letter. A Unix path (no drive letter, no backslashes) is returned
/// unchanged, so downstream comparison is unaffected off Windows.
fn normalize_path(p: &str) -> String {
    // Strip the Windows extended-length prefix in BOTH slash shapes. TypeScript
    // normalizes paths to forward slashes internally, so `travsr-lsif-ts` emits
    // the verbatim prefix as `//?/` (and `//?/UNC/`), while a backslash producer
    // (rust-analyzer, the daemon's repo_root) emits `\\?\`. Handling only the
    // backslash form left every travsr-lsif-ts occurrence path as `//?/C:/...`,
    // which never relativized and surfaced verbatim in `references`.
    let stripped = p
        .strip_prefix(r"\\?\UNC\")
        .or_else(|| p.strip_prefix(r"\\?\"))
        .or_else(|| p.strip_prefix("//?/UNC/"))
        .or_else(|| p.strip_prefix("//?/"))
        .unwrap_or(p);
    let mut s = stripped.replace('\\', "/");
    // Lowercase only a leading `X:` drive letter so a case-mismatched drive
    // (`D:` vs `d:`) still matches; the rest of the path keeps its casing.
    let b = s.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        s[..1].make_ascii_lowercase();
    }
    s
}

/// Decode a `file://` URI from an LSIF dump into an OS filesystem path.
///
/// rust-analyzer emits `file:///C:/proj/src/main.rs` on Windows; naively
/// stripping `file://` leaves `/C:/proj/src/main.rs`, whose leading slash before
/// the drive letter is not a valid Windows path and fails to open — which would
/// silently fail-open the #650 call-site source read (every ref treated as a
/// call) on Windows. Drop that leading slash when it precedes a `X:` drive
/// letter. Unix paths (`/home/..`) contain no such prefix and are returned
/// unchanged, so this is a no-op off Windows.
fn file_uri_to_path(uri: &str) -> String {
    let s = uri.strip_prefix("file://").unwrap_or(uri);
    let b = s.as_bytes();
    if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
        s[1..].to_string()
    } else {
        s.to_string()
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
    fn make_relative_unix_no_regression_case_sensitive() {
        // #738 guard: the Windows fix must NOT make Unix matching case-insensitive.
        // A case-mismatched Unix prefix (no drive letter) must still fail to match
        // and fall back to the abs path unchanged — byte-for-byte old behaviour.
        assert_eq!(
            make_relative("/repo", "/REPO/src/foo.ts"),
            "/REPO/src/foo.ts"
        );
        // Casing inside the repo-relative portion is preserved verbatim.
        assert_eq!(make_relative("/repo", "/repo/Src/Foo.TS"), "Src/Foo.TS");
    }

    #[test]
    fn make_relative_windows_backslash_base_vs_forward_slash_uri() {
        // #738: the real daemon passes `repo_root.to_string_lossy()` — an
        // extended-length backslash path — while `file_uri_to_path` yields a
        // forward-slash, drive-letter path from rust-analyzer's `file:///D:/..`
        // URI. Before the fix, `strip_prefix` never matched and the absolute
        // path was returned, so `resolve_lsif_positional_refs` dropped every ref.
        // The result must be the repo-relative, forward-slash path that matches a
        // Phase A `Node::vname.path` (e.g. `crates/travsr-store/src/lib.rs`).
        let phase_a_path = "crates/travsr-store/src/lib.rs";
        let abs = "D:/com.travsr/travsr/crates/travsr-store/src/lib.rs";

        // `\\?\`-extended backslash base (what the daemon actually passes).
        assert_eq!(
            make_relative(r"\\?\D:\com.travsr\travsr", abs),
            phase_a_path
        );
        // Plain backslash drive base (still mismatched separators before the fix).
        assert_eq!(make_relative(r"D:\com.travsr\travsr", abs), phase_a_path);
        // Forward-slash base with a trailing separator.
        assert_eq!(make_relative("D:/com.travsr/travsr/", abs), phase_a_path);
        // Drive-letter case-insensitivity: base `\\?\D:\..` vs URI `d:/..`.
        assert_eq!(
            make_relative(
                r"\\?\D:\com.travsr\travsr",
                "d:/com.travsr/travsr/crates/travsr-store/src/lib.rs"
            ),
            phase_a_path
        );
    }

    #[test]
    fn make_relative_forward_slash_verbatim_prefix_from_ts_emitter() {
        // travsr-lsif-ts (via TypeScript) normalizes the Windows verbatim prefix to
        // FORWARD slashes, so the document URI decodes to `//?/C:/...`. The base is a
        // plain backslash repo_root. Before this fix `normalize_path` only stripped
        // the backslash `\\?\` form, so `//?/C:/...` never relativized and every TS
        // occurrence surfaced as `//?/C:/...` in `references`.
        let phase_a_path = "src/main.ts";
        assert_eq!(
            make_relative(r"C:\proj\app", "//?/C:/proj/app/src/main.ts"),
            phase_a_path
        );
        // Both operands carrying the forward-slash verbatim prefix.
        assert_eq!(
            make_relative("//?/C:/proj/app", "//?/C:/proj/app/src/main.ts"),
            phase_a_path
        );
        // UNC forward-slash verbatim form.
        assert_eq!(
            make_relative("//?/UNC/server/share/app", "//?/UNC/server/share/app/x.ts"),
            "x.ts"
        );
    }

    #[test]
    fn make_relative_not_under_base_returns_normalized_abs() {
        // A path outside the repo (e.g. an out-of-tree std/dep def) falls back to
        // the normalized abs path so downstream comparisons stay separator-
        // consistent; it still matches no Phase A node and is dropped fail-closed.
        assert_eq!(
            make_relative(r"\\?\D:\repo", r"C:\other\lib.rs"),
            "c:/other/lib.rs"
        );
    }

    #[test]
    fn file_uri_to_path_decodes_windows_and_unix() {
        // Unix: leading slash is part of the path, kept as-is.
        assert_eq!(
            file_uri_to_path("file:///home/u/proj/src/a.rs"),
            "/home/u/proj/src/a.rs"
        );
        // Windows: rust-analyzer's `file:///C:/..` must lose the slash before the
        // drive letter so the path opens (#650 source read fails otherwise).
        assert_eq!(
            file_uri_to_path("file:///C:/proj/src/a.rs"),
            "C:/proj/src/a.rs"
        );
        assert_eq!(file_uri_to_path("file:///d:/x/y.rs"), "d:/x/y.rs");
        // No scheme prefix → returned unchanged (defensive).
        assert_eq!(file_uri_to_path("/already/a/path"), "/already/a/path");
        // A single leading slash that is NOT a drive letter is preserved.
        assert_eq!(file_uri_to_path("file:///srv/app.rs"), "/srv/app.rs");
    }

    #[test]
    fn ingest_wraps_ingest_raw() {
        let dump = minimal_dump("svc.ts", "class:Svc", "other.ts");
        let out = ingest(&dump, "").unwrap();
        assert_eq!(out.nodes.len(), 0);
        assert_eq!(out.edges.len(), 1);
    }

    #[test]
    fn reader_entry_points_match_str_wrappers() {
        // #736: the streaming (`BufRead`) entry points must produce exactly what
        // the in-memory `&str` wrappers do — same edges, same occurrences —
        // whether fed from a byte slice or a buffered file reader.
        let dump = minimal_dump("svc.ts", "fn:charge", "caller.ts");

        let via_str = ingest_raw(&dump, "").unwrap();
        let via_reader = ingest_raw_from_reader(dump.as_bytes(), "").unwrap();
        assert_eq!(via_str, via_reader, "ingest_raw: reader must match &str");

        let g2_str = ingest_g2(&dump, "").unwrap();
        let g2_reader = ingest_g2_from_reader(dump.as_bytes(), "").unwrap();
        assert_eq!(g2_str.refs.len(), g2_reader.refs.len());
        assert_eq!(g2_reader.refs[0].caller_path, g2_str.refs[0].caller_path);
        assert_eq!(g2_reader.refs[0].caller_line, g2_str.refs[0].caller_line);
        assert_eq!(g2_reader.refs[0].callee_id, g2_str.refs[0].callee_id);

        // Also through a real buffered file reader — the production streaming
        // shape (dump on disk, never materialized as one String).
        let mut file = tempfile::NamedTempFile::new().expect("tempfile");
        std::io::Write::write_all(&mut file, dump.as_bytes()).expect("write dump");
        let reader = std::io::BufReader::new(std::fs::File::open(file.path()).expect("open"));
        let via_file = ingest_raw_from_reader(reader, "").unwrap();
        assert_eq!(via_str, via_file, "ingest_raw: file reader must match &str");
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
    /// `item property:"references"` edges as (referenceResult id, document id),
    /// buffered during the single streaming pass so the emit phase never needs
    /// a second walk over the raw dump (#736 item 7). `inVs` is deliberately
    /// not kept — this path emits one document-level edge per item edge
    /// (DEBT travsr-126).
    ref_items: Vec<(u64, u64)>,
}

/// Parse a rust-analyzer LSIF dump into an intermediate graph.
///
/// Two-pass over the *side tables* (not the raw dump) to handle
/// forward-referenced monikers: rust-analyzer sometimes emits a `moniker` edge
/// (outV=resultSet, inV=moniker_vertex) before the moniker vertex itself.
/// Pass 1 builds what it can while streaming the dump once; pass 2 resolves
/// any pending entries from the tables.
fn parse_rust_graph(dump: impl std::io::BufRead) -> anyhow::Result<RustLsifGraph> {
    let mut g = RustLsifGraph::default();

    // (resultSet id, moniker vertex id) pairs encountered as edges but whose
    // moniker vertex had not yet appeared at edge-parse time.
    let mut pending: Vec<(u64, u64)> = Vec::new();

    for line in dump.lines() {
        let line = line.context("reading LSIF dump line")?;
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
                Some("item") => {
                    // Buffer `item property:"references"` edges for the
                    // single-pass emit phase (#736).
                    if v.get("property").and_then(|p| p.as_str()) != Some("references") {
                        continue;
                    }
                    if let (Some(rr_id), Some(doc_id)) = (
                        v.get("outV").and_then(|i| i.as_u64()),
                        v.get("document").and_then(|i| i.as_u64()),
                    ) {
                        g.ref_items.push((rr_id, doc_id));
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

/// Emit `RefCall` edges from the graph's buffered `item property:"references"`
/// records.
fn ingest_rust_edges(g: &RustLsifGraph, corpus: &str) -> Vec<Edge> {
    let mut edges = Vec::new();

    for &(rr_id, doc_id) in &g.ref_items {
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
        workspace_dep_markers: Vec::new(),
    })
}

/// Return only the `Edge` vec from a rust-analyzer LSIF dump.
///
/// Useful in tests that want to inspect edges directly without the
/// `ParseOutput` wrapper. Thin wrapper over [`ingest_rust_raw_from_reader`]
/// for callers that already hold the dump in memory.
pub fn ingest_rust_raw(dump: &str, corpus: &str) -> anyhow::Result<Vec<Edge>> {
    ingest_rust_raw_from_reader(std::io::Cursor::new(dump), corpus)
}

/// Streaming variant of [`ingest_rust_raw`] (#736 item 7): consumes the LSIF
/// JSON-Lines dump incrementally from any `BufRead` so the raw dump text never
/// has to be held as one `String` alongside the side tables.
pub fn ingest_rust_raw_from_reader(
    dump: impl std::io::BufRead,
    corpus: &str,
) -> anyhow::Result<Vec<Edge>> {
    let g = parse_rust_graph(dump)?;
    Ok(ingest_rust_edges(&g, corpus))
}

/// Positional, fail-closed ingestion of a rust-analyzer LSIF dump (E3 W3b).
///
/// Replaces [`ingest_rust`]'s moniker-synth path (whose callee VName, built at
/// `path = project_root`, matched no Phase A node → 100% dangling). Emits one
/// [`LsifPositionalRef`] per (call site, callee-definition) pair, with 1-based
/// lines and repo-relative paths. The callee is left identified by its
/// **definition location** for the store to resolve positionally against Phase A
/// node spans, failing closed when it resolves to no node.
///
/// **Linkage.** Each occurrence is a `range` whose `next` edge points at its
/// `resultSet`; that resultSet's `textDocument/definition` edge points at a
/// `definitionResult` whose `item` edges list the definition range(s):
///
/// ```text
/// range --next--> resultSet --textDocument/definition--> definitionResult
/// definitionResult --item{document}--> [definition range, ...]
/// document --contains--> [range, ...]
/// ```
///
/// This is the same authoritative structure `bench/lsif_oracle.py` resolves
/// against; the definition *position* — not the moniker — is the answer. An
/// earlier version grouped `item property:{definitions,references}` under a
/// shared vertex, which cross-linked distinct symbols (e.g. an `Iterator::filter`
/// call misattributed to a same-file `Session::filter`); the per-occurrence
/// `next → resultSet → definitionResult` walk is required to keep them apart.
///
/// The definition occurrence itself carries a `next` edge too; it is skipped so
/// a definition never emits a self-reference edge. References whose definition is
/// outside the project (`std`, crates) still resolve to their out-of-tree def
/// path and are dropped later by the store's positional resolver (no matching
/// node) — fail closed by construction.
///
/// O(N) over dump lines plus O(occurrences × defs-per-symbol) for the emit pass.
///
/// `repo_root` is the daemon's own repo root (the same base `Node::vname.path`
/// is made relative to), NOT rust-analyzer's self-reported LSIF `projectRoot`.
/// For a Cargo workspace member, rust-analyzer's `projectRoot` can diverge from
/// the repo root (e.g. a member sub-root); using it here would make
/// `caller_path`/`callee_def_path` relative to the wrong base, so they would
/// never match a real `Node::vname.path` — silently failing every positional
/// ref closed (#I4) instead of resolving. Always relativize against the
/// caller-supplied `repo_root` so both sides of every downstream comparison
/// (`resolve_lsif_positional_refs`, E7's `lsif_covered`) agree.
pub fn ingest_rust_positional(dump: &str, repo_root: &str) -> Vec<travsr_core::LsifPositionalRef> {
    // A Cursor over an in-memory &str cannot fail a line read, so the reader
    // core's only error path (I/O) is unreachable here.
    ingest_rust_positional_from_reader(std::io::Cursor::new(dump), repo_root)
        .expect("in-memory LSIF dump reads cannot fail")
}

/// Streaming variant of [`ingest_rust_positional`] (#736 item 7): consumes the
/// LSIF JSON-Lines dump incrementally from any `BufRead` so the raw dump text
/// never has to be held as one `String` alongside the side tables built below
/// (which are the irreducible working set). Errors only on a failed read from
/// the underlying reader.
pub fn ingest_rust_positional_from_reader(
    dump: impl std::io::BufRead,
    repo_root: &str,
) -> anyhow::Result<Vec<travsr_core::LsifPositionalRef>> {
    let mut doc_paths: HashMap<u64, String> = HashMap::new();
    let mut range_lines: HashMap<u64, u32> = HashMap::new();
    // range id → 0-based UTF-16 start column (for the call-site filter, #650).
    let mut range_cols: HashMap<u64, u32> = HashMap::new();
    // range id → resultSet id (the `next` edge).
    let mut next_edge: HashMap<u64, u64> = HashMap::new();
    // range id → document id (from `contains`).
    let mut range_doc: HashMap<u64, u64> = HashMap::new();
    // resultSet id → definitionResult id (`textDocument/definition`).
    let mut result_def: HashMap<u64, u64> = HashMap::new();
    // vertex id (definitionResult) → its (doc_id, range_id) item targets.
    let mut items: HashMap<u64, Vec<(u64, u64)>> = HashMap::new();

    for line in dump.lines() {
        let line = line.context("reading LSIF dump line")?;
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
                Some("document") => {
                    if let (Some(id), Some(uri)) = (
                        v.get("id").and_then(|i| i.as_u64()),
                        v.get("uri").and_then(|u| u.as_str()),
                    ) {
                        // #650: the resolved path is read from disk to classify
                        // each occurrence as a call, so it must be a valid OS
                        // path on Windows too (rust-analyzer emits `file:///C:/`).
                        let path = file_uri_to_path(uri);
                        doc_paths.insert(id, path);
                    }
                }
                Some("range") => {
                    if let (Some(id), Some(l)) = (
                        v.get("id").and_then(|i| i.as_u64()),
                        v.get("start")
                            .and_then(|s| s.get("line"))
                            .and_then(|l| l.as_u64()),
                    ) {
                        range_lines.insert(id, l as u32);
                        if let Some(c) = v
                            .get("start")
                            .and_then(|s| s.get("character"))
                            .and_then(|c| c.as_u64())
                        {
                            range_cols.insert(id, c as u32);
                        }
                    }
                }
                _ => {}
            },
            Some("edge") => match v.get("label").and_then(|l| l.as_str()) {
                Some("contains") => {
                    if let (Some(doc), Some(in_vs)) = (
                        v.get("outV").and_then(|i| i.as_u64()),
                        v.get("inVs").and_then(|a| a.as_array()),
                    ) {
                        for r in in_vs {
                            if let Some(rid) = r.as_u64() {
                                range_doc.insert(rid, doc);
                            }
                        }
                    }
                }
                Some("next") => {
                    if let (Some(out_v), Some(in_v)) = (
                        v.get("outV").and_then(|i| i.as_u64()),
                        v.get("inV").and_then(|i| i.as_u64()),
                    ) {
                        next_edge.insert(out_v, in_v);
                    }
                }
                Some("textDocument/definition") => {
                    if let (Some(out_v), Some(in_v)) = (
                        v.get("outV").and_then(|i| i.as_u64()),
                        v.get("inV").and_then(|i| i.as_u64()),
                    ) {
                        result_def.insert(out_v, in_v);
                    }
                }
                Some("item") => {
                    if let (Some(out_v), Some(doc), Some(in_vs)) = (
                        v.get("outV").and_then(|i| i.as_u64()),
                        v.get("document").and_then(|i| i.as_u64()),
                        v.get("inVs").and_then(|a| a.as_array()),
                    ) {
                        let entry = items.entry(out_v).or_default();
                        for r in in_vs {
                            if let Some(rid) = r.as_u64() {
                                entry.push((doc, rid));
                            }
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    // Every range that is some symbol's definition target — skipped as an
    // occurrence so a definition site never emits a self-reference edge.
    let def_range_ids: std::collections::HashSet<u64> = result_def
        .values()
        .filter_map(|defres| items.get(defres))
        .flat_map(|targets| targets.iter().map(|(_, rid)| *rid))
        .collect();

    let mut src = crate::callsite::SourceLines::new();
    let mut out = Vec::new();
    // #738: normalize the repo root once — every emitted ref relativizes its
    // caller/callee paths against this same base.
    let base_prefix = normalized_base_prefix(repo_root);
    for (&range_id, &caller_line0) in &range_lines {
        if def_range_ids.contains(&range_id) {
            continue;
        }
        let (Some(&doc), Some(&rs)) = (range_doc.get(&range_id), next_edge.get(&range_id)) else {
            continue;
        };
        let Some(caller_abs) = doc_paths.get(&doc) else {
            continue;
        };
        // #650: classify whether this occurrence is a call. rust-analyzer LSIF
        // reports a reference for every use — `self`/`Self`, type refs, path
        // segments — not just calls. Non-calls still record a `find_references`
        // occurrence downstream, but must NOT create a call-graph `ref/call` edge
        // (that is what collapses into `src == dst` self-loops and spurious
        // non-call edges). Fail open (treat as a call) when the source line is
        // unreadable — in production the file always exists.
        // RFC-027 #813 P2: read the caller line once to classify the occurrence
        // AND convert its UTF-16 range start to a byte column. LSIF positions are
        // UTF-16 code units (metaData positionEncoding); source is UTF-8, so the
        // stored occurrence column must be the byte offset the daemon can convert
        // back at use. Fail open (treat as a call, no column) when the line is
        // unreadable; in production the file always exists.
        let (is_call, caller_col) = match range_cols.get(&range_id) {
            Some(&col) => match src.line(std::path::Path::new(caller_abs), caller_line0 + 1) {
                Some(t) => (
                    crate::callsite::occurrence_is_call(t, col),
                    Some(crate::callsite::utf16_col_to_byte(t, col) as u32),
                ),
                None => (true, None),
            },
            None => (true, None),
        };
        let Some(defres) = result_def.get(&rs) else {
            continue; // occurrence with no definition (e.g. a keyword range)
        };
        let Some(targets) = items.get(defres) else {
            continue;
        };
        let caller_path = relative_to_base(&base_prefix, caller_abs);
        for (tdoc, trid) in targets {
            let (Some(def_abs), Some(&def_line0)) = (doc_paths.get(tdoc), range_lines.get(trid))
            else {
                continue;
            };
            out.push(travsr_core::LsifPositionalRef {
                caller_path: caller_path.clone(),
                caller_line: caller_line0 + 1,
                callee_def_path: relative_to_base(&base_prefix, def_abs),
                callee_def_line: def_line0 + 1,
                is_call,
                caller_col,
            });
        }
    }
    Ok(out)
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
    fn ingest_rust_positional_discriminates_same_leaf_by_definition() {
        // Real rust-analyzer LSIF shape (range --next--> resultSet
        // --textDocument/definition--> definitionResult --item--> def range):
        // two `filter` methods (Session::filter def types.rs:2, Bag::filter def
        // types.rs:6) each called on the SAME source line main.rs:6 — the
        // positional discrimination leaf-guessing cannot make. Plus an external
        // call (println at main.rs:9) whose definition lives outside the project;
        // it resolves to its out-of-tree def path here and is dropped later by
        // the store's positional resolver (no matching node). Definition ranges
        // (10, 11) must not emit self-reference edges.
        let dump = r#"
{"id":1,"type":"vertex","label":"metaData","version":"0.4.3","projectRoot":"file:///repo","positionEncoding":"utf-16","toolInfo":{"name":"rust-analyzer","version":"0"}}
{"id":2,"type":"vertex","label":"document","uri":"file:///repo/src/types.rs"}
{"id":3,"type":"vertex","label":"document","uri":"file:///repo/src/main.rs"}
{"id":99,"type":"vertex","label":"document","uri":"file:///ext/std.rs"}
{"id":10,"type":"vertex","label":"range","start":{"line":2,"character":11},"end":{"line":2,"character":17}}
{"id":11,"type":"vertex","label":"range","start":{"line":6,"character":11},"end":{"line":6,"character":17}}
{"id":20,"type":"vertex","label":"range","start":{"line":6,"character":6},"end":{"line":6,"character":12}}
{"id":21,"type":"vertex","label":"range","start":{"line":6,"character":16},"end":{"line":6,"character":22}}
{"id":30,"type":"vertex","label":"range","start":{"line":9,"character":4},"end":{"line":9,"character":11}}
{"id":70,"type":"vertex","label":"range","start":{"line":100,"character":0},"end":{"line":100,"character":7}}
{"id":50,"type":"vertex","label":"resultSet"}
{"id":51,"type":"vertex","label":"resultSet"}
{"id":52,"type":"vertex","label":"resultSet"}
{"id":60,"type":"vertex","label":"definitionResult"}
{"id":61,"type":"vertex","label":"definitionResult"}
{"id":62,"type":"vertex","label":"definitionResult"}
{"id":200,"type":"edge","label":"contains","outV":2,"inVs":[10,11]}
{"id":201,"type":"edge","label":"contains","outV":3,"inVs":[20,21,30]}
{"id":202,"type":"edge","label":"contains","outV":99,"inVs":[70]}
{"id":210,"type":"edge","label":"next","outV":10,"inV":50}
{"id":211,"type":"edge","label":"next","outV":20,"inV":50}
{"id":212,"type":"edge","label":"next","outV":11,"inV":51}
{"id":213,"type":"edge","label":"next","outV":21,"inV":51}
{"id":214,"type":"edge","label":"next","outV":30,"inV":52}
{"id":220,"type":"edge","label":"textDocument/definition","outV":50,"inV":60}
{"id":221,"type":"edge","label":"textDocument/definition","outV":51,"inV":61}
{"id":222,"type":"edge","label":"textDocument/definition","outV":52,"inV":62}
{"id":230,"type":"edge","label":"item","document":2,"property":"definitions","inVs":[10],"outV":60}
{"id":231,"type":"edge","label":"item","document":2,"property":"definitions","inVs":[11],"outV":61}
{"id":232,"type":"edge","label":"item","document":99,"property":"definitions","inVs":[70],"outV":62}
"#;
        let mut out = ingest_rust_positional(dump, "/repo");
        out.sort_by(|a, b| {
            a.callee_def_path
                .cmp(&b.callee_def_path)
                .then(a.callee_def_line.cmp(&b.callee_def_line))
        });
        assert_eq!(out.len(), 3, "def ranges 10/11 must not self-emit");

        // External println: resolves to its out-of-tree def path (store drops it).
        assert_eq!(out[0].callee_def_path, "/ext/std.rs");
        assert_eq!(out[0].callee_def_line, 101);
        assert_eq!(out[0].caller_path, "src/main.rs");
        assert_eq!(out[0].caller_line, 10);
        // Session::filter: def types.rs:2(0-based)→3, ref main.rs:6(0-based)→7.
        assert_eq!(out[1].callee_def_path, "src/types.rs");
        assert_eq!(out[1].callee_def_line, 3);
        assert_eq!(out[1].caller_line, 7);
        // Bag::filter: def types.rs:6(0-based)→7, ref main.rs:6(0-based)→7.
        assert_eq!(out[2].callee_def_path, "src/types.rs");
        assert_eq!(out[2].callee_def_line, 7);
        assert_eq!(out[2].caller_line, 7);
    }

    #[test]
    fn ingest_rust_positional_ignores_dump_project_root_divergence() {
        // #I4: rust-analyzer's self-reported LSIF `projectRoot` can diverge from
        // the daemon's actual repo root (e.g. a Cargo workspace member
        // sub-root). Relativizing against the dump's `projectRoot` here would
        // produce a `caller_path`/`callee_def_path` that never matches a real
        // `Node::vname.path` downstream, silently dropping every positional
        // ref. The dump below self-reports `projectRoot":"file:///repo/crates/foo"`
        // (a workspace member sub-root), but the caller passes the real repo
        // root `/repo` — paths must come out relative to `/repo`, not the
        // dump's claim.
        let dump = r#"
{"id":1,"type":"vertex","label":"metaData","version":"0.4.3","projectRoot":"file:///repo/crates/foo","positionEncoding":"utf-16","toolInfo":{"name":"rust-analyzer","version":"0"}}
{"id":2,"type":"vertex","label":"document","uri":"file:///repo/crates/foo/src/lib.rs"}
{"id":3,"type":"vertex","label":"document","uri":"file:///repo/crates/foo/src/helper.rs"}
{"id":10,"type":"vertex","label":"range","start":{"line":1,"character":0},"end":{"line":1,"character":4}}
{"id":20,"type":"vertex","label":"range","start":{"line":5,"character":6},"end":{"line":5,"character":12}}
{"id":50,"type":"vertex","label":"resultSet"}
{"id":60,"type":"vertex","label":"definitionResult"}
{"id":200,"type":"edge","label":"contains","outV":3,"inVs":[10]}
{"id":201,"type":"edge","label":"contains","outV":2,"inVs":[20]}
{"id":210,"type":"edge","label":"next","outV":20,"inV":50}
{"id":220,"type":"edge","label":"textDocument/definition","outV":50,"inV":60}
{"id":230,"type":"edge","label":"item","document":3,"property":"definitions","inVs":[10],"outV":60}
"#;
        let out = ingest_rust_positional(dump, "/repo");
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].caller_path, "crates/foo/src/lib.rs",
            "caller_path must be relative to the real repo root, not the dump's projectRoot"
        );
        assert_eq!(
            out[0].callee_def_path, "crates/foo/src/helper.rs",
            "callee_def_path must be relative to the real repo root, not the dump's projectRoot"
        );

        // #736: the streaming entry point must agree with the &str wrapper.
        let via_reader = ingest_rust_positional_from_reader(dump.as_bytes(), "/repo").unwrap();
        assert_eq!(via_reader.len(), 1);
        assert_eq!(via_reader[0].caller_path, out[0].caller_path);
        assert_eq!(via_reader[0].callee_def_path, out[0].callee_def_path);
        assert_eq!(via_reader[0].caller_line, out[0].caller_line);
        assert_eq!(via_reader[0].callee_def_line, out[0].callee_def_line);
    }

    #[test]
    fn rust_reader_entry_points_match_str_wrappers() {
        // #736: the streaming (`BufRead`) rust-analyzer entry points must
        // produce exactly what the in-memory `&str` wrappers do.
        let dump = rust_dump_one_ref();
        let via_str = ingest_rust_raw(dump, "corp").unwrap();
        let via_reader = ingest_rust_raw_from_reader(dump.as_bytes(), "corp").unwrap();
        assert_eq!(
            via_str, via_reader,
            "ingest_rust_raw: reader must match &str"
        );
        assert_eq!(via_reader.len(), 1);
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

    #[test]
    fn positional_refs_flag_calls_and_non_call_occurrences() {
        // #650 cause fix: rust-analyzer LSIF reports a reference occurrence for
        // every use of a symbol. Every occurrence is still emitted (so
        // `find_references` keeps non-call use sites), but each is tagged
        // `is_call` — only calls become `ref/call` edges downstream. A type
        // reference on the same symbol must be flagged non-call (it is what
        // collapses into a `src == dst` self-loop when the callee def resolves to
        // the enclosing fn, and is a spurious non-call edge off the diagonal).
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        // Occurrences we will point the dump at:
        //   line 1, col 11 → `Session` type reference  (is_call = false)
        //   line 2, col 6  → `filter` method call       (is_call = true)
        let main_rs = "fn run(s: Session) {\n    let x: Session = s;\n    s.filter();\n}\n";
        std::fs::write(src_dir.join("main.rs"), main_rs).unwrap();
        std::fs::write(
            src_dir.join("types.rs"),
            "struct Session;\n\nimpl Session {\n    fn filter(&self) {}\n}\n",
        )
        .unwrap();

        // Build portable file:// URIs. On Windows `dir.path()` uses `\` and a
        // drive letter (`C:\..`); backslashes are invalid inside a JSON string
        // (so serde would drop the document vertices, emptying the output) and
        // the raw path is not URI-shaped. Normalize to forward slashes and the
        // real rust-analyzer Windows shape `file:///C:/..` (a leading slash
        // before the drive), which `file_uri_to_path` decodes to a readable OS
        // path — exercising the Windows decode this test guards. On Unix the
        // path already starts with `/`, so this yields the usual `file:///..`.
        let root = dir.path().to_string_lossy().replace('\\', "/");
        let uri_root = if root.starts_with('/') {
            root.clone()
        } else {
            format!("/{root}")
        };
        let main_uri = format!("file://{uri_root}/src/main.rs");
        let types_uri = format!("file://{uri_root}/src/types.rs");
        // 10/11 are def ranges in types.rs; 20 is the call occurrence, 21 the
        // type-reference occurrence, both in main.rs.
        let dump = format!(
            r#"
{{"id":1,"type":"vertex","label":"metaData","version":"0.4.3","projectRoot":"file://{uri_root}","positionEncoding":"utf-16","toolInfo":{{"name":"rust-analyzer","version":"0"}}}}
{{"id":2,"type":"vertex","label":"document","uri":"{main_uri}"}}
{{"id":3,"type":"vertex","label":"document","uri":"{types_uri}"}}
{{"id":10,"type":"vertex","label":"range","start":{{"line":3,"character":7}},"end":{{"line":3,"character":13}}}}
{{"id":11,"type":"vertex","label":"range","start":{{"line":0,"character":7}},"end":{{"line":0,"character":14}}}}
{{"id":20,"type":"vertex","label":"range","start":{{"line":2,"character":6}},"end":{{"line":2,"character":12}}}}
{{"id":21,"type":"vertex","label":"range","start":{{"line":1,"character":11}},"end":{{"line":1,"character":18}}}}
{{"id":50,"type":"vertex","label":"resultSet"}}
{{"id":51,"type":"vertex","label":"resultSet"}}
{{"id":60,"type":"vertex","label":"definitionResult"}}
{{"id":61,"type":"vertex","label":"definitionResult"}}
{{"id":200,"type":"edge","label":"contains","outV":2,"inVs":[20,21]}}
{{"id":201,"type":"edge","label":"contains","outV":3,"inVs":[10,11]}}
{{"id":210,"type":"edge","label":"next","outV":20,"inV":50}}
{{"id":211,"type":"edge","label":"next","outV":21,"inV":51}}
{{"id":220,"type":"edge","label":"textDocument/definition","outV":50,"inV":60}}
{{"id":221,"type":"edge","label":"textDocument/definition","outV":51,"inV":61}}
{{"id":230,"type":"edge","label":"item","document":3,"property":"definitions","inVs":[10],"outV":60}}
{{"id":231,"type":"edge","label":"item","document":3,"property":"definitions","inVs":[11],"outV":61}}
"#
        );

        let out = ingest_rust_positional(&dump, &root);
        // Both occurrences are emitted (find_references keeps the type use site).
        assert_eq!(
            out.len(),
            2,
            "both occurrences must be emitted; got {out:?}"
        );
        let call = out
            .iter()
            .find(|r| r.caller_line == 3)
            .expect("the s.filter() call occurrence");
        assert!(call.is_call, "the `s.filter()` occurrence must be a call");
        assert_eq!(call.callee_def_path, "src/types.rs");
        let type_ref = out
            .iter()
            .find(|r| r.caller_line == 2)
            .expect("the `Session` type-reference occurrence");
        assert!(
            !type_ref.is_call,
            "the `let x: Session` type reference must be flagged non-call"
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
        workspace_dep_markers: vec![],
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
    repo_root: &std::path::Path,
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
    //
    // #650: general-purpose SCIP indexers (scip-go, scip-java, scip-clang, ...)
    // emit an occurrence for every reference, not just calls. Classify each so
    // non-calls still record a `find_references` occurrence but do not create a
    // spurious `ref/call` edge (and, when the callee resolves to the enclosing
    // fn, a `src == dst` self-loop). The `(`-rule is only sound for
    // paren-required languages; others keep the prior edge-emitting behavior.
    let classify_calls = crate::callsite::uses_call_parens(language);
    let mut src = crate::callsite::SourceLines::new();
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
                // Occurrence column is range[1] (0-based UTF-16). Fail open
                // (treat as a call) when the language is paren-optional or the
                // source line is unreadable.
                let is_call = if classify_calls {
                    match occ.range.get(1) {
                        Some(&col) => src
                            .line(&repo_root.join(path), caller_line)
                            .map(|t| crate::callsite::occurrence_is_call(t, col.max(0) as u32))
                            .unwrap_or(true),
                        None => true,
                    }
                } else {
                    true
                };
                // RFC-027 #813 P2: convert the occurrence's 0-based UTF-16 start
                // column (SCIP positions are UTF-16 code units) to the byte offset
                // the occurrence store keeps, reading the source line via the same
                // cache `is_call` used. None when the range carries no column or
                // the line is unreadable, in which case the daemon name-searches.
                let caller_col: Option<u32> = match occ.range.get(1) {
                    Some(&col) => src
                        .line(&repo_root.join(path), caller_line)
                        .map(|t| crate::callsite::utf16_col_to_byte(t, col.max(0) as u32) as u32),
                    None => None,
                };
                out.refs.push(travsr_core::ScipRef {
                    caller_path: path.clone(),
                    caller_line,
                    callee_id,
                    is_call,
                    caller_col,
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

// ── SCIP call-site filter tests (#650) ─────────────────────────────────────────

#[cfg(test)]
mod scip_g2_callsite_tests {
    use super::*;

    fn occ(line: i32, start: i32, end: i32, symbol: &str, roles: i32) -> scip::types::Occurrence {
        scip::types::Occurrence {
            range: vec![line, start, end],
            symbol: symbol.to_string(),
            symbol_roles: roles,
            ..Default::default()
        }
    }

    /// A tiny SCIP index over a real Go file with two definitions (`Svc`, `Do`)
    /// and two references: a `Svc` type reference (NON-call) and an `s.Do()`
    /// call. Returns `(index_bytes, repo_root_tempdir)`.
    fn go_index_with_call_and_type_ref() -> (Vec<u8>, tempfile::TempDir) {
        use protobuf::Message as _;
        let dir = tempfile::tempdir().unwrap();
        // Columns: line 3 `Svc` at col 11 (non-call), line 4 `Do` at col 6 (call).
        let go = "package p\ntype Svc struct{}\nfunc (s Svc) Do() {}\nfunc run(s Svc) {\n    s.Do()\n}\n";
        std::fs::write(dir.path().join("svc.go"), go).unwrap();

        let doc = scip::types::Document {
            relative_path: "svc.go".to_string(),
            occurrences: vec![
                occ(1, 5, 8, "Svc#", 1),    // def
                occ(2, 13, 15, "Do().", 1), // def
                occ(3, 11, 14, "Svc#", 0),  // type ref → non-call
                occ(4, 6, 8, "Do().", 0),   // call
            ],
            ..Default::default()
        };
        let index = scip::types::Index {
            documents: vec![doc],
            ..Default::default()
        };
        (index.write_to_bytes().unwrap(), dir)
    }

    #[test]
    fn paren_language_flags_only_calls() {
        // Both occurrences are emitted (find_references keeps the type use site),
        // but Go is paren-required so only the `s.Do()` occurrence is a call.
        let (bytes, dir) = go_index_with_call_and_type_ref();
        let out = ingest_scip_g2(&bytes, "corp", "go", dir.path()).unwrap();
        assert_eq!(
            out.refs.len(),
            2,
            "both occurrences emitted; got {:?}",
            out.refs
        );
        let call = out
            .refs
            .iter()
            .find(|r| r.caller_line == 5)
            .expect("the s.Do() call on line 5");
        assert!(call.is_call, "s.Do() must be a call");
        let type_ref = out
            .refs
            .iter()
            .find(|r| r.caller_line == 4)
            .expect("the `Svc` type reference on line 4");
        assert!(
            !type_ref.is_call,
            "the `Svc` type reference must be non-call"
        );
    }

    #[test]
    fn paren_optional_language_flags_everything_as_call() {
        // Ruby allows paren-less calls (`obj.method`), so we must NOT apply the
        // `(`-rule — every occurrence stays a call (recall over precision), which
        // preserves the prior edge-emitting behavior for those languages.
        let (bytes, dir) = go_index_with_call_and_type_ref();
        let out = ingest_scip_g2(&bytes, "corp", "ruby", dir.path()).unwrap();
        assert_eq!(
            out.refs.len(),
            2,
            "both occurrences emitted; got {:?}",
            out.refs
        );
        assert!(
            out.refs.iter().all(|r| r.is_call),
            "paren-optional languages leave every occurrence flagged as a call"
        );
    }
}
