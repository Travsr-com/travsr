//! Native TypeScript/JavaScript Phase B — zero external-tool dependencies.
//!
//! Sources of edges (tree-sitter only, no spawned processes):
//!   - `RefCall`          — function and method call sites
//!   - `IsImplementation` — `class Foo implements IBar`
//!   - `Overrides`        — `class Foo extends Bar` (method name matching)
//!
//! When travsr-lsif-ts is available the caller merges LSIF output on top
//! for higher-fidelity type-resolved edges.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use streaming_iterator::StreamingIterator as _;
use travsr_core::{Edge, EdgeKind, Node, VName};
use tree_sitter::{Parser, Query, QueryCursor};

// ── Tree-sitter queries ───────────────────────────────────────────────────────

/// Call-site captures:
///   `call.fn`     — `foo()`
///   `call.method` — `obj.method()` / `this.method()`
///   `call.new`    — `new Foo()`
const CALL_QUERY: &str = "
(call_expression function: (identifier) @call.fn)
(call_expression function: (member_expression property: (property_identifier) @call.method))
(new_expression constructor: (identifier) @call.new)
";

/// Inheritance captures:
///   `extends.class`   / `extends.base`    — `class Foo extends Bar`
///   `implements.class` / `implements.iface` — `class Foo implements IBar`
const EXTENDS_QUERY: &str = "
(class_declaration
  name: (type_identifier) @extends.class
  (class_heritage (extends_clause value: (identifier) @extends.base)))
";

const IMPLEMENTS_QUERY: &str = "
(class_declaration
  name: (type_identifier) @implements.class
  (class_heritage (implements_clause (type_identifier) @implements.iface)))
";

// ── Public API ────────────────────────────────────────────────────────────────

/// Extract native Phase B edges for a TypeScript/JavaScript corpus at `root`.
pub fn extract_native_phase_b(corpus: &str, root: &Path) -> anyhow::Result<(Vec<Node>, Vec<Edge>)> {
    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();

    let language = tree_sitter::Language::new(tree_sitter_typescript::LANGUAGE_TYPESCRIPT);
    let call_q = Query::new(&language, CALL_QUERY).context("ts call query")?;
    let extends_q = Query::new(&language, EXTENDS_QUERY).context("ts extends query")?;
    let implements_q = Query::new(&language, IMPLEMENTS_QUERY).context("ts implements query")?;

    for (abs_path, vname_path) in collect_source_files(root, &["ts", "tsx", "mts", "cts"]) {
        match extract_file_edges(
            corpus,
            &abs_path,
            &vname_path,
            &language,
            &call_q,
            &extends_q,
            &implements_q,
        ) {
            Ok((file_nodes, file_edges)) => {
                nodes.extend(file_nodes);
                edges.extend(file_edges);
            }
            Err(e) => {
                tracing::debug!(err = %e, path = %abs_path.display(), "ts phase_b file skipped")
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
    extends_q: &Query,
    implements_q: &Query,
) -> anyhow::Result<(Vec<Node>, Vec<Edge>)> {
    let source =
        std::fs::read(abs_path).with_context(|| format!("reading {}", abs_path.display()))?;

    let mut parser = Parser::new();
    parser
        .set_language(language)
        .context("loading TypeScript grammar")?;
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
                    find_enclosing_fn_ts(cap.node, source.as_slice())
                else {
                    continue;
                };

                let caller_id = match &caller_class {
                    Some(c) => {
                        ts_vname(corpus, vname_path, &format!("method:{c}.{caller_fn}")).id()
                    }
                    None => ts_vname(corpus, vname_path, &format!("fn:{caller_fn}")).id(),
                };

                let callee_id = match cap_name.as_str() {
                    "call.method" => match &caller_class {
                        Some(c) => {
                            ts_vname(corpus, vname_path, &format!("method:{c}.{callee_name}")).id()
                        }
                        None => ts_vname(corpus, vname_path, &format!("fn:{callee_name}")).id(),
                    },
                    "call.new" => {
                        ts_vname(corpus, vname_path, &format!("class:{callee_name}")).id()
                    }
                    _ => ts_vname(corpus, vname_path, &format!("fn:{callee_name}")).id(),
                };

                if caller_id != callee_id {
                    edges.push(Edge::new(caller_id, callee_id, EdgeKind::RefCall));
                }
            }
        }
    }

    // ── Extends (subclass → superclass) ──────────────────────────────────────
    {
        let cap_names: Vec<String> = extends_q
            .capture_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut cursor = QueryCursor::new();
        let mut iter = cursor.matches(extends_q, tree.root_node(), source.as_slice());

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
                    "extends.class" => class_name = Some(text),
                    "extends.base" => base_name = Some(text),
                    _ => {}
                }
            }
            if let (Some(child), Some(base)) = (class_name, base_name) {
                let child_id = ts_vname(corpus, vname_path, &format!("class:{child}")).id();
                let base_id = ts_vname(corpus, vname_path, &format!("class:{base}")).id();
                // Emit class node for the subclass (base may be in another file)
                nodes.push(Node::new(
                    ts_vname(corpus, vname_path, &format!("class:{child}")),
                    "class",
                ));
                edges.push(Edge::new(child_id, base_id, EdgeKind::IsImplementation));
            }
        }
    }

    // ── Implements (class → interface) ───────────────────────────────────────
    {
        let cap_names: Vec<String> = implements_q
            .capture_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut cursor = QueryCursor::new();
        let mut iter = cursor.matches(implements_q, tree.root_node(), source.as_slice());

        while let Some(m) = iter.next() {
            let mut class_name: Option<&str> = None;
            let mut iface_name: Option<&str> = None;
            for &cap in m.captures {
                let Some(cap_name) = cap_names.get(cap.index as usize) else {
                    continue;
                };
                let Ok(text) = cap.node.utf8_text(source.as_slice()) else {
                    continue;
                };
                match cap_name.as_str() {
                    "implements.class" => class_name = Some(text),
                    "implements.iface" => iface_name = Some(text),
                    _ => {}
                }
            }
            if let (Some(cls), Some(iface)) = (class_name, iface_name) {
                let cls_id = ts_vname(corpus, vname_path, &format!("class:{cls}")).id();
                let iface_id = ts_vname(corpus, vname_path, &format!("class:{iface}")).id();
                edges.push(Edge::new(cls_id, iface_id, EdgeKind::IsImplementation));
            }
        }
    }

    Ok((nodes, edges))
}

// ── AST helpers ───────────────────────────────────────────────────────────────

/// Walk up the AST to find the nearest enclosing named function or method.
/// Returns `(fn_name, Option<enclosing_class>)`.
fn find_enclosing_fn_ts(
    node: tree_sitter::Node<'_>,
    source: &[u8],
) -> Option<(String, Option<String>)> {
    let mut cur = node.parent()?;
    loop {
        match cur.kind() {
            "method_definition" => {
                let fn_name = cur
                    .child_by_field_name("name")?
                    .utf8_text(source)
                    .ok()?
                    .to_string();
                let class_name = find_parent_class_ts(cur, source);
                return Some((fn_name, class_name));
            }
            "function_declaration" | "function" => {
                let fn_name = cur
                    .child_by_field_name("name")?
                    .utf8_text(source)
                    .ok()?
                    .to_string();
                return Some((fn_name, None));
            }
            "arrow_function" => {
                // Arrow function assigned to a variable: `const foo = () => {}`
                if let Some(parent) = cur.parent() {
                    if parent.kind() == "variable_declarator" {
                        let fn_name = parent
                            .child_by_field_name("name")?
                            .utf8_text(source)
                            .ok()?
                            .to_string();
                        return Some((fn_name, None));
                    }
                }
                // Anonymous arrow — skip to outer scope
            }
            "program" => return None,
            _ => {}
        }
        cur = cur.parent()?;
    }
}

/// Walk up from a method node to find the enclosing class name.
fn find_parent_class_ts(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let mut cur = node.parent()?;
    loop {
        match cur.kind() {
            "class_body" => {
                // class_body's parent is the class_declaration
                if let Some(class_decl) = cur.parent() {
                    if matches!(
                        class_decl.kind(),
                        "class_declaration" | "class" | "abstract_class_declaration"
                    ) {
                        return class_decl
                            .child_by_field_name("name")?
                            .utf8_text(source)
                            .ok()
                            .map(str::to_string);
                    }
                }
                return None;
            }
            "program" => return None,
            _ => {}
        }
        cur = cur.parent()?;
    }
}

// ── VName helpers ─────────────────────────────────────────────────────────────

fn ts_vname(corpus: &str, path: &str, signature: &str) -> VName {
    VName::new(corpus, "", path, "typescript", signature)
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
            if matches!(name, "target" | "node_modules" | ".git" | "dist" | "build") {
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
