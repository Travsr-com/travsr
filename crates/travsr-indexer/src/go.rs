use std::path::Path;

use anyhow::Context as _;
use streaming_iterator::StreamingIterator as _;
use travsr_core::{Node, VName};
use tree_sitter::{Parser, Query, QueryCursor};

use crate::emit;
use crate::ffi::{FfiMarker, FfiMarkerKind};
use crate::ParseOutput;

const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024; // 10 MB

const PARSE_TIMEOUT_MICROS: u64 = 5_000_000; // 5 seconds

const _: () = {
    assert!(PARSE_TIMEOUT_MICROS >= 1_000_000);
    assert!(PARSE_TIMEOUT_MICROS <= 30_000_000);
};

// ── Supported Go features (Phase 3, Sprint 12 — INDEX-170) ───────────────────
//
// Supported (structural nodes, emitted by this parser):
//   - Top-level function declarations  → kind="function",  sig="fn:<name>"
//   - Method declarations              → kind="method",    sig="method:<ReceiverType>.<name>"
//   - Struct type declarations         → kind="class",     sig="class:<name>"
//   - Interface type declarations      → kind="interface", sig="interface:<name>"
//   - Type alias (type X = Y)          → kind="type",      sig="type:<name>"
//   - Top-level var / const declarations→ kind="var",      sig="var:<name>"
//   - Import declarations              → kind="import",    sig="import:<path>"
//   - Package membership               → kind="go-pkg",    sig="go-pkg:<dir>/<pkg>"
//
// The go-pkg node's VName path is set to the parent directory (not the file
// path) so that every file in the same package directory hashes to the same
// NodeId. This allows the daemon's co-package pass to group co-package files
// by NodeId without any cross-file state.
//
// Known gaps (Phase 3 / DEBT-018):
//   - Build tags (`//go:build`) are ignored — all files indexed regardless
//   - vendor/ directories skipped by the daemon WalkBuilder (not this parser)
//   - Generic type parameters dropped: `func Foo[T any]()` → sig="fn:Foo"
//   - `import "C"` (cgo pseudo-package) emitted and then filtered in link_imports_go
//   - Call/ref edges deferred to Phase 4 (Go LSIF / pyright-equivalent)
//   - Multi-value var blocks: only first name in each spec is indexed
//   - Methods on generic types (*Stack[T]) silently dropped — DEBT-020
//   - Co-package edges only refreshed on `travsr init`, not per-commit — DEBT-024

// ── Tree-sitter queries ───────────────────────────────────────────────────────
//
// Captures:
//   @fn.name     — identifier in a function_declaration (no receiver)
//   @method.name — field_identifier in a method_declaration
//   @recv.type   — receiver type name for method qualification
//   @class.name  — type_spec name whose type is struct_type
//   @iface.name  — type_spec name whose type is interface_type
//   @type.name   — type_spec name for any other type kind
//   @var.name    — identifier in a var_spec or const_spec (top-level only)
//   @import      — import_spec node
const QUERIES: &str = r#"
(function_declaration name: (identifier) @fn.name)
(method_declaration
  receiver: (parameter_list
    (parameter_declaration
      [
        (type_identifier) @recv.type
        (pointer_type (type_identifier) @recv.type)
      ]
    )
  )
  name: (field_identifier) @method.name)
(type_declaration
  (type_spec
    name: (type_identifier) @class.name
    type: (struct_type)))
(type_declaration
  (type_spec
    name: (type_identifier) @iface.name
    type: (interface_type)))
(type_declaration
  (type_alias
    name: (type_identifier) @type.name))
(type_declaration
  (type_spec
    name: (type_identifier) @type.name
    type: [
      (type_identifier)
      (qualified_type)
      (map_type)
      (slice_type)
      (array_type)
      (function_type)
      (channel_type)
      (pointer_type)
      (generic_type)
    ]))
(source_file
  (var_declaration
    (var_spec
      name: (identifier) @var.name)))
(source_file
  (const_declaration
    (const_spec
      name: (identifier) @var.name)))
(import_spec) @import
(package_clause
  (package_identifier) @pkg.name)
"#;

/// Parse `abs_path` and emit graph records using `vname_path` as the stable
/// VName path (repo-relative, forward-slash).
pub fn parse(corpus: &str, abs_path: &Path, vname_path: &str) -> anyhow::Result<ParseOutput> {
    let file_size = std::fs::metadata(abs_path)
        .with_context(|| format!("stat {}", abs_path.display()))?
        .len();
    if file_size > MAX_FILE_BYTES {
        anyhow::bail!(
            "skipping {}: file is too large ({} bytes > {} byte limit)",
            abs_path.display(),
            file_size,
            MAX_FILE_BYTES
        );
    }

    let source =
        std::fs::read(abs_path).with_context(|| format!("reading {}", abs_path.display()))?;

    let language = tree_sitter::Language::new(tree_sitter_go::LANGUAGE);
    let file_node = go_file_node(corpus, vname_path);
    let file_id = file_node.id;

    let mut output = ParseOutput {
        nodes: vec![file_node],
        edges: vec![],
        ffi_markers: vec![],
    };

    let mut parser = Parser::new();
    parser
        .set_language(&language)
        .context("loading Go grammar")?;

    let tree = match parser.parse(&source, None) {
        Some(t) => t,
        None => {
            tracing::warn!(
                "parse timed out for {} after {}s — emitting file node only",
                abs_path.display(),
                PARSE_TIMEOUT_MICROS / 1_000_000
            );
            return Ok(output);
        }
    };

    let query = Query::new(&language, QUERIES).context("compiling Go tree-sitter query")?;

    let capture_names: Vec<String> = query
        .capture_names()
        .iter()
        .map(|s| s.to_string())
        .collect();

    // Use matches() (not captures()) — the captures() iterator reports captures
    // lazily, so m.captures is incomplete when processing early captures of a
    // multi-capture pattern (e.g. recv.type before method.name in a method).
    // matches() provides all captures for the full pattern match in one shot.
    // We process one match at a time (no collect()) so the buffer is fresh.
    let mut cursor = QueryCursor::new();
    let mut iter = cursor.matches(&query, tree.root_node(), source.as_slice());

    // Helper used inside the loop: find capture text by name within a match.
    let find_cap_text = |m: &tree_sitter::QueryMatch<'_, '_>, name: &str| -> Option<String> {
        m.captures
            .iter()
            .find(|c| capture_names.get(c.index as usize).map(|s| s.as_str()) == Some(name))
            .and_then(|c| c.node.utf8_text(source.as_slice()).ok())
            .map(str::to_owned)
    };

    // G2: walk up from a capture node to the nearest body-containing declaration
    // and return its 1-based end line.  Handles function_declaration (1 level),
    // method_declaration (3 levels from recv.type), and type_declaration (2 levels).
    // Falls back to the capture's own start line when no declaration is found.
    let decl_end_line = |node: tree_sitter::Node<'_>| -> u32 {
        let mut cur = node;
        for _ in 0..5 {
            match cur.parent() {
                Some(parent) => match parent.kind() {
                    "function_declaration"
                    | "method_declaration"
                    | "type_declaration"
                    | "const_declaration"
                    | "var_declaration" => return parent.end_position().row as u32 + 1,
                    _ => cur = parent,
                },
                None => break,
            }
        }
        node.start_position().row as u32 + 1
    };

    while let Some(m) = iter.next() {
        // Determine which pattern fired by inspecting the anchor capture — each
        // pattern in QUERIES has a unique first capture name.
        let Some(anchor) = m.captures.first() else {
            continue;
        };
        let Some(anchor_name) = capture_names.get(anchor.index as usize) else {
            continue;
        };

        match anchor_name.as_str() {
            "fn.name" => {
                let Some(name) = find_cap_text(m, "fn.name") else {
                    continue;
                };
                let name = strip_generics(&name).to_owned();
                let line = anchor.node.start_position().row as u32 + 1;
                let node = go_fn_node(corpus, vname_path, &name)
                    .with_line(line)
                    .with_end_line(decl_end_line(anchor.node));
                output.edges.push(emit::defines_edge(file_id, node.id));
                output.nodes.push(node);
            }
            "recv.type" => {
                // Method: all captures (recv.type + method.name) are in m.captures.
                let Some(recv) = find_cap_text(m, "recv.type") else {
                    continue;
                };
                let Some(method_name) = find_cap_text(m, "method.name") else {
                    continue;
                };
                let recv = strip_generics(&recv).to_owned();
                let class_id = go_class_node(corpus, vname_path, &recv).id;
                // Use method.name capture for the line, not recv.type (anchor) —
                // recv.type points to the receiver identifier, not the method name.
                let line = m
                    .captures
                    .iter()
                    .find(|c| {
                        capture_names.get(c.index as usize).map(|s| s.as_str())
                            == Some("method.name")
                    })
                    .map(|c| c.node.start_position().row as u32 + 1)
                    .unwrap_or_else(|| anchor.node.start_position().row as u32 + 1);
                // decl_end_line walks up from recv.type → parameter_declaration →
                // parameter_list → method_declaration in at most 3 hops.
                let node = go_method_node(corpus, vname_path, &recv, &method_name)
                    .with_line(line)
                    .with_end_line(decl_end_line(anchor.node));
                output.edges.push(emit::defines_edge(class_id, node.id));
                output.nodes.push(node);
            }
            "class.name" => {
                let Some(name) = find_cap_text(m, "class.name") else {
                    continue;
                };
                let name = strip_generics(&name).to_owned();
                let line = anchor.node.start_position().row as u32 + 1;
                let node = go_class_node(corpus, vname_path, &name)
                    .with_line(line)
                    .with_end_line(decl_end_line(anchor.node));
                output.edges.push(emit::defines_edge(file_id, node.id));
                output.nodes.push(node);
            }
            "iface.name" => {
                let Some(name) = find_cap_text(m, "iface.name") else {
                    continue;
                };
                let name = strip_generics(&name).to_owned();
                let line = anchor.node.start_position().row as u32 + 1;
                let node = go_iface_node(corpus, vname_path, &name)
                    .with_line(line)
                    .with_end_line(decl_end_line(anchor.node));
                output.edges.push(emit::defines_edge(file_id, node.id));
                output.nodes.push(node);
            }
            "type.name" => {
                let Some(name) = find_cap_text(m, "type.name") else {
                    continue;
                };
                let name = strip_generics(&name).to_owned();
                let line = anchor.node.start_position().row as u32 + 1;
                let node = go_type_node(corpus, vname_path, &name)
                    .with_line(line)
                    .with_end_line(decl_end_line(anchor.node));
                output.edges.push(emit::defines_edge(file_id, node.id));
                output.nodes.push(node);
            }
            "var.name" => {
                let Some(name) = find_cap_text(m, "var.name") else {
                    continue;
                };
                let line = anchor.node.start_position().row as u32 + 1;
                let node = go_var_node(corpus, vname_path, &name)
                    .with_line(line)
                    .with_end_line(decl_end_line(anchor.node));
                output.edges.push(emit::defines_edge(file_id, node.id));
                output.nodes.push(node);
            }
            "import" => {
                let import_node = anchor.node;
                if let Some(path_lit) = extract_import_path(import_node, source.as_slice()) {
                    if path_lit == "C" {
                        continue;
                    }
                    let node = go_import_node(corpus, vname_path, &path_lit);
                    output.edges.push(emit::depends_edge(file_id, node.id));
                    output.nodes.push(node);
                }
            }
            "pkg.name" => {
                let Some(pkg_name) = find_cap_text(m, "pkg.name") else {
                    continue;
                };
                // Use the parent directory as the VName path so that every file
                // in the same package directory produces the identical NodeId.
                // This is the key invariant that lets the daemon's co-package pass
                // group co-package files by a shared NodeId without cross-file state.
                let parent_dir = std::path::Path::new(vname_path)
                    .parent()
                    .and_then(|p| p.to_str())
                    .unwrap_or("");
                let parent_dir = if parent_dir.is_empty() {
                    ".".to_owned()
                } else {
                    parent_dir.replace('\\', "/")
                };
                let sig = format!("go-pkg:{parent_dir}/{pkg_name}");
                let pkg_node = Node::new(VName::new(corpus, "", &parent_dir, "go", &sig), "go-pkg");
                output.edges.push(emit::defines_edge(file_id, pkg_node.id));
                output.nodes.push(pkg_node);
            }
            _ => {}
        }
    }

    output.nodes.sort_unstable_by_key(|n| n.id);
    output.nodes.dedup_by_key(|n| n.id);
    output.edges.sort_unstable_by_key(|e| (e.src, e.dst));
    output
        .edges
        .dedup_by(|a, b| a.src == b.src && a.dst == b.dst && a.kind == b.kind);

    // Collect cgo FFI markers and synthetic C destination nodes (RFC-005).
    let (cgo_markers, c_nodes) = collect_cgo_markers(corpus, vname_path, &source);
    output.nodes.extend(c_nodes);
    output.ffi_markers.extend(cgo_markers);

    Ok(output)
}

/// Extract the unquoted import path from an `import_spec` AST node.
///
/// The grammar produces:
/// ```text
/// (import_spec path: (interpreted_string_literal))
/// (import_spec name: (identifier) path: (interpreted_string_literal))
/// ```
///
/// Returns the bare path string without surrounding quotes, e.g. `"fmt"` → `fmt`.
fn extract_import_path(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    // Try the `path` named field first (tree-sitter-go ≥ 0.20).
    if let Some(path_node) = node.child_by_field_name("path") {
        let raw = path_node.utf8_text(source).ok()?;
        return Some(unquote(raw));
    }
    // Fallback: walk children for the interpreted_string_literal.
    for i in 0..node.child_count() {
        let child = node.child(i as u32)?;
        if child.kind() == "interpreted_string_literal" {
            let raw = child.utf8_text(source).ok()?;
            return Some(unquote(raw));
        }
    }
    None
}

/// Strip surrounding double-quotes from a Go string literal token.
fn unquote(s: &str) -> String {
    s.trim_matches('"').to_owned()
}

/// Strip generic type parameters from a Go identifier.
/// `Handler[T]` → `Handler`, `Foo` → `Foo`.
fn strip_generics(s: &str) -> &str {
    if let Some(idx) = s.find('[') {
        &s[..idx]
    } else {
        s
    }
}

// ── Node constructors ─────────────────────────────────────────────────────────

fn go_file_node(corpus: &str, path: &str) -> Node {
    Node::new(go_vname(corpus, path, "file"), "file")
}

fn go_fn_node(corpus: &str, path: &str, name: &str) -> Node {
    Node::new(go_vname(corpus, path, &format!("fn:{name}")), "function")
}

fn go_method_node(corpus: &str, path: &str, recv: &str, method: &str) -> Node {
    Node::new(
        go_vname(corpus, path, &format!("method:{recv}.{method}")),
        "method",
    )
}

fn go_class_node(corpus: &str, path: &str, name: &str) -> Node {
    Node::new(go_vname(corpus, path, &format!("class:{name}")), "class")
}

fn go_iface_node(corpus: &str, path: &str, name: &str) -> Node {
    Node::new(
        go_vname(corpus, path, &format!("interface:{name}")),
        "interface",
    )
}

fn go_type_node(corpus: &str, path: &str, name: &str) -> Node {
    Node::new(go_vname(corpus, path, &format!("type:{name}")), "type")
}

fn go_var_node(corpus: &str, path: &str, name: &str) -> Node {
    Node::new(go_vname(corpus, path, &format!("var:{name}")), "var")
}

fn go_import_node(corpus: &str, path: &str, import_path: &str) -> Node {
    Node::new(
        go_vname(corpus, path, &format!("import:{import_path}")),
        "import",
    )
}

fn go_vname(corpus: &str, path: &str, signature: &str) -> VName {
    VName::new(corpus, "", path, "go", signature)
}

// ── FFI marker collection (RFC-005, cgo) ─────────────────────────────────────

/// Scan for cgo FFI markers: `import "C"` blocks, `//export` directives,
/// and `C.<name>` call sites. Also synthesizes destination C VNames.
///
/// Returns (markers, synthetic_c_nodes) — the synthetic nodes give the
/// ffi_resolver a destination NodeId for Go→C edges.
pub(crate) fn collect_cgo_markers(
    corpus: &str,
    vname_path: &str,
    source: &[u8],
) -> (Vec<FfiMarker>, Vec<Node>) {
    let mut markers = Vec::new();
    let mut c_nodes = Vec::new();

    let source_str = match std::str::from_utf8(source) {
        Ok(s) => s,
        Err(_) => return (markers, c_nodes),
    };

    let has_cgo = source_str.contains(r#"import "C""#);
    if !has_cgo {
        return (markers, c_nodes);
    }

    // Scan for //export <Name> directives
    for line in source_str.lines() {
        let trimmed = line.trim();
        if let Some(export_name) = trimmed.strip_prefix("//export ") {
            let name = export_name.trim();
            if name.is_empty() {
                continue;
            }
            // Synthetic C destination VName (RFC-005: <cgo_synthetic>/ prefix)
            let basename = std::path::Path::new(vname_path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown.go");
            let c_path = format!("<cgo_synthetic>/{basename}");
            let c_vname = VName::new(corpus, "", &c_path, "c", format!("fn:{name}"));
            let c_node = Node::new(c_vname.clone(), "function");
            let c_node_id = c_node.id;
            c_nodes.push(c_node);

            // Go-side export marker
            let go_vname = VName::new(corpus, "", vname_path, "go", format!("fn:{name}"));
            if let Some(m) = FfiMarker::try_new(
                go_vname.id(),
                FfiMarkerKind::CgoExport,
                name,
                None::<String>,
                None,
                None::<String>,
                corpus,
            ) {
                markers.push(m);
            }

            // C-side destination marker (so resolver can match Go→C)
            if let Some(m) = FfiMarker::try_new(
                c_node_id,
                FfiMarkerKind::GoCallC,
                name,
                None::<String>,
                None,
                Some("cgo"),
                corpus,
            ) {
                markers.push(m);
            }
        }
    }

    (markers, c_nodes)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/go/main.go")
    }

    fn handler_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/go/handler.go")
    }

    #[test]
    fn oversized_file_returns_err() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.go");
        let big = vec![b'a'; (MAX_FILE_BYTES + 1) as usize];
        std::fs::write(&path, &big).unwrap();
        let result = parse("", &path, "big.go");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("too large"));
    }

    #[test]
    fn malformed_go_still_emits_file_node() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.go");
        std::fs::write(&path, b"func(struct(import").unwrap();
        let out = parse("", &path, "bad.go").unwrap();
        assert!(!out.nodes.is_empty());
        assert!(
            out.nodes.iter().any(|n| n.kind == "file"),
            "expected file node even for malformed input"
        );
    }

    #[test]
    fn language_field_is_go() {
        let out = parse("", &fixture_path(), "main.go").unwrap();
        for node in &out.nodes {
            assert_eq!(node.vname.language, "go");
        }
    }

    #[test]
    fn vname_path_is_respected() {
        let out = parse("", &fixture_path(), "custom/path.go").unwrap();
        for node in &out.nodes {
            if node.kind == "go-pkg" {
                // go-pkg nodes use the parent directory as their VName path so
                // that all files in the same package directory share one NodeId.
                assert_eq!(
                    node.vname.path, "custom",
                    "go-pkg node must use parent dir as vname.path"
                );
            } else {
                assert_eq!(
                    node.vname.path, "custom/path.go",
                    "non-pkg node must use the file's vname_path"
                );
            }
        }
    }

    #[test]
    fn golden_main_go_emits_expected_kinds() {
        let out = parse("", &fixture_path(), "main.go").unwrap();
        let kinds: std::collections::HashSet<&str> =
            out.nodes.iter().map(|n| n.kind.as_str()).collect();
        assert!(kinds.contains("file"), "missing file node");
        assert!(kinds.contains("function"), "missing function node");
        assert!(kinds.contains("import"), "missing import node");
    }

    #[test]
    fn top_level_fn_has_fn_signature() {
        let out = parse("", &fixture_path(), "main.go").unwrap();
        assert!(
            out.nodes
                .iter()
                .any(|n| n.vname.signature == "fn:main" && n.kind == "function"),
            "expected fn:main function node"
        );
    }

    #[test]
    fn handler_go_emits_struct_as_class() {
        let out = parse("", &handler_path(), "handler.go").unwrap();
        assert!(
            out.nodes
                .iter()
                .any(|n| n.vname.signature == "class:Handler" && n.kind == "class"),
            "expected class:Handler from struct type"
        );
    }

    #[test]
    fn method_has_receiver_qualified_signature() {
        let out = parse("", &handler_path(), "handler.go").unwrap();
        assert!(
            out.nodes
                .iter()
                .any(|n| n.vname.signature == "method:Handler.ServeHTTP" && n.kind == "method"),
            "expected method:Handler.ServeHTTP"
        );
    }

    #[test]
    fn pointer_receiver_method_uses_base_type() {
        let out = parse("", &handler_path(), "handler.go").unwrap();
        assert!(
            out.nodes
                .iter()
                .any(|n| n.vname.signature == "method:Handler.Handle" && n.kind == "method"),
            "expected method:Handler.Handle for pointer receiver *Handler"
        );
    }

    #[test]
    fn interface_node_emitted() {
        let out = parse("", &handler_path(), "handler.go").unwrap();
        assert!(
            out.nodes
                .iter()
                .any(|n| n.vname.signature == "interface:Doer" && n.kind == "interface"),
            "expected interface:Doer"
        );
    }

    #[test]
    fn import_nodes_emitted() {
        let out = parse("", &fixture_path(), "main.go").unwrap();
        assert!(
            out.nodes
                .iter()
                .any(|n| n.vname.signature == "import:fmt" && n.kind == "import"),
            "expected import:fmt"
        );
    }

    #[test]
    fn file_defines_function_edge() {
        use travsr_core::EdgeKind;
        let out = parse("", &fixture_path(), "main.go").unwrap();
        let file_id = out.nodes.iter().find(|n| n.kind == "file").unwrap().id;
        let fn_id = out
            .nodes
            .iter()
            .find(|n| n.vname.signature == "fn:main")
            .unwrap()
            .id;
        assert!(
            out.edges
                .iter()
                .any(|e| e.src == file_id && e.dst == fn_id && e.kind == EdgeKind::DefinesBinding),
            "expected file → fn:main DefinesBinding edge"
        );
    }

    #[test]
    fn class_defines_method_edge() {
        use travsr_core::EdgeKind;
        let out = parse("", &handler_path(), "handler.go").unwrap();
        let class_id = out
            .nodes
            .iter()
            .find(|n| n.vname.signature == "class:Handler")
            .unwrap()
            .id;
        let method_id = out
            .nodes
            .iter()
            .find(|n| n.vname.signature == "method:Handler.ServeHTTP")
            .unwrap()
            .id;
        assert!(
            out.edges.iter().any(|e| e.src == class_id
                && e.dst == method_id
                && e.kind == EdgeKind::DefinesBinding),
            "expected class:Handler → method:Handler.ServeHTTP DefinesBinding edge"
        );
    }

    #[test]
    fn file_depends_import_edge() {
        use travsr_core::EdgeKind;
        let out = parse("", &fixture_path(), "main.go").unwrap();
        let file_id = out.nodes.iter().find(|n| n.kind == "file").unwrap().id;
        let import_id = out
            .nodes
            .iter()
            .find(|n| n.vname.signature == "import:fmt")
            .unwrap()
            .id;
        assert!(
            out.edges
                .iter()
                .any(|e| e.src == file_id && e.dst == import_id && e.kind == EdgeKind::Depends),
            "expected file → import:fmt Depends edge"
        );
    }

    #[test]
    fn cgo_import_filtered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cgo.go");
        std::fs::write(&path, b"package main\nimport \"C\"\n").unwrap();
        let out = parse("", &path, "cgo.go").unwrap();
        assert!(
            !out.nodes.iter().any(|n| n.vname.signature == "import:C"),
            "import:C (cgo) must be filtered out"
        );
    }

    #[test]
    fn generic_function_drops_type_params() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gen.go");
        std::fs::write(
            &path,
            b"package main\nfunc Map[T any](xs []T) []T { return xs }\n",
        )
        .unwrap();
        let out = parse("", &path, "gen.go").unwrap();
        assert!(
            out.nodes
                .iter()
                .any(|n| n.vname.signature == "fn:Map" && n.kind == "function"),
            "expected fn:Map (no type params)"
        );
    }

    #[test]
    fn var_const_nodes_emitted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vars.go");
        std::fs::write(
            &path,
            b"package main\nvar Version = \"1.0\"\nconst MaxSize = 100\n",
        )
        .unwrap();
        let out = parse("", &path, "vars.go").unwrap();
        assert!(
            out.nodes
                .iter()
                .any(|n| n.vname.signature == "var:Version" && n.kind == "var"),
            "expected var:Version"
        );
        assert!(
            out.nodes
                .iter()
                .any(|n| n.vname.signature == "var:MaxSize" && n.kind == "var"),
            "expected var:MaxSize from const"
        );
    }

    #[test]
    fn type_alias_node_emitted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("types.go");
        std::fs::write(&path, b"package main\ntype ID = string\n").unwrap();
        let out = parse("", &path, "types.go").unwrap();
        assert!(
            out.nodes
                .iter()
                .any(|n| n.vname.signature == "type:ID" && n.kind == "type"),
            "expected type:ID"
        );
    }

    #[test]
    fn named_type_definitions_emitted() {
        // DEBT-019 / #317: `type X Y` (no `=`) over non-struct underlying types
        // must produce nodes — SCIP marks them as types and G1 needs a
        // tree-sitter counterpart to unify onto.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("named.go");
        std::fs::write(
            &path,
            b"package main\ntype Disposition string\ntype DocumentMap map[string]int\ntype EnsureRBACFunc func(int) error\n",
        )
        .unwrap();
        let out = parse("", &path, "named.go").unwrap();
        for name in ["Disposition", "DocumentMap", "EnsureRBACFunc"] {
            assert!(
                out.nodes
                    .iter()
                    .any(|n| n.vname.signature == format!("type:{name}") && n.kind == "type"),
                "expected type:{name}"
            );
        }
    }

    #[test]
    fn idempotent_parse() {
        let out1 = parse("", &fixture_path(), "main.go").unwrap();
        let out2 = parse("", &fixture_path(), "main.go").unwrap();
        assert_eq!(out1.nodes.len(), out2.nodes.len());
        assert_eq!(out1.edges.len(), out2.edges.len());
    }

    #[test]
    fn no_duplicate_nodes() {
        let out = parse("", &handler_path(), "handler.go").unwrap();
        let mut ids: Vec<_> = out.nodes.iter().map(|n| n.id).collect();
        ids.sort_unstable();
        let orig_len = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), orig_len, "duplicate NodeIds found");
    }

    #[test]
    fn no_duplicate_edges() {
        let out = parse("", &handler_path(), "handler.go").unwrap();
        let mut edges: Vec<_> = out
            .edges
            .iter()
            .map(|e| (e.src.0, e.dst.0, e.kind.as_str()))
            .collect();
        edges.sort_unstable();
        let orig_len = edges.len();
        edges.dedup();
        assert_eq!(edges.len(), orig_len, "duplicate edges found");
    }

    #[test]
    fn go_pkg_node_emitted_for_package_clause() {
        use travsr_core::EdgeKind;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.go");
        std::fs::write(&path, b"package strategies\nfunc New() {}\n").unwrap();
        let out = parse("", &path, "strategies/server.go").unwrap();

        let pkg_node = out
            .nodes
            .iter()
            .find(|n| n.kind == "go-pkg")
            .expect("expected go-pkg node for package clause");
        assert_eq!(
            pkg_node.vname.signature, "go-pkg:strategies/strategies",
            "go-pkg sig must encode parent_dir/pkg_name"
        );
        assert_eq!(
            pkg_node.vname.path, "strategies",
            "go-pkg vname.path must be parent dir, not file path"
        );

        let file_id = out.nodes.iter().find(|n| n.kind == "file").unwrap().id;
        assert!(
            out.edges.iter().any(|e| e.src == file_id
                && e.dst == pkg_node.id
                && e.kind == EdgeKind::DefinesBinding),
            "expected file → go-pkg DefinesBinding edge"
        );
    }

    #[test]
    fn go_pkg_node_root_level_uses_dot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.go");
        std::fs::write(&path, b"package main\nfunc main() {}\n").unwrap();
        let out = parse("", &path, "main.go").unwrap();
        let pkg_node = out
            .nodes
            .iter()
            .find(|n| n.kind == "go-pkg")
            .expect("expected go-pkg node for root-level file");
        assert_eq!(
            pkg_node.vname.signature, "go-pkg:./main",
            "root-level file must use '.' as parent_dir in go-pkg sig"
        );
        assert_eq!(
            pkg_node.vname.path, ".",
            "root-level go-pkg vname.path must be '.'"
        );
    }

    #[test]
    fn go_pkg_node_is_exactly_one_per_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.go");
        std::fs::write(&path, b"package strategies\nfunc A() {}\nfunc B() {}\n").unwrap();
        let out = parse("", &path, "strategies/a.go").unwrap();
        let pkg_count = out.nodes.iter().filter(|n| n.kind == "go-pkg").count();
        assert_eq!(pkg_count, 1, "exactly one go-pkg node per file after dedup");
    }

    #[test]
    fn co_package_files_share_same_pkg_node_id() {
        let dir = tempfile::tempdir().unwrap();
        let path_a = dir.path().join("a.go");
        let path_b = dir.path().join("b.go");
        std::fs::write(&path_a, b"package strategies\nfunc A() {}\n").unwrap();
        std::fs::write(&path_b, b"package strategies\nfunc B() {}\n").unwrap();
        let out_a = parse("", &path_a, "strategies/a.go").unwrap();
        let out_b = parse("", &path_b, "strategies/b.go").unwrap();
        let pkg_id_a = out_a
            .nodes
            .iter()
            .find(|n| n.kind == "go-pkg")
            .map(|n| n.id);
        let pkg_id_b = out_b
            .nodes
            .iter()
            .find(|n| n.kind == "go-pkg")
            .map(|n| n.id);
        assert_eq!(
            pkg_id_a, pkg_id_b,
            "files in the same package directory must produce identical go-pkg NodeIds"
        );
    }

    #[test]
    fn different_packages_same_dir_produce_distinct_pkg_node_ids() {
        // External test packages (pkg_test convention) must get a distinct NodeId.
        let dir = tempfile::tempdir().unwrap();
        let path_a = dir.path().join("a.go");
        let path_b = dir.path().join("a_test.go");
        std::fs::write(&path_a, b"package mypkg\nfunc A() {}\n").unwrap();
        std::fs::write(&path_b, b"package mypkg_test\nfunc TestA() {}\n").unwrap();
        let out_a = parse("", &path_a, "mypkg/a.go").unwrap();
        let out_b = parse("", &path_b, "mypkg/a_test.go").unwrap();
        let pkg_id_a = out_a
            .nodes
            .iter()
            .find(|n| n.kind == "go-pkg")
            .map(|n| n.id);
        let pkg_id_b = out_b
            .nodes
            .iter()
            .find(|n| n.kind == "go-pkg")
            .map(|n| n.id);
        assert_ne!(
            pkg_id_a, pkg_id_b,
            "mypkg and mypkg_test must produce distinct go-pkg NodeIds"
        );
    }
}
