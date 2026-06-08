use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use travsr_core::{Edge, NodeId};
use travsr_indexer::{FfiMarker, Node};
use travsr_plugin_host::{trust::TrustConfig, PluginIndexer};

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
    let trust = TrustConfig::from_disk();
    let (phase_b_nodes, phase_b_edges) = indexer.invoke_phase_b_all(dir, &trust);
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
        let entry = entry.context("walking directory")?;
        if !entry.file_type().is_file() {
            continue;
        }
        let ext = entry
            .path()
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        if matches!(
            ext,
            "ts" | "tsx" | "js" | "jsx" | "mjs"   // TypeScript / JavaScript
            | "rs"                                   // Rust
            | "py" | "pyi"                           // Python
            | "go"                                   // Go
            | "java"                                 // Java
            | "kt" | "kts"                           // Kotlin
            | "scala"                                // Scala
            | "rb"                                   // Ruby
            | "php"                                  // PHP
            | "cs"                                   // C#
            | "cpp" | "cc" | "cxx" | "hpp" | "hh"  // C++
            | "c" | "h"                              // C
            | "swift"                                // Swift
            | "dart" // Dart
        ) {
            files.push(entry.path().to_path_buf());
        }
    }
    Ok(files)
}
