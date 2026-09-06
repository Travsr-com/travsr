use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use travsr_core::{Edge, EdgeKind, NodeId, ScipRef};
use travsr_indexer::{FfiMarker, Node};
use travsr_plugin_host::PluginIndexer;

/// Walk `dir`, index all source files, resolve FFI edges, and write the
/// graph JSON to `output`.  Used by the cross-lang precision gate CI workflow.
pub fn run(dir: &Path, output: &Path, corpus: &str) -> anyhow::Result<()> {
    let mut indexer = PluginIndexer::new(corpus);

    let mut all_nodes: HashMap<NodeId, Node> = HashMap::new();
    let mut all_markers: Vec<FfiMarker> = Vec::new();
    let mut all_phase_a_edges: Vec<Edge> = Vec::new();

    let mut files = collect_source_files(dir)?;
    files.sort();

    for abs_path in &files {
        let vname_path = abs_path
            .strip_prefix(dir)
            .unwrap_or(abs_path)
            .to_string_lossy()
            .replace('\\', "/");

        match indexer.parse_file_with_vname(abs_path, &vname_path) {
            Ok(out) => {
                for node in &out.nodes {
                    all_nodes.insert(node.id, node.clone());
                }
                all_markers.extend(out.ffi_markers);
                all_phase_a_edges.extend(out.edges);
            }
            Err(e) => {
                tracing::warn!(file = %abs_path.display(), error = %e, "index: skipping file");
            }
        }
    }

    let ffi_edges = indexer.resolve_ffi_edges(&all_markers);

    // Phase B: semantic edges (resolves-to, ref/call) via external sidecars.
    // P1 (#322): derive present_languages from the already-collected file list.
    let present_languages: std::collections::HashSet<String> = files
        .iter()
        .filter_map(|p| p.extension().and_then(|e| e.to_str()))
        .filter_map(travsr_core::Language::from_extension)
        .map(|l| l.as_str().to_string())
        .collect();
    let phase_b_inputs = travsr_plugin_host::PhaseBInputs {
        repo_root: dir,
        present_languages,
        // P6 (#329): forward the already-collected file list so Phase B runners
        // skip their own directory walks.
        indexable_paths: &files,
        liveness: None,
    };
    // E3 W3b note: `_phase_b_positional` (rust-analyzer LSIF positional refs) is
    // resolved only in the daemon path, which has a persistent store to resolve
    // callee definition locations against; this offline JSON emitter has none.
    let (
        phase_b_nodes,
        phase_b_edges,
        phase_b_refs,
        _phase_b_unresolved,
        _phase_b_positional,
        _phase_b_outcome,
    ) = indexer.invoke_phase_b_all(&phase_b_inputs);
    for node in phase_b_nodes {
        all_nodes.insert(node.id, node);
    }
    let mut all_phase_b_edges = phase_b_edges;

    // #833: LSIF/SCIP occurrence refs (ScipRef) carry the highest-fidelity
    // cross-file call graph, but the offline emitter used to drop them (the
    // daemon attributes them via write_scip_attributed_batch; this path had no
    // store to do so). Attribute them here against the in-memory node set so
    // `travsr index` emits the same ref/call and ref/field edges the daemon
    // persists. Dedup against the edges already collected above.
    if !phase_b_refs.is_empty() {
        let mut seen: std::collections::HashSet<(NodeId, NodeId, &'static str)> = all_phase_b_edges
            .iter()
            .map(|e| (e.src, e.dst, e.kind.as_str()))
            .collect();
        for edge in attribute_scip_refs(&all_nodes, &phase_b_refs) {
            if seen.insert((edge.src, edge.dst, edge.kind.as_str())) {
                all_phase_b_edges.push(edge);
            }
        }
    }

    // Emit all indexed nodes sorted by (path, signature) for deterministic output.
    let node_entries: Vec<serde_json::Value> = {
        let mut nodes: Vec<&Node> = all_nodes.values().collect();
        nodes.sort_by(|a, b| {
            a.vname
                .path
                .cmp(&b.vname.path)
                .then_with(|| a.vname.signature.cmp(&b.vname.signature))
        });
        nodes
            .into_iter()
            .map(|n| {
                serde_json::json!({
                    "corpus": n.vname.corpus,
                    "language": n.vname.language,
                    "path": n.vname.path,
                    "signature": n.vname.signature,
                    "kind": n.kind,
                })
            })
            .collect()
    };

    let mut edge_entries: Vec<serde_json::Value> = Vec::new();

    // Phase A structural edges (defines/binding, depends)
    for edge in &all_phase_a_edges {
        let (Some(src), Some(dst)) = (all_nodes.get(&edge.src), all_nodes.get(&edge.dst)) else {
            continue;
        };
        edge_entries.push(serde_json::json!({
            "kind": edge.kind.as_str(),
            "src": { "language": src.vname.language, "path": src.vname.path, "signature": src.vname.signature },
            "dst": { "language": dst.vname.language, "path": dst.vname.path, "signature": dst.vname.signature },
        }));
    }

    // Phase B semantic edges (resolves-to, ref/call)
    let mut phase_b_dropped = 0usize;
    for edge in &all_phase_b_edges {
        let (Some(src), Some(dst)) = (all_nodes.get(&edge.src), all_nodes.get(&edge.dst)) else {
            phase_b_dropped += 1;
            continue;
        };
        edge_entries.push(serde_json::json!({
            "kind": edge.kind.as_str(),
            "src": { "language": src.vname.language, "path": src.vname.path, "signature": src.vname.signature },
            "dst": { "language": dst.vname.language, "path": dst.vname.path, "signature": dst.vname.signature },
        }));
    }

    // Cross-language FFI edges (ffi/call)
    for edge in &ffi_edges {
        let (Some(src), Some(dst)) = (all_nodes.get(&edge.src), all_nodes.get(&edge.dst)) else {
            tracing::warn!(src = ?edge.src, dst = ?edge.dst, "index: FFI edge references unknown node");
            continue;
        };
        edge_entries.push(serde_json::json!({
            "kind": edge.kind.as_str(),
            "confidence": edge.confidence,
            "src": { "language": src.vname.language, "path": src.vname.path, "signature": src.vname.signature },
            "dst": { "language": dst.vname.language, "path": dst.vname.path, "signature": dst.vname.signature },
        }));
    }

    let out = serde_json::json!({
        "schema_version": 1,
        "nodes": node_entries,
        "edges": edge_entries,
    });

    let content = serde_json::to_string_pretty(&out)?;
    std::fs::write(output, &content).with_context(|| format!("writing {}", output.display()))?;

    tracing::info!(
        files = files.len(),
        phase_a_edges = all_phase_a_edges.len(),
        phase_b_edges = all_phase_b_edges.len(),
        phase_b_dropped,
        ffi_edges = ffi_edges.len(),
        output = %output.display(),
        "index: complete"
    );

    Ok(())
}

fn collect_source_files(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            // depth 0 is the root the user named (e.g. `travsr index .`, whose
            // file_name() is "."). Never prune it on the dotfile rule, or the
            // whole walk yields nothing; the rule is meant for dot-entries the
            // walk *descends into*, not the root itself.
            e.depth() == 0
                || !e
                    .file_name()
                    .to_str()
                    .map(|s| s.starts_with('.'))
                    .unwrap_or(false)
        })
    {
        // UX-011: `walkdir::Error`'s Display already embeds the path and the OS
        // message ("IO error for operation on <dir>: <os error>"). Chaining it as
        // a `#[source]` (what `.context()` does) makes `{:#}` print that OS text a
        // second time from the underlying `io::Error`. Fold it into a source-less
        // message so the OS string appears exactly once.
        let entry = entry.map_err(|e| anyhow::anyhow!("walking directory: {e}"))?;
        if !entry.file_type().is_file() {
            continue;
        }
        // Single source of truth for "is this indexable": recognized source /
        // data-format extension OR a name-recognized manifest (go.mod, *.csproj).
        // Previously this walk carried its own hardcoded extension list that had
        // drifted from `Language::from_extension` (it omitted json/yaml/toml/xml
        // and markdown entirely); routing through the shared classifier keeps
        // every enumeration gate consistent.
        if travsr_core::is_indexable_path(entry.path()) {
            files.push(entry.path().to_path_buf());
        }
    }
    Ok(files)
}

/// Attribute LSIF/SCIP occurrence refs (`ScipRef`) to their enclosing-symbol
/// nodes, producing the semantic edges the daemon persists via the store's
/// `write_scip_attributed_batch` but which the offline emitter previously
/// dropped (#833). Works entirely against the in-memory node set this path
/// already holds. The rules mirror the store path exactly:
///
///   * caller = the narrowest function/method node whose line span contains the
///     occurrence, falling back to the file node for the occurrence's path;
///   * callee = `ref.callee_id`, which MUST already be a node — fail closed, so
///     an unresolved (external / stdlib) symbol never becomes a dangling edge;
///   * self-loops (caller == callee, from positional collapse) are dropped;
///   * a reference onto a `field` node becomes `ref/field`; a call becomes
///     `ref/call`; other non-call references carry no edge (they exist only as
///     occurrence sites, which the offline JSON schema does not represent).
///
/// Returned edges are deduplicated by (src, dst, kind).
fn attribute_scip_refs(all_nodes: &HashMap<NodeId, Node>, refs: &[ScipRef]) -> Vec<Edge> {
    struct Span<'a> {
        node: &'a Node,
        line: u32,
        end_line: u32,
    }

    // Function/method spans and file nodes, keyed by path.
    let mut spans_by_path: HashMap<&str, Vec<Span>> = HashMap::new();
    let mut file_by_path: HashMap<&str, &Node> = HashMap::new();
    for node in all_nodes.values() {
        match node.kind.as_str() {
            "function" | "method" | "fn" => {
                if let (Some(line), Some(end_line)) = (node.line, node.end_line) {
                    spans_by_path
                        .entry(node.vname.path.as_str())
                        .or_default()
                        .push(Span {
                            node,
                            line,
                            end_line,
                        });
                }
            }
            "file" => {
                file_by_path.entry(node.vname.path.as_str()).or_insert(node);
            }
            _ => {}
        }
    }
    // Narrowest span first (smallest first), id as a deterministic tie-break —
    // mirrors fetch_all_fn_spans' `ORDER BY (end_line - line) ASC, id ASC`.
    for spans in spans_by_path.values_mut() {
        spans.sort_by(|a, b| {
            (a.end_line - a.line)
                .cmp(&(b.end_line - b.line))
                .then(a.node.id.cmp(&b.node.id))
        });
    }

    let mut seen: std::collections::HashSet<(NodeId, NodeId, &'static str)> =
        std::collections::HashSet::new();
    let mut edges: Vec<Edge> = Vec::new();
    for r in refs {
        // Fail closed: a callee that resolves to no node is a genuine unresolved
        // symbol, not a not-yet-written one (the full node table is in memory).
        let Some(callee) = all_nodes.get(&r.callee_id) else {
            continue;
        };

        let caller_id = spans_by_path
            .get(r.caller_path.as_str())
            .and_then(|spans| {
                spans
                    .iter()
                    .find(|s| s.line <= r.caller_line && s.end_line >= r.caller_line)
                    .map(|s| s.node.id)
            })
            .or_else(|| file_by_path.get(r.caller_path.as_str()).map(|n| n.id));
        let Some(caller_id) = caller_id else {
            continue;
        };

        // Drop self-loops (positional collapse): they carry no reachability.
        if caller_id == r.callee_id {
            continue;
        }

        let kind = if callee.kind == "field" {
            EdgeKind::RefField
        } else if r.is_call {
            EdgeKind::RefCall
        } else {
            // Non-call, non-field reference: an occurrence site only, which the
            // offline schema does not carry. No edge.
            continue;
        };

        if seen.insert((caller_id, r.callee_id, kind.as_str())) {
            edges.push(Edge {
                src: caller_id,
                dst: r.callee_id,
                kind,
                confidence: None,
                provenance: None,
            });
        }
    }
    edges
}

#[cfg(test)]
mod tests {
    use super::*;
    use travsr_core::VName;

    fn node(path: &str, sig: &str, kind: &str, line: Option<u32>, end: Option<u32>) -> Node {
        let mut n = Node::new(VName::new("c", "", path, "typescript", sig), kind);
        n.line = line;
        n.end_line = end;
        n
    }

    fn scip_ref(path: &str, line: u32, callee: NodeId, is_call: bool) -> ScipRef {
        ScipRef {
            caller_path: path.to_string(),
            caller_line: line,
            callee_id: callee,
            is_call,
            caller_col: None,
        }
    }

    fn nodes(list: Vec<Node>) -> HashMap<NodeId, Node> {
        list.into_iter().map(|n| (n.id, n)).collect()
    }

    // ── collect_source_files depth-0 rule (#833) ────────────────────────────

    #[test]
    fn collects_from_a_root_whose_own_name_starts_with_a_dot() {
        // `travsr index .` reaches walkdir with a root whose file_name() is
        // "."; the dotfile rule used to prune it and the walk yielded nothing.
        // A root literally named ".hidden" is the same case, without the cwd
        // dependence. The rule must still prune dot-entries *below* the root.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join(".hidden");
        std::fs::create_dir(&root).expect("create root");
        std::fs::write(root.join("main.rs"), "fn main() {}").expect("write source");
        std::fs::create_dir(root.join(".git")).expect("create .git");
        std::fs::write(root.join(".git/config.rs"), "fn nope() {}").expect("write pruned");

        let files = collect_source_files(&root).expect("walk must succeed");

        let names: Vec<_> = files
            .iter()
            .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
            .collect();
        assert_eq!(names, vec!["main.rs"], "got {files:?}");
    }

    #[test]
    fn attributes_call_to_narrowest_enclosing_fn() {
        // outer spans 1..20, inner spans 5..10; an occurrence on line 7 must
        // attribute to inner (the narrowest enclosing span), not outer.
        let outer = node("a.js", "fn:outer", "function", Some(1), Some(20));
        let inner = node("a.js", "fn:inner", "function", Some(5), Some(10));
        let callee = node("b.js", "fn:target", "function", Some(1), Some(3));
        let callee_id = callee.id;
        let inner_id = inner.id;
        let all = nodes(vec![outer, inner, callee]);

        let edges = attribute_scip_refs(&all, &[scip_ref("a.js", 7, callee_id, true)]);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].src, inner_id);
        assert_eq!(edges[0].dst, callee_id);
        assert_eq!(edges[0].kind, EdgeKind::RefCall);
    }

    #[test]
    fn falls_back_to_file_node_outside_any_span() {
        let file = node("a.js", "", "file", None, None);
        let callee = node("b.js", "fn:target", "function", Some(1), Some(3));
        let file_id = file.id;
        let callee_id = callee.id;
        let all = nodes(vec![file, callee]);

        // Occurrence on line 2 with no enclosing fn -> attributed to the file node.
        let edges = attribute_scip_refs(&all, &[scip_ref("a.js", 2, callee_id, true)]);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].src, file_id);
    }

    #[test]
    fn drops_self_loop_and_unresolved_callee() {
        let f = node("a.js", "fn:f", "function", Some(1), Some(10));
        let f_id = f.id;
        let all = nodes(vec![f]);

        // Self-loop: caller resolves to f, callee IS f.
        let self_loop = attribute_scip_refs(&all, &[scip_ref("a.js", 5, f_id, true)]);
        assert!(self_loop.is_empty(), "self-loop must be dropped");

        // Fail closed: callee id is not a known node.
        let ghost = VName::new("c", "", "z.js", "typescript", "fn:ghost").id();
        let unresolved = attribute_scip_refs(&all, &[scip_ref("a.js", 5, ghost, true)]);
        assert!(unresolved.is_empty(), "unresolved callee must be dropped");
    }

    #[test]
    fn field_ref_becomes_ref_field_and_non_call_is_skipped() {
        let caller = node("a.js", "fn:use", "function", Some(1), Some(10));
        let field = node("b.js", "field:Obj.x", "field", Some(2), Some(2));
        let plain = node("b.js", "fn:target", "function", Some(1), Some(3));
        let caller_id = caller.id;
        let field_id = field.id;
        let plain_id = plain.id;
        let all = nodes(vec![caller, field, plain]);

        // A reference onto a field node -> ref/field, regardless of is_call.
        let f = attribute_scip_refs(&all, &[scip_ref("a.js", 5, field_id, false)]);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].kind, EdgeKind::RefField);
        assert_eq!(f[0].src, caller_id);

        // A non-call reference onto a non-field node -> no edge (occurrence only).
        let none = attribute_scip_refs(&all, &[scip_ref("a.js", 5, plain_id, false)]);
        assert!(none.is_empty());
    }

    #[test]
    fn dedups_repeated_occurrences() {
        let caller = node("a.js", "fn:use", "function", Some(1), Some(10));
        let callee = node("b.js", "fn:target", "function", Some(1), Some(3));
        let caller_id = caller.id;
        let callee_id = callee.id;
        let all = nodes(vec![caller, callee]);

        // Two call occurrences on different lines, same caller and callee.
        let edges = attribute_scip_refs(
            &all,
            &[
                scip_ref("a.js", 3, callee_id, true),
                scip_ref("a.js", 8, callee_id, true),
            ],
        );
        assert_eq!(
            edges.len(),
            1,
            "same caller->callee call must collapse to one edge"
        );
        assert_eq!(edges[0].src, caller_id);
    }
}
