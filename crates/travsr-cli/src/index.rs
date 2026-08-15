use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use travsr_core::{Edge, NodeId};
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
    };
    // E3 W3b note: `_phase_b_positional` (rust-analyzer LSIF positional refs) is
    // resolved only in the daemon path, which has a persistent store to resolve
    // callee definition locations against; this offline JSON emitter has none.
    let (
        phase_b_nodes,
        phase_b_edges,
        _phase_b_refs,
        _phase_b_unresolved,
        _phase_b_positional,
        _phase_b_outcome,
    ) = indexer.invoke_phase_b_all(&phase_b_inputs);
    for node in phase_b_nodes {
        all_nodes.insert(node.id, node);
    }
    let all_phase_b_edges = phase_b_edges;

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
            !e.file_name()
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
