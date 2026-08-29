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
use travsr_core::{Edge, EdgeKind, Node, UnresolvedCall, VName};
use tree_sitter::{Parser, Query, QueryCursor};

// ── Tree-sitter queries ───────────────────────────────────────────────────────

/// Call-site captures:
///   `call.fn`     — `foo()`
///   `call.method` — `obj.method()` / `this.method()`, with the receiver
///                   expression bound as `call.recv` (E4) so the extractor can
///                   attempt to recover its type instead of discarding it.
///   `call.new`    — `new Foo()`
const CALL_QUERY: &str = "
(call_expression function: (identifier) @call.fn)
(call_expression function: (member_expression object: (_) @call.recv property: (property_identifier) @call.method))
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
///
/// When `files` is `Some`, the caller supplies pre-walked `(abs_path, vname_path)`
/// pairs from the daemon's Phase A walk (P6 — #329); the extractor uses them
/// directly and skips its own directory walk. Pass `None` to fall back to
/// `collect_source_files`.
pub fn extract_native_phase_b(
    corpus: &str,
    root: &Path,
    files: Option<&[(PathBuf, String)]>,
) -> anyhow::Result<(Vec<Node>, Vec<Edge>, Vec<UnresolvedCall>)> {
    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();
    // E4: call sites are emitted as `UnresolvedCall`s (receiver type recovered
    // where possible) and resolved fail-closed against the real node table by
    // the daemon — no more same-file leaf guesses that dangle cross-file.
    let mut unresolved: Vec<UnresolvedCall> = Vec::new();

    let language = tree_sitter::Language::new(tree_sitter_typescript::LANGUAGE_TYPESCRIPT);
    let call_q = Query::new(&language, CALL_QUERY).context("ts call query")?;
    let extends_q = Query::new(&language, EXTENDS_QUERY).context("ts extends query")?;
    let implements_q = Query::new(&language, IMPLEMENTS_QUERY).context("ts implements query")?;

    // Use the daemon's pre-walked file list when available (P6 — #329); fall back
    // to a local walk for old daemons and the `travsr index` CLI path.
    let walked;
    let file_pairs: &[(PathBuf, String)] = match files {
        Some(f) => f,
        None => {
            walked = collect_source_files(root, &["ts", "tsx", "mts", "cts"]);
            &walked
        }
    };

    for (abs_path, vname_path) in file_pairs {
        match extract_file_edges(
            corpus,
            abs_path,
            vname_path,
            &language,
            &call_q,
            &extends_q,
            &implements_q,
        ) {
            Ok((file_nodes, file_edges, file_unresolved)) => {
                nodes.extend(file_nodes);
                edges.extend(file_edges);
                unresolved.extend(file_unresolved);
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

    Ok((nodes, edges, unresolved))
}

/// RFC-027 live IsImplementation lane: the `extends`/`implements` clauses in
/// `files`, as unresolved references (base type name + line).
///
/// Unlike [`extract_native_phase_b`], which emits `IsImplementation` edges under
/// a same-file assumption (`ts_vname(corpus, vname_path, ...)` for the base, so a
/// cross-file base dangles), this returns the raw clause so the daemon can
/// resolve the base against the *real* node table — lexically when it is unique
/// repo-wide, or via the editor's definition provider — and abstain otherwise.
/// It never resolves and never mints identity.
pub fn extract_unresolved_inheritance(
    root: &Path,
    files: Option<&[(PathBuf, String)]>,
) -> anyhow::Result<Vec<travsr_core::InheritanceRef>> {
    let language = tree_sitter::Language::new(tree_sitter_typescript::LANGUAGE_TYPESCRIPT);
    let extends_q = Query::new(&language, EXTENDS_QUERY).context("ts extends query")?;
    let implements_q = Query::new(&language, IMPLEMENTS_QUERY).context("ts implements query")?;

    let walked;
    let file_pairs: &[(PathBuf, String)] = match files {
        Some(f) => f,
        None => {
            walked = collect_source_files(root, &["ts", "tsx", "mts", "cts"]);
            &walked
        }
    };

    let mut out: Vec<travsr_core::InheritanceRef> = Vec::new();
    for (abs_path, _vname_path) in file_pairs {
        let Ok(source) = std::fs::read(abs_path) else {
            continue;
        };
        let mut parser = Parser::new();
        if parser.set_language(&language).is_err() {
            continue;
        }
        let Some(tree) = parser.parse(&source, None) else {
            continue;
        };
        collect_inheritance(
            &extends_q,
            "extends.base",
            tree.root_node(),
            &source,
            &mut out,
        );
        collect_inheritance(
            &implements_q,
            "implements.iface",
            tree.root_node(),
            &source,
            &mut out,
        );
    }
    Ok(out)
}

/// Push one `InheritanceRef` per match of `base_cap` in `q`, carrying the base
/// name's text and its 1-based line.
fn collect_inheritance(
    q: &Query,
    base_cap: &str,
    root: tree_sitter::Node,
    source: &[u8],
    out: &mut Vec<travsr_core::InheritanceRef>,
) {
    let cap_names: Vec<String> = q.capture_names().iter().map(|s| s.to_string()).collect();
    let mut cursor = QueryCursor::new();
    let mut iter = cursor.matches(q, root, source);
    while let Some(m) = iter.next() {
        for &cap in m.captures {
            let Some(name) = cap_names.get(cap.index as usize) else {
                continue;
            };
            if name != base_cap {
                continue;
            }
            let Ok(text) = cap.node.utf8_text(source) else {
                continue;
            };
            out.push(travsr_core::InheritanceRef {
                base_name: text.to_string(),
                line: cap.node.start_position().row as u32 + 1,
            });
        }
    }
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
) -> anyhow::Result<(Vec<Node>, Vec<Edge>, Vec<UnresolvedCall>)> {
    let source =
        std::fs::read(abs_path).with_context(|| format!("reading {}", abs_path.display()))?;

    let mut parser = Parser::new();
    parser
        .set_language(language)
        .context("loading TypeScript grammar")?;
    let tree = match parser.parse(&source, None) {
        Some(t) => t,
        None => return Ok((vec![], vec![], vec![])),
    };

    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();
    let mut unresolved: Vec<UnresolvedCall> = Vec::new();

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
                // The receiver is consumed as a sibling of `call.method`, not on
                // its own — skip it here so it never becomes a callee.
                if cap_name == "call.recv" {
                    continue;
                }
                let Ok(callee_name) = cap.node.utf8_text(source.as_slice()) else {
                    continue;
                };
                if callee_name.len() < 2 {
                    continue;
                }

                // 1-based call-site line (#299) for edge_sites → find_references.
                let occ_line = cap.node.start_position().row.saturating_add(1) as u32;

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

                // E4: emit an UnresolvedCall (fail-closed, resolved against the
                // real node table by the daemon) instead of a same-file leaf
                // guess. `this.method()` recovers `recv_type` = enclosing class;
                // `obj.method()` recovers it from the nearest preceding
                // `const obj = new Foo()` / `obj: Foo` annotation, else `None`
                // (the daemon then requires a unique qualified leaf).
                match cap_name.as_str() {
                    "call.method" => {
                        let recv_type = m
                            .captures
                            .iter()
                            .find(|c| {
                                cap_names.get(c.index as usize).map(String::as_str)
                                    == Some("call.recv")
                            })
                            .and_then(|recv_cap| {
                                resolve_receiver_type_ts(
                                    recv_cap.node,
                                    source.as_slice(),
                                    &caller_class,
                                )
                            });
                        unresolved.push(UnresolvedCall {
                            src: caller_id,
                            callee_sig: format!("fn:{callee_name}"),
                            alt_callee_sig: None,
                            hint_crate: None,
                            caller_line: occ_line,
                            is_method_call: true,
                            recv_type,
                        });
                    }
                    // `new Foo()` targets the class definition — resolve it
                    // fail-closed against the real `class:Foo` node.
                    "call.new" => {
                        unresolved.push(UnresolvedCall {
                            src: caller_id,
                            callee_sig: format!("class:{callee_name}"),
                            alt_callee_sig: None,
                            hint_crate: None,
                            caller_line: occ_line,
                            is_method_call: false,
                            recv_type: None,
                        });
                    }
                    // Bare `foo()` — free function; daemon resolves by unique sig.
                    _ => {
                        unresolved.push(UnresolvedCall {
                            src: caller_id,
                            callee_sig: format!("fn:{callee_name}"),
                            alt_callee_sig: None,
                            hint_crate: None,
                            caller_line: occ_line,
                            is_method_call: false,
                            recv_type: None,
                        });
                    }
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

    Ok((nodes, edges, unresolved))
}

// ── Receiver-type resolution (E4) ─────────────────────────────────────────────

/// Recover the receiver's type name for `recv.method()`, using only information
/// visible inside the enclosing function (file-local by construction, so an
/// incremental single-file reindex reaches the same answer as a full build).
///
/// Accepted: `this` resolves via the enclosing class; a plain identifier
/// resolves via the nearest preceding `const/let name = new Type()` /
/// `name: Type` annotation (variable or parameter). Anything else — member
/// chains (`this.x.method()`), calls, index expressions — returns `None`,
/// keeping the daemon on its fail-closed unique-leaf path.
fn resolve_receiver_type_ts(
    recv: tree_sitter::Node<'_>,
    source: &[u8],
    enclosing_class: &Option<String>,
) -> Option<String> {
    match recv.kind() {
        "this" => enclosing_class.clone(),
        "identifier" => {
            let name = recv.utf8_text(source).ok()?;
            let scope = enclosing_scope_ts(recv)?;
            nearest_preceding_binding_type_ts(scope, source, name, recv.start_byte())
        }
        _ => None,
    }
}

/// Walk up to the nearest enclosing function-like node (the scope whose
/// parameters and body can bind the receiver identifier).
fn enclosing_scope_ts(node: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    let mut cur = node.parent()?;
    loop {
        match cur.kind() {
            "method_definition"
            | "function_declaration"
            | "function_expression"
            | "arrow_function" => return Some(cur),
            "program" => return None,
            _ => {}
        }
        cur = cur.parent()?;
    }
}

/// Type of the nearest binding of `name` starting before `before_byte`: an
/// annotated parameter (`name: Type`), an annotated variable (`const name: Type`),
/// or a `new Type()` initializer (`const name = new Type()`). Only these
/// syntactically unambiguous shapes are used; generics/unions yield `None`.
fn nearest_preceding_binding_type_ts(
    scope: tree_sitter::Node<'_>,
    source: &[u8],
    name: &str,
    before_byte: usize,
) -> Option<String> {
    // Nearest binding of `name` before the call, as `(byte_pos, recovered_type)`.
    // The type is `Option` because a binding may exist without a recoverable
    // type (a bare `x = other()` reassignment); its position is still recorded
    // so a later untyped rebinding supersedes an earlier typed one and the
    // result is `None` (fail closed) rather than a stale earlier type.
    let mut best: Option<(usize, Option<String>)> = None;

    if let Some(params) = scope.child_by_field_name("parameters") {
        let mut c = params.walk();
        for p in params.children(&mut c) {
            if !matches!(p.kind(), "required_parameter" | "optional_parameter") {
                continue;
            }
            if p.child_by_field_name("pattern")
                .and_then(|n| n.utf8_text(source).ok())
                != Some(name)
            {
                continue;
            }
            let Some(ty) = p
                .child_by_field_name("type")
                .and_then(|t| type_name_ts(t, source))
            else {
                continue;
            };
            if p.start_byte() < before_byte
                && best.as_ref().map_or(true, |(b, _)| p.start_byte() > *b)
            {
                best = Some((p.start_byte(), Some(ty)));
            }
        }
    }

    if let Some(body) = scope.child_by_field_name("body") {
        collect_nearest_var_ts(body, source, name, before_byte, &mut best);
    }

    best.and_then(|(_, t)| t)
}

/// Bare type name from a `type_annotation` node. Accepts `type_identifier`
/// (`Foo`); rejects generics/unions (`Foo<T>`, `A | B`) whose receiver has no
/// single resolvable name.
fn type_name_ts(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    match node.kind() {
        "type_annotation" => node.named_child(0).and_then(|c| type_name_ts(c, source)),
        "type_identifier" => node.utf8_text(source).ok().map(str::to_string),
        _ => None,
    }
}

/// The receiver-usable type of a value node: the constructor name of a
/// `new Type()` expression (a bare identifier constructor), else `None`. A
/// non-`new` initializer or a namespaced/generic constructor is not a single
/// resolvable receiver type.
fn new_ctor_type_ts(value: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    if value.kind() != "new_expression" {
        return None;
    }
    let ctor = value.child_by_field_name("constructor")?;
    if ctor.kind() == "identifier" {
        ctor.utf8_text(source).ok().map(str::to_string)
    } else {
        None
    }
}

/// Recurse through `node` collecting bindings of `name` before `before_byte` —
/// both `variable_declarator`s (`const name = ...`) and bare reassignments
/// (`name = ...`, an `assignment_expression`) — keeping the nearest preceding
/// one. The recorded type is an explicit `: Type` annotation or a `new Type()`
/// initializer, else `None`: a nearer binding whose type is unrecoverable
/// (`name = other()`) must supersede an earlier typed one so the receiver
/// resolves to `None`, never a stale type. Does not descend into nested function
/// bodies (their locals are not visible to an outer receiver).
fn collect_nearest_var_ts(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    name: &str,
    before_byte: usize,
    best: &mut Option<(usize, Option<String>)>,
) {
    if node.kind() == "variable_declarator"
        && node
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(source).ok())
            == Some(name)
        && node.start_byte() < before_byte
        && best.as_ref().map_or(true, |(b, _)| node.start_byte() > *b)
    {
        let ty = node
            .child_by_field_name("type")
            .and_then(|t| type_name_ts(t, source))
            .or_else(|| {
                node.child_by_field_name("value")
                    .and_then(|v| new_ctor_type_ts(v, source))
            });
        *best = Some((node.start_byte(), ty));
    }
    if node.kind() == "assignment_expression"
        && node
            .child_by_field_name("left")
            .and_then(|n| n.utf8_text(source).ok())
            == Some(name)
        && node.start_byte() < before_byte
        && best.as_ref().map_or(true, |(b, _)| node.start_byte() > *b)
    {
        let ty = node
            .child_by_field_name("right")
            .and_then(|v| new_ctor_type_ts(v, source));
        *best = Some((node.start_byte(), ty));
    }
    if matches!(
        node.kind(),
        "function_declaration" | "method_definition" | "arrow_function" | "function_expression"
    ) {
        return;
    }
    let mut c = node.walk();
    for child in node.children(&mut c) {
        collect_nearest_var_ts(child, source, name, before_byte, best);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Run the real `CALL_QUERY` + `resolve_receiver_type_ts` path end-to-end
    /// against `source`, returning the recovered receiver type for the
    /// `.method_name()` call site.
    fn recv_type_for_call(source: &[u8], method_name: &str) -> Option<String> {
        let language = tree_sitter::Language::new(tree_sitter_typescript::LANGUAGE_TYPESCRIPT);
        let mut parser = Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let query = Query::new(&language, CALL_QUERY).unwrap();
        let cap_names: Vec<String> = query
            .capture_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut cursor = QueryCursor::new();
        let mut iter = cursor.matches(&query, tree.root_node(), source);
        while let Some(m) = iter.next() {
            for &cap in m.captures {
                let cap_name = cap_names.get(cap.index as usize).map(String::as_str);
                if cap_name != Some("call.method") {
                    continue;
                }
                if cap.node.utf8_text(source).ok() != Some(method_name) {
                    continue;
                }
                let caller_class = find_enclosing_fn_ts(cap.node, source).and_then(|(_, c)| c);
                let recv = m.captures.iter().find(|c| {
                    cap_names.get(c.index as usize).map(String::as_str) == Some("call.recv")
                });
                return recv.and_then(|r| resolve_receiver_type_ts(r.node, source, &caller_class));
            }
        }
        None
    }

    #[test]
    fn recv_this_resolves_to_enclosing_class() {
        let source = br#"
class App {
    run() {
        this.helper();
    }
}
"#;
        assert_eq!(
            recv_type_for_call(source, "helper"),
            Some("App".to_string())
        );
    }

    #[test]
    fn recv_new_initializer_resolves() {
        let source = br#"
function build() {
    const store = new SqliteStore();
    store.open();
}
"#;
        assert_eq!(
            recv_type_for_call(source, "open"),
            Some("SqliteStore".to_string())
        );
    }

    #[test]
    fn recv_reassignment_to_unrecoverable_type_supersedes_earlier_new() {
        // Fail-closed: a bare `store = other()` reassignment (an
        // assignment_expression, not a declarator) is the nearest binding and
        // its type is unrecoverable, so it must supersede the earlier
        // `new SqliteStore()` — receiver resolves to None, not a stale type.
        let source = br#"
function build() {
    let store = new SqliteStore();
    store = other();
    store.open();
}
"#;
        assert_eq!(recv_type_for_call(source, "open"), None);
    }

    #[test]
    fn recv_reassignment_to_new_type_wins_over_earlier_binding() {
        // The nearest binding wins in both directions: a later `new Foo()`
        // reassignment overrides an earlier binding of a different type.
        let source = br#"
function build() {
    let store = new Widget();
    store = new SqliteStore();
    store.open();
}
"#;
        assert_eq!(
            recv_type_for_call(source, "open"),
            Some("SqliteStore".to_string())
        );
    }

    #[test]
    fn recv_typed_variable_resolves() {
        let source = br#"
function build() {
    const s: Session = get();
    s.close();
}
"#;
        assert_eq!(
            recv_type_for_call(source, "close"),
            Some("Session".to_string())
        );
    }

    #[test]
    fn recv_typed_parameter_resolves() {
        let source = br#"
function handle(session: Session) {
    session.close();
}
"#;
        assert_eq!(
            recv_type_for_call(source, "close"),
            Some("Session".to_string())
        );
    }

    #[test]
    fn recv_member_chain_is_none() {
        // `this.other.run()` — receiver is a member chain, not `this` or a plain
        // identifier. Must NOT resolve (no false self-class edge).
        let source = br#"
class App {
    run() {
        this.other.run();
    }
}
"#;
        assert_eq!(recv_type_for_call(source, "run"), None);
    }

    /// RFC-027 live IsImplementation lane: `extends`/`implements` clauses come
    /// back as unresolved references carrying the base type's name and the line
    /// it is written on, so the daemon can resolve them against the real node
    /// table rather than the same-file assumption `extract_native_phase_b` makes.
    #[test]
    fn extends_and_implements_come_back_as_inheritance_refs() {
        let dir = std::env::temp_dir().join(format!("rfc027_inherit_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("order.ts");
        std::fs::write(
            &file,
            b"import { Base } from './base';\nexport class Order extends Base implements Shape {\n  area() { return 0; }\n}\n",
        )
        .unwrap();
        let files = vec![(file.clone(), "order.ts".to_string())];
        let refs = extract_unresolved_inheritance(&dir, Some(&files)).unwrap();

        // Both clauses are on line 2 (the class declaration line).
        assert!(
            refs.iter().any(|r| r.base_name == "Base" && r.line == 2),
            "extends Base captured at its line, got {refs:?}",
        );
        assert!(
            refs.iter().any(|r| r.base_name == "Shape" && r.line == 2),
            "implements Shape captured at its line, got {refs:?}",
        );
        assert_eq!(refs.len(), 2, "exactly the two clauses, no more: {refs:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn new_expression_emits_class_unresolved_call() {
        let dir = std::env::temp_dir().join(format!("e4_ts_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("m.ts");
        std::fs::write(
            &file,
            b"function build() {\n    const x = new Session();\n    x.run();\n}\n",
        )
        .unwrap();
        let files = vec![(file.clone(), "m.ts".to_string())];
        let (_nodes, edges, unresolved) = extract_native_phase_b("c", &dir, Some(&files)).unwrap();
        assert!(
            edges.iter().all(|e| e.kind != EdgeKind::RefCall),
            "call sites must not emit raw RefCall edges"
        );
        // `new Session()` → class:Session UnresolvedCall.
        assert!(
            unresolved
                .iter()
                .any(|u| u.callee_sig == "class:Session" && !u.is_method_call),
            "new Session() emitted as class: UnresolvedCall"
        );
        // `x.run()` → recv_type Session (from `new Session()`).
        let run = unresolved
            .iter()
            .find(|u| u.callee_sig == "fn:run")
            .expect("run() emitted as UnresolvedCall");
        assert!(run.is_method_call);
        assert_eq!(run.recv_type.as_deref(), Some("Session"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
