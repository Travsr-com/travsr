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
use travsr_core::{Edge, EdgeKind, Node, UnresolvedCall, VName};
use tree_sitter::{Parser, Query, QueryCursor};

// ── Tree-sitter queries ───────────────────────────────────────────────────────

/// Call-site captures:
///   `call.id`     — `foo()`
///   `call.method` — `self.process()` / `obj.method()`, with the receiver
///                   expression bound as `call.recv` (E4) so the extractor can
///                   attempt to recover its type instead of discarding it.
const CALL_QUERY: &str = "
(call function: (identifier) @call.id)
(call function: (attribute object: (_) @call.recv attribute: (identifier) @call.method))
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
) -> anyhow::Result<(Vec<Node>, Vec<Edge>, Vec<UnresolvedCall>)> {
    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();
    // E4: call sites are emitted as `UnresolvedCall`s (receiver type recovered
    // where possible) and resolved fail-closed against the real node table by
    // the daemon — no more same-file leaf guesses that dangle cross-file.
    let mut unresolved: Vec<UnresolvedCall> = Vec::new();

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
            Ok((file_nodes, file_edges, file_unresolved)) => {
                nodes.extend(file_nodes);
                edges.extend(file_edges);
                unresolved.extend(file_unresolved);
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

    Ok((nodes, edges, unresolved))
}

// ── Per-file analysis ─────────────────────────────────────────────────────────

fn extract_file_edges(
    corpus: &str,
    abs_path: &Path,
    vname_path: &str,
    language: &tree_sitter::Language,
    call_q: &Query,
    inherit_q: &Query,
) -> anyhow::Result<(Vec<Node>, Vec<Edge>, Vec<UnresolvedCall>)> {
    let source =
        std::fs::read(abs_path).with_context(|| format!("reading {}", abs_path.display()))?;

    let mut parser = Parser::new();
    parser
        .set_language(language)
        .context("loading Python grammar")?;
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

                // E4: emit an UnresolvedCall (fail-closed, resolved against the
                // real node table by the daemon) instead of a same-file leaf
                // guess. `self.method()` inside `class Foo` recovers `recv_type`
                // = Foo so the daemon resolves `method:Foo.method` exactly;
                // `obj.method()` recovers the type from the nearest preceding
                // `obj = Type(...)` / annotated parameter, else `None` (the
                // daemon then requires a unique qualified leaf — no fabrication).
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
                                resolve_receiver_type_py(
                                    recv_cap.node,
                                    source.as_slice(),
                                    &caller_class,
                                )
                            });
                        unresolved.push(UnresolvedCall {
                            src: caller_id,
                            callee_sig: format!("fn:{callee_name}"),
                            hint_crate: None,
                            caller_line: occ_line,
                            is_method_call: true,
                            recv_type,
                        });
                    }
                    // Bare `foo()` — free function; daemon resolves by unique sig.
                    _ => {
                        unresolved.push(UnresolvedCall {
                            src: caller_id,
                            callee_sig: format!("fn:{callee_name}"),
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

    Ok((nodes, edges, unresolved))
}

// ── Receiver-type resolution (E4) ─────────────────────────────────────────────

/// Recover the receiver's type name for `recv.method()`, using only information
/// visible inside the enclosing function (file-local by construction, so an
/// incremental single-file reindex reaches the same answer as a full build).
///
/// Accepted: `self`/`cls` resolve via the enclosing class; a plain identifier
/// resolves via the nearest preceding `name = Type(...)` assignment or annotated
/// parameter of that name. Anything else — attribute chains (`self.x.method()`),
/// subscripts, temporaries — returns `None`, keeping the daemon on its
/// fail-closed unique-leaf path. `None` is always the safe answer.
fn resolve_receiver_type_py(
    recv: tree_sitter::Node<'_>,
    source: &[u8],
    enclosing_class: &Option<String>,
) -> Option<String> {
    if recv.kind() != "identifier" {
        return None;
    }
    let name = recv.utf8_text(source).ok()?;
    if name == "self" || name == "cls" {
        return enclosing_class.clone();
    }
    let enclosing_fn = enclosing_function_def_py(recv)?;
    nearest_preceding_binding_type_py(enclosing_fn, source, name, recv.start_byte())
}

/// Walk up to the nearest enclosing `function_definition` node itself (as opposed
/// to [`find_enclosing_fn_py`], which derives its name/class).
fn enclosing_function_def_py(node: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    let mut cur = node.parent()?;
    loop {
        match cur.kind() {
            "function_definition" => return Some(cur),
            "module" => return None,
            _ => {}
        }
        cur = cur.parent()?;
    }
}

/// Type of the nearest binding of `name` starting before `before_byte`: an
/// annotated parameter (`def f(name: Type)`) or an assignment to a capitalized
/// constructor call (`name = Type(...)`). Nearest = greatest start byte among
/// candidates. Only these two syntactically unambiguous shapes are used; a plain
/// identifier / subscript / union annotation yields `None`.
fn nearest_preceding_binding_type_py(
    enclosing_fn: tree_sitter::Node<'_>,
    source: &[u8],
    name: &str,
    before_byte: usize,
) -> Option<String> {
    let mut best: Option<(usize, String)> = None;

    // Annotated parameters — `typed_parameter` / `typed_default_parameter`.
    if let Some(params) = enclosing_fn.child_by_field_name("parameters") {
        let mut c = params.walk();
        for p in params.children(&mut c) {
            let matches_name = match p.kind() {
                "typed_parameter" => {
                    p.child(0).and_then(|n| n.utf8_text(source).ok()) == Some(name)
                }
                "typed_default_parameter" => {
                    p.child_by_field_name("name")
                        .and_then(|n| n.utf8_text(source).ok())
                        == Some(name)
                }
                _ => false,
            };
            if !matches_name {
                continue;
            }
            let Some(ty) = p
                .child_by_field_name("type")
                .and_then(|t| type_name_py(t, source))
            else {
                continue;
            };
            if p.start_byte() < before_byte
                && best.as_ref().map_or(true, |(b, _)| p.start_byte() > *b)
            {
                best = Some((p.start_byte(), ty));
            }
        }
    }

    // Assignments `name = Type(...)` in the function body.
    if let Some(body) = enclosing_fn.child_by_field_name("body") {
        collect_nearest_assign_py(body, source, name, before_byte, &mut best);
    }

    best.map(|(_, t)| t)
}

/// Bare type name from a Python annotation node. Accepts `Foo` (optionally
/// wrapped in a `type` node); rejects subscripts/unions (`List[Foo]`, `A | B`)
/// where the receiver's concrete type is not a single resolvable name.
fn type_name_py(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    match node.kind() {
        "type" => node.child(0).and_then(|c| type_name_py(c, source)),
        "identifier" => node.utf8_text(source).ok().map(str::to_string),
        _ => None,
    }
}

/// Recurse through `node` collecting `name = Type(...)` assignments starting
/// before `before_byte`, keeping the nearest preceding one in `best`. Does not
/// descend into nested `function_definition` bodies — a binding local to a
/// nested fn is never visible to a receiver outside it.
fn collect_nearest_assign_py(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    name: &str,
    before_byte: usize,
    best: &mut Option<(usize, String)>,
) {
    if node.kind() == "assignment" {
        if let (Some(left), Some(right)) = (
            node.child_by_field_name("left"),
            node.child_by_field_name("right"),
        ) {
            if left.kind() == "identifier"
                && left.utf8_text(source) == Ok(name)
                && right.kind() == "call"
            {
                if let Some(func) = right.child_by_field_name("function") {
                    if func.kind() == "identifier" {
                        if let Ok(t) = func.utf8_text(source) {
                            if t.starts_with(|c: char| c.is_uppercase())
                                && node.start_byte() < before_byte
                                && best.as_ref().map_or(true, |(b, _)| node.start_byte() > *b)
                            {
                                *best = Some((node.start_byte(), t.to_string()));
                            }
                        }
                    }
                }
            }
        }
    }
    if node.kind() == "function_definition" {
        return;
    }
    let mut c = node.walk();
    for child in node.children(&mut c) {
        collect_nearest_assign_py(child, source, name, before_byte, best);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Run the real `CALL_QUERY` + `resolve_receiver_type_py` path end-to-end
    /// against `source`, returning the recovered receiver type for the
    /// `.method_name()` call site. Exercises the exact query/capture wiring the
    /// extractor uses, not a hand-rolled shortcut.
    fn recv_type_for_call(source: &[u8], method_name: &str) -> Option<String> {
        let language = tree_sitter::Language::new(tree_sitter_python::LANGUAGE);
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
                let caller_class = find_enclosing_fn_py(cap.node, source).and_then(|(_, c)| c);
                let recv = m.captures.iter().find(|c| {
                    cap_names.get(c.index as usize).map(String::as_str) == Some("call.recv")
                });
                return recv.and_then(|r| resolve_receiver_type_py(r.node, source, &caller_class));
            }
        }
        None
    }

    #[test]
    fn recv_self_resolves_to_enclosing_class() {
        let source = br#"
class Session:
    def run(self):
        self.helper()
"#;
        assert_eq!(
            recv_type_for_call(source, "helper"),
            Some("Session".to_string())
        );
    }

    #[test]
    fn recv_constructor_assignment_resolves() {
        let source = br#"
def build():
    store = SqliteStore()
    store.open()
"#;
        assert_eq!(
            recv_type_for_call(source, "open"),
            Some("SqliteStore".to_string())
        );
    }

    #[test]
    fn recv_annotated_parameter_resolves() {
        let source = br#"
def handle(self, session: Session):
    session.close()
"#;
        assert_eq!(
            recv_type_for_call(source, "close"),
            Some("Session".to_string())
        );
    }

    #[test]
    fn recv_attribute_chain_is_none() {
        // `self.other.run()` — receiver is an attribute chain, not a plain
        // identifier or `self`. Must NOT resolve (no false self-class edge).
        let source = br#"
class App:
    def run(self):
        self.other.run()
"#;
        assert_eq!(recv_type_for_call(source, "run"), None);
    }

    #[test]
    fn recv_unbound_identifier_is_none() {
        let source = br#"
def f(x):
    x.method()
"#;
        assert_eq!(recv_type_for_call(source, "method"), None);
    }

    #[test]
    fn emits_unresolved_calls_not_raw_edges() {
        // A cross-file method call must become an UnresolvedCall (resolved
        // fail-closed by the daemon), never a same-file guessed edge.
        let dir = std::env::temp_dir().join(format!("e4_py_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("m.py");
        std::fs::write(
            &file,
            b"class Session:\n    def run(self):\n        self.helper()\n",
        )
        .unwrap();
        let files = vec![(file.clone(), "m.py".to_string())];
        let (_nodes, edges, unresolved) = extract_native_phase_b("c", &dir, Some(&files)).unwrap();
        // No raw ref/call edges from call sites any more.
        assert!(
            edges.iter().all(|e| e.kind != EdgeKind::RefCall),
            "call sites must not emit raw RefCall edges"
        );
        let helper = unresolved
            .iter()
            .find(|u| u.callee_sig == "fn:helper")
            .expect("helper() emitted as UnresolvedCall");
        assert!(helper.is_method_call);
        assert_eq!(helper.recv_type.as_deref(), Some("Session"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
