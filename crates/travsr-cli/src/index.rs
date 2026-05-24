use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use travsr_core::NodeId;
use travsr_indexer::{FfiMarker, Indexer, Node};

/// Walk `dir`, index all source files, resolve FFI edges, and write the
/// graph JSON to `output`.  Used by the cross-lang precision gate CI workflow.
pub fn run(dir: &Path, output: &Path, corpus: &str) -> anyhow::Result<()> {
    let indexer = Indexer::with_corpus(corpus);

    let mut all_nodes: HashMap<NodeId, Node> = HashMap::new();
    let mut all_markers: Vec<FfiMarker> = Vec::new();

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
            }
            Err(e) => {
                tracing::warn!(file = %abs_path.display(), error = %e, "index: skipping file");
            }
        }
    }

    let ffi_edges = indexer.resolve_ffi_edges(&all_markers);

    let mut edge_entries: Vec<serde_json::Value> = Vec::new();
    for edge in &ffi_edges {
        let (Some(src), Some(dst)) = (all_nodes.get(&edge.src), all_nodes.get(&edge.dst)) else {
            tracing::warn!(src = ?edge.src, dst = ?edge.dst, "index: FFI edge references unknown node");
            continue;
        };
        edge_entries.push(serde_json::json!({
            "kind": edge.kind.as_str(),
            "confidence": edge.confidence,
            "src": {
                "language": src.vname.language,
                "path": src.vname.path,
                "signature": src.vname.signature,
            },
            "dst": {
                "language": dst.vname.language,
                "path": dst.vname.path,
                "signature": dst.vname.signature,
            },
        }));
    }

    let out = serde_json::json!({
        "schema_version": 1,
        "edges": edge_entries,
    });

    let content = serde_json::to_string_pretty(&out)?;
    std::fs::write(output, &content).with_context(|| format!("writing {}", output.display()))?;

    tracing::info!(
        files = files.len(),
        markers = all_markers.len(),
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
        let entry = entry.context("walking directory")?;
        if !entry.file_type().is_file() {
            continue;
        }
        let ext = entry
            .path()
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        if matches!(ext, "ts" | "tsx" | "rs" | "py" | "pyi" | "go") {
            files.push(entry.path().to_path_buf());
        }
    }
    Ok(files)
}
