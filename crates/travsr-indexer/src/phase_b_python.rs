//! Native Python Phase B — zero external-tool dependencies.
//!
//! Sources of edges (tree-sitter only, no spawned processes):
//!   - `RefCall`          — function and method call sites
//!   - `IsImplementation` — `class Foo(Bar)` inheritance
//!
//! When scip-python is available the caller merges SCIP output on top
//! for higher-fidelity type-resolved edges.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use streaming_iterator::StreamingIterator as _;
use travsr_core::{Edge, EdgeKind, Node, VName};
use tree_sitter::{Parser, Query, QueryCursor};

// ── Tree-sitter queries ───────────────────────────────────────────────────────

/// Call-site captures:
///   `call.id`     — `foo()`
///   `call.method` — `self.process()` / `obj.method()`
const CALL_QUERY: &str = "
(call function: (identifier) @call.id)
(call function: (attribute attribute: (identifier) @call.method))
";

/// Inheritance captures:
///   `class.name` / `base.name` — `class Dog(Animal): ...`
const INHERIT_QUERY: &str = "
(class_definition
  name: (identifier) @class.name
  superclasses: (argument_list (identifier) @base.name))
";

// ── Public API ────────────────────────────────────────────────────────────────

/// Extract native Phase B edges for a Python corpus at `root`.
///
/// When `files` is `Some`, the caller supplies pre-walked `(abs_path, vname_path)`
/// pairs from the daemon's Phase A walk (P6 — #329); the extractor uses them
/// directly and skips its own directory walk. Pass `None` to fall back to
/// `collect_source_files`.
pub fn extract_native_phase_b(
    corpus: &str,
    root: &Path,
    files: Option<&[(PathBuf, String)]>,
) -> anyhow::Result<(Vec<Node>, Vec<Edge>)> {
    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();

    let language = tree_sitter::Language::new(tree_sitter_python::LANGUAGE);
    let call_q = Query::new(&language, CALL_QUERY).context("python call query")?;
    let inherit_q = Query::new(&language, INHERIT_QUERY).context("python inherit query")?;

    // Use the daemon's pre-walked file list when available (P6 — #329); fall back
    // to a local walk for old daemons and the `travsr index` CLI path.
    let walked;
    let file_pairs: &[(PathBuf, String)] = match files {
        Some(f) => f,
        None => {
            walked = collect_source_files(root, &["py", "pyi"]);
            &walked
        }
    };

    for (abs_path, vname_path) in file_pairs {
        match extract_file_edges(corpus, abs_path, vname_path, &language, &call_q, &inherit_q) {
            Ok((file_nodes, file_edges)) => {
                nodes.extend(file_nodes);
                edges.extend(file_edges);
            }
            Err(e) => {
                tracing::debug!(err = %e, path = %abs_path.display(), "python phase_b file skipped")
            }
        }
    }

    nodes.sort_unstable_by_key(|n| n.id);
    nodes.dedup_by_key(|n| n.id);
    edges.sort_unstable_by_key(|e| (e.src, e.dst));
    edges.dedup_by(|a, b| a.src == b.src && a.dst == b.dst && a.kind == b.kind);

    Ok((nodes, edges))
}

// ── Per-file analysis ─────────────────────────────────────────────────────────

fn extract_file_edges(
    corpus: &str,
    abs_path: &Path,
    vname_path: &str,
    language: &tree_sitter::Language,
    call_q: &Query,
    inherit_q: &Query,
) -> anyhow::Result<(Vec<Node>, Vec<Edge>)> {
    let source =
        std::fs::read(abs_path).with_context(|| format!("reading {}", abs_path.display()))?;

    let mut parser = Parser::new();
    parser
        .set_language(language)
        .context("loading Python grammar")?;
    let tree = match parser.parse(&source, None) {
        Some(t) => t,
        None => return Ok((vec![], vec![])),
    };

    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();

    // ── Call sites ────────────────────────────────────────────────────────────
    {
        let cap_names: Vec<String> = call_q
            .capture_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut cursor = QueryCursor::new();
        let mut iter = cursor.matches(call_q, tree.root_node(), source.as_slice());

        while let Some(m) = iter.next() {
            for &cap in m.captures {
                let Some(cap_name) = cap_names.get(cap.index as usize) else {
                    continue;
                };
                let Ok(callee_name) = cap.node.utf8_text(source.as_slice()) else {
                    continue;
                };
                if callee_name.len() < 2 {
                    continue;
                }

                let Some((caller_fn, caller_class)) =
                    find_enclosing_fn_py(cap.node, source.as_slice())
                else {
                    continue;
                };

                let caller_id = match &caller_class {
                    Some(c) => {
                        py_vname(corpus, vname_path, &format!("method:{c}.{caller_fn}")).id()
                    }
                    None => py_vname(corpus, vname_path, &format!("fn:{caller_fn}")).id(),
                };

                let callee_id = match cap_name.as_str() {
                    "call.method" => match &caller_class {
                        // `self.method()` inside class Foo → try Foo.method first
                        Some(c) => {
                            py_vname(corpus, vname_path, &format!("method:{c}.{callee_name}")).id()
                        }
                        None => py_vname(corpus, vname_path, &format!("fn:{callee_name}")).id(),
                    },
                    _ => py_vname(corpus, vname_path, &format!("fn:{callee_name}")).id(),
                };

                if caller_id != callee_id {
                    edges.push(Edge::new(caller_id, callee_id, EdgeKind::RefCall));
                }
            }
        }
    }

    // ── Inheritance ───────────────────────────────────────────────────────────
    {
        let cap_names: Vec<String> = inherit_q
            .capture_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut cursor = QueryCursor::new();
        let mut iter = cursor.matches(inherit_q, tree.root_node(), source.as_slice());

        while let Some(m) = iter.next() {
            let mut class_name: Option<&str> = None;
            let mut base_name: Option<&str> = None;
            for &cap in m.captures {
                let Some(cap_name) = cap_names.get(cap.index as usize) else {
                    continue;
                };
                let Ok(text) = cap.node.utf8_text(source.as_slice()) else {
                    continue;
                };
                match cap_name.as_str() {
                    "class.name" => class_name = Some(text),
                    "base.name" => base_name = Some(text),
                    _ => {}
                }
            }
            if let (Some(child), Some(base)) = (class_name, base_name) {
                let child_id = py_vname(corpus, vname_path, &format!("class:{child}")).id();
                let base_id = py_vname(corpus, vname_path, &format!("class:{base}")).id();
                // Emit node so it's queryable even without Phase A re-parsing
                nodes.push(Node::new(
                    py_vname(corpus, vname_path, &format!("class:{child}")),
                    "class",
                ));
                edges.push(Edge::new(child_id, base_id, EdgeKind::IsImplementation));
            }
        }
    }

    Ok((nodes, edges))
}

// ── AST helpers ───────────────────────────────────────────────────────────────

/// Walk up the AST to find the nearest enclosing function or method definition.
/// Returns `(fn_name, Option<enclosing_class>)`.
fn find_enclosing_fn_py(
    node: tree_sitter::Node<'_>,
    source: &[u8],
) -> Option<(String, Option<String>)> {
    let mut cur = node.parent()?;
    loop {
        match cur.kind() {
            "function_definition" => {
                let fn_name = cur
                    .child_by_field_name("name")?
                    .utf8_text(source)
                    .ok()?
                    .to_string();
                let class_name = find_parent_class_py(cur, source);
                return Some((fn_name, class_name));
            }
            "module" => return None,
            _ => {}
        }
        cur = cur.parent()?;
    }
}

/// Walk up from a function_definition to find the enclosing class name.
fn find_parent_class_py(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let mut cur = node.parent()?;
    loop {
        match cur.kind() {
            "block" => {
                // block's parent should be class_definition
                if let Some(parent) = cur.parent() {
                    if parent.kind() == "class_definition" {
                        return parent
                            .child_by_field_name("name")?
                            .utf8_text(source)
                            .ok()
                            .map(str::to_string);
                    }
                }
                return None;
            }
            "module" => return None,
            _ => {}
        }
        cur = cur.parent()?;
    }
}

// ── VName helpers ─────────────────────────────────────────────────────────────

fn py_vname(corpus: &str, path: &str, signature: &str) -> VName {
    VName::new(corpus, "", path, "python", signature)
}

// ── File walker ───────────────────────────────────────────────────────────────

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
            if matches!(
                name,
                "target" | "node_modules" | ".git" | "__pycache__" | ".venv" | "venv"
            ) {
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
