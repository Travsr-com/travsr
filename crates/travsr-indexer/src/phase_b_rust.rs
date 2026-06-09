//! Native Rust Phase B — zero external-tool dependencies.
//!
//! Sources of edges:
//!   1. Cargo.toml parsing → `Depends` edges between crate nodes
//!   2. Tree-sitter call-site query → `RefCall` edges between functions
//!
//! Accuracy: name-based resolution without a type system. Correct for direct
//! calls; approximate for trait-dispatched generics. Always better than zero
//! edges (the outcome when rust-analyzer is absent). When rust-analyzer is
//! available the caller merges LSIF output on top for higher fidelity.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use streaming_iterator::StreamingIterator as _;
use travsr_core::{Edge, EdgeKind, Node, VName};
use tree_sitter::{Parser, Query, QueryCursor};

// ── Tree-sitter query ─────────────────────────────────────────────────────────

/// Captures three call-site patterns:
///   `call.fn`     — bare identifier: `foo()`
///   `call.method` — field call: `self.process()` / `obj.method()`
///   `call.scoped` — scoped path: `Type::new()` / `std::mem::swap()`
const CALL_QUERY: &str = "
(call_expression function: (identifier) @call.fn)
(call_expression function: (field_expression field: (field_identifier) @call.method))
(call_expression function: (scoped_identifier name: (identifier) @call.scoped))
";

// ── Public API ────────────────────────────────────────────────────────────────

/// Extract native Phase B edges for a Rust corpus rooted at `root`.
///
/// Returns `(nodes, edges)`:
///   - crate nodes + `Depends` edges from Cargo.toml dependency graph
///   - `RefCall` edges from tree-sitter call-site analysis
pub fn extract_native_phase_b(corpus: &str, root: &Path) -> anyhow::Result<(Vec<Node>, Vec<Edge>)> {
    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();

    // Pass 1: crate dependency graph via Cargo.toml
    match extract_cargo_deps(corpus, root) {
        Ok((dep_nodes, dep_edges)) => {
            nodes.extend(dep_nodes);
            edges.extend(dep_edges);
        }
        Err(e) => tracing::debug!(err = %e, "cargo dep extraction skipped"),
    }

    // Pass 2: call-site edges via tree-sitter
    let language = tree_sitter::Language::new(tree_sitter_rust::LANGUAGE);
    let query = match Query::new(&language, CALL_QUERY) {
        Ok(q) => q,
        Err(e) => {
            tracing::warn!(err = %e, "rust call-site query compile failed — skipping Phase B calls");
            return Ok((nodes, edges));
        }
    };

    for (abs_path, vname_path) in collect_source_files(root, &["rs"]) {
        match extract_file_call_edges(corpus, &abs_path, &vname_path, &language, &query) {
            Ok(file_edges) => edges.extend(file_edges),
            Err(e) => {
                tracing::debug!(err = %e, path = %abs_path.display(), "rust call extraction skipped")
            }
        }
    }

    // Dedup — same pattern as Phase A
    nodes.sort_unstable_by_key(|n| n.id);
    nodes.dedup_by_key(|n| n.id);
    edges.sort_unstable_by_key(|e| (e.src, e.dst));
    edges.dedup_by(|a, b| a.src == b.src && a.dst == b.dst && a.kind == b.kind);

    Ok((nodes, edges))
}

// ── Cargo.toml dependency graph ───────────────────────────────────────────────

fn extract_cargo_deps(corpus: &str, root: &Path) -> anyhow::Result<(Vec<Node>, Vec<Edge>)> {
    let root_cargo = root.join("Cargo.toml");
    if !root_cargo.exists() {
        return Ok((vec![], vec![]));
    }

    let root_text = std::fs::read_to_string(&root_cargo).context("reading root Cargo.toml")?;
    let root_doc: toml::Value = root_text.parse().context("parsing root Cargo.toml")?;

    // Collect all member Cargo.toml paths
    let cargo_paths: Vec<PathBuf> = if let Some(members) = root_doc
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
    {
        members
            .iter()
            .filter_map(|m| m.as_str())
            .flat_map(|member| {
                if member.contains('*') {
                    // Simple glob: enumerate direct children of prefix dir
                    let prefix = member.trim_end_matches("/*").trim_end_matches('*');
                    let base = root.join(prefix);
                    if let Ok(rd) = std::fs::read_dir(base) {
                        rd.flatten()
                            .map(|e| e.path().join("Cargo.toml"))
                            .filter(|p| p.exists())
                            .collect()
                    } else {
                        vec![]
                    }
                } else {
                    let p = root.join(member).join("Cargo.toml");
                    if p.exists() {
                        vec![p]
                    } else {
                        vec![]
                    }
                }
            })
            .collect()
    } else {
        vec![root_cargo]
    };

    // Parse each Cargo.toml: (pkg_name, rel_cargo_path, dep_names)
    let pkg_data: Vec<(String, String, Vec<String>)> = cargo_paths
        .iter()
        .filter_map(|cargo_path| {
            let text = std::fs::read_to_string(cargo_path).ok()?;
            let doc: toml::Value = text.parse().ok()?;
            let pkg_name = doc
                .get("package")
                .and_then(|p| p.get("name"))
                .and_then(|n| n.as_str())?
                .to_string();
            let rel = cargo_path
                .strip_prefix(root)
                .unwrap_or(cargo_path)
                .to_string_lossy()
                .replace('\\', "/");
            let mut dep_names: Vec<String> = Vec::new();
            for section in &["dependencies", "dev-dependencies", "build-dependencies"] {
                if let Some(deps) = doc.get(section).and_then(|d| d.as_table()) {
                    dep_names.extend(deps.keys().cloned());
                }
            }
            Some((pkg_name, rel, dep_names))
        })
        .collect();

    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();

    for (pkg_name, rel_path, dep_names) in &pkg_data {
        let pkg_node = Node::new(
            VName::new(
                corpus,
                "",
                rel_path.as_str(),
                "rust",
                format!("crate:{pkg_name}"),
            ),
            "crate",
        );
        let pkg_id = pkg_node.id;
        nodes.push(pkg_node);

        for dep_name in dep_names {
            let dep_rel = pkg_data
                .iter()
                .find(|(n, _, _)| n == dep_name)
                .map(|(_, p, _)| p.as_str())
                .unwrap_or("");
            let dep_node = Node::new(
                VName::new(corpus, "", dep_rel, "rust", format!("crate:{dep_name}")),
                "crate",
            );
            let dep_id = dep_node.id;
            nodes.push(dep_node);
            edges.push(Edge::new(pkg_id, dep_id, EdgeKind::Depends));
        }
    }

    Ok((nodes, edges))
}

// ── Call-site extraction ──────────────────────────────────────────────────────

fn extract_file_call_edges(
    corpus: &str,
    abs_path: &Path,
    vname_path: &str,
    language: &tree_sitter::Language,
    query: &Query,
) -> anyhow::Result<Vec<Edge>> {
    let source =
        std::fs::read(abs_path).with_context(|| format!("reading {}", abs_path.display()))?;

    let mut parser = Parser::new();
    parser
        .set_language(language)
        .context("loading Rust grammar")?;
    let tree = match parser.parse(&source, None) {
        Some(t) => t,
        None => return Ok(vec![]),
    };

    let cap_names: Vec<String> = query
        .capture_names()
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut cursor = QueryCursor::new();
    let mut iter = cursor.matches(query, tree.root_node(), source.as_slice());

    let mut edges: Vec<Edge> = Vec::new();
    while let Some(m) = iter.next() {
        for &cap in m.captures {
            let Some(cap_name) = cap_names.get(cap.index as usize) else {
                continue;
            };
            let Ok(callee_name) = cap.node.utf8_text(source.as_slice()) else {
                continue;
            };
            // Skip short names that are likely noise (closures, etc.)
            if callee_name.len() < 2 {
                continue;
            }

            let Some((caller_fn, caller_impl)) = find_enclosing_fn(cap.node, source.as_slice())
            else {
                continue;
            };

            let caller_id = match &caller_impl {
                Some(t) => VName::new(
                    corpus,
                    "",
                    vname_path,
                    "rust",
                    format!("fn:{t}.{caller_fn}"),
                )
                .id(),
                None => VName::new(corpus, "", vname_path, "rust", format!("fn:{caller_fn}")).id(),
            };

            let callee_id = match cap_name.as_str() {
                "call.method" => {
                    // Best-effort: resolve to same-file impl method
                    match &caller_impl {
                        Some(t) => VName::new(
                            corpus,
                            "",
                            vname_path,
                            "rust",
                            format!("fn:{t}.{callee_name}"),
                        )
                        .id(),
                        None => {
                            VName::new(corpus, "", vname_path, "rust", format!("fn:{callee_name}"))
                                .id()
                        }
                    }
                }
                "call.scoped" => {
                    // Extract qualifying type from the scoped path parent node
                    let qual_type = cap
                        .node
                        .parent()
                        .and_then(|p| p.child_by_field_name("path"))
                        .and_then(|path_node| path_node.utf8_text(source.as_slice()).ok())
                        .and_then(|t| t.split("::").last().map(str::to_string))
                        .filter(|s| !s.is_empty() && s != callee_name);
                    match qual_type {
                        Some(t) => VName::new(
                            corpus,
                            "",
                            vname_path,
                            "rust",
                            format!("fn:{t}.{callee_name}"),
                        )
                        .id(),
                        None => {
                            VName::new(corpus, "", vname_path, "rust", format!("fn:{callee_name}"))
                                .id()
                        }
                    }
                }
                _ => VName::new(corpus, "", vname_path, "rust", format!("fn:{callee_name}")).id(),
            };

            if caller_id != callee_id {
                edges.push(Edge::new(caller_id, callee_id, EdgeKind::RefCall));
            }
        }
    }

    Ok(edges)
}

/// Walk up the tree-sitter AST to find the nearest enclosing `function_item`.
/// Returns `(fn_name, Option<impl_type>)` or `None` when outside any function.
fn find_enclosing_fn(
    node: tree_sitter::Node<'_>,
    source: &[u8],
) -> Option<(String, Option<String>)> {
    let mut cur = node.parent()?;
    loop {
        match cur.kind() {
            "function_item" => {
                let fn_name = cur
                    .child_by_field_name("name")?
                    .utf8_text(source)
                    .ok()?
                    .to_string();
                let impl_type = find_parent_impl_type(cur, source);
                return Some((fn_name, impl_type));
            }
            "source_file" => return None,
            _ => {}
        }
        cur = cur.parent()?;
    }
}

/// Walk up from `node` to find the nearest enclosing `impl_item` type name.
/// Returns the implementing type (e.g. `"Worker"` for `impl Trait for Worker`).
/// Stops at `mod_item` boundaries.
fn find_parent_impl_type(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let mut cur = node.parent()?;
    loop {
        match cur.kind() {
            "impl_item" => {
                let type_node = cur.child_by_field_name("type")?;
                return match type_node.kind() {
                    "type_identifier" => type_node.utf8_text(source).ok().map(str::to_string),
                    "generic_type" => (0..type_node.child_count())
                        .filter_map(|i| type_node.child(i as u32))
                        .find(|c| c.kind() == "type_identifier")
                        .and_then(|c| c.utf8_text(source).ok().map(str::to_string)),
                    _ => None,
                };
            }
            "mod_item" => return None,
            _ => {}
        }
        cur = cur.parent()?;
    }
}

// ── File walker ───────────────────────────────────────────────────────────────

/// Collect (abs_path, repo-relative vname_path) for all source files with
/// the given extensions under `root`. Skips `target/`, `node_modules/`, hidden dirs.
pub(crate) fn collect_source_files(root: &Path, exts: &[&str]) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    walk(root, root, exts, &mut out);
    out
}

fn walk(root: &Path, dir: &Path, exts: &[&str], out: &mut Vec<(PathBuf, String)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            if matches!(name, "target" | "node_modules" | ".git") {
                continue;
            }
            walk(root, &path, exts, out);
        } else if path.is_file() {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if exts.contains(&ext) {
                let vname_path = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((path, vname_path));
            }
        }
    }
}
