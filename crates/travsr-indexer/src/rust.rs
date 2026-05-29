use std::path::Path;

use anyhow::Context as _;
use travsr_core::{Node, VName};
use tree_sitter::{Parser, Query, QueryCursor};

use crate::emit;
use crate::ffi::{FfiMarker, FfiMarkerKind};
use crate::ParseOutput;

/// Reject files larger than this before reading them into memory.
const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024; // 10 MB

/// Kill the Tree-sitter parse if it hasn't finished within this window.
const PARSE_TIMEOUT_MICROS: u64 = 5_000_000; // 5 seconds

const _: () = {
    assert!(PARSE_TIMEOUT_MICROS >= 1_000_000);
    assert!(PARSE_TIMEOUT_MICROS <= 30_000_000);
};

// Captures: fn, struct, enum, trait, impl, mod (inline + file), const, static, use.
// Phase A (Sprint 8): structural definitions only; semantic call/ref edges
// are deferred to Phase B (Sprint 9, rust-analyzer LSIF).
//
// use_declaration is captured as a whole node (@use.decl) and walked
// recursively by extract_use_paths() to handle all use-tree variants:
//   use foo::bar;          → use:foo::bar
//   use foo::{bar, baz};   → use:foo::bar, use:foo::baz
//   use foo::bar as Baz;   → use:foo::bar  (alias ignored for graph identity)
//   use foo::*;            → use:foo::*    (wildcard; ResolvesTo skips these)
//
// mod_item is captured as @mod.name for both inline modules and file-system
// module declarations. The parse() match arm checks for the presence of a
// `body` field at runtime: no body → file-module node; has body → module node.
const QUERIES: &str = "
(function_item name: (identifier) @fn.name)
(struct_item name: (type_identifier) @struct.name)
(enum_item name: (type_identifier) @enum.name)
(trait_item name: (type_identifier) @trait.name)
(impl_item type: (type_identifier) @impl.name)
(impl_item type: (generic_type (type_identifier) @impl.name))
(mod_item name: (identifier) @mod.name)
(const_item name: (identifier) @const.name)
(static_item name: (identifier) @static.name)
(use_declaration) @use.decl
";

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

    let language = tree_sitter_rust::language();
    let file_node = rust_file_node(corpus, vname_path);
    let file_id = file_node.id;

    let mut output = ParseOutput {
        nodes: vec![file_node],
        edges: vec![],
        ffi_markers: vec![],
    };

    let mut parser = Parser::new();
    parser
        .set_language(&language)
        .context("loading Rust grammar")?;
    parser.set_timeout_micros(PARSE_TIMEOUT_MICROS);

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

    let query = Query::new(&language, QUERIES).context("compiling Rust tree-sitter query")?;

    let capture_names: Vec<String> = query
        .capture_names()
        .iter()
        .map(|s| s.to_string())
        .collect();

    let mut cursor = QueryCursor::new();
    let captures = cursor.captures(&query, tree.root_node(), source.as_slice());

    for (m, cap_idx) in captures {
        let capture = m.captures[cap_idx];
        let Some(cap_name) = capture_names.get(capture.index as usize) else {
            continue;
        };
        let text = match capture.node.utf8_text(source.as_slice()) {
            Ok(t) => t,
            Err(_) => continue,
        };

        let line = capture.node.start_position().row as u32 + 1;
        match cap_name.as_str() {
            "fn.name" => {
                // Functions inside impl blocks become methods; the parent impl
                // type is the namespace so signatures are `fn:TypeName.method`.
                let parent_impl = find_parent_impl_type(capture.node, source.as_slice());
                let (node, src_id) = if let Some(impl_type) = parent_impl {
                    let impl_id = rust_impl_node(corpus, vname_path, &impl_type).id;
                    let n = rust_method_node(corpus, vname_path, &impl_type, text).with_line(line);
                    (n, impl_id)
                } else {
                    let n = rust_fn_node(corpus, vname_path, text).with_line(line);
                    (n, file_id)
                };
                output.edges.push(emit::defines_edge(src_id, node.id));
                output.nodes.push(node);
            }
            "struct.name" => {
                let node = rust_struct_node(corpus, vname_path, text).with_line(line);
                output.edges.push(emit::defines_edge(file_id, node.id));
                output.nodes.push(node);
            }
            "enum.name" => {
                let node = rust_enum_node(corpus, vname_path, text).with_line(line);
                output.edges.push(emit::defines_edge(file_id, node.id));
                output.nodes.push(node);
            }
            "trait.name" => {
                let node = rust_trait_node(corpus, vname_path, text).with_line(line);
                output.edges.push(emit::defines_edge(file_id, node.id));
                output.nodes.push(node);
            }
            "impl.name" => {
                let node = rust_impl_node(corpus, vname_path, text).with_line(line);
                output.edges.push(emit::defines_edge(file_id, node.id));
                output.nodes.push(node);
            }
            "mod.name" => {
                // Distinguish `mod foo;` (file declaration, no body) from
                // `mod foo { … }` (inline module, has body) at the AST level.
                // capture.node is the `identifier` from `(mod_item name: (identifier))`,
                // so its parent is always the enclosing `mod_item` node.
                let has_body = capture
                    .node
                    .parent()
                    .and_then(|p| p.child_by_field_name("body"))
                    .is_some();
                let node = if has_body {
                    // Inline module — structural container.
                    rust_mod_node(corpus, vname_path, text).with_line(line)
                } else {
                    // File-system module declaration.
                    // link_imports_rust() resolves this to foo.rs / foo/mod.rs.
                    rust_filemod_node(corpus, vname_path, text).with_line(line)
                };
                output.edges.push(emit::defines_edge(file_id, node.id));
                output.nodes.push(node);
            }
            "const.name" => {
                let node = rust_const_node(corpus, vname_path, text).with_line(line);
                output.edges.push(emit::defines_edge(file_id, node.id));
                output.nodes.push(node);
            }
            "static.name" => {
                let node = rust_static_node(corpus, vname_path, text).with_line(line);
                output.edges.push(emit::defines_edge(file_id, node.id));
                output.nodes.push(node);
            }
            "use.decl" => {
                // Walk the full use-tree to extract every leaf path, including
                // grouped imports (`use std::{fmt, io}`) and renames.
                if let Some(arg) = capture.node.child_by_field_name("argument") {
                    let mut paths = Vec::new();
                    extract_use_paths(arg, "", source.as_slice(), &mut paths);
                    for path in paths {
                        let node = rust_use_node(corpus, vname_path, &path);
                        output.edges.push(emit::depends_edge(file_id, node.id));
                        output.nodes.push(node);
                    }
                }
            }
            _ => {}
        }
    }

    // Dedup: a type with both `impl T` and `impl Trait for T` emits the same
    // `impl:T` node twice. Canonicalise in-memory before returning so callers
    // (and the store's put_node) don't perform redundant writes.
    output.nodes.sort_unstable_by_key(|n| n.id);
    output.nodes.dedup_by_key(|n| n.id);
    output.edges.sort_unstable_by_key(|e| (e.src, e.dst));
    output
        .edges
        .dedup_by(|a, b| a.src == b.src && a.dst == b.dst && a.kind == b.kind);

    // Collect FFI markers for cross-language edge resolution (RFC-005).
    let ffi_markers = collect_ffi_markers(corpus, vname_path, &source, &tree);
    output.ffi_markers = ffi_markers;

    Ok(output)
}

/// Walk up the AST from `node` to find the nearest enclosing `impl_item`.
/// Returns the implementing **type** name (not the trait name), e.g. for
/// `impl Processor for Worker` returns `"Worker"`.
/// Stops at `mod_item` — functions inside a module but outside any impl
/// are free functions, not methods.
fn find_parent_impl_type(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let mut current = node.parent()?;
    loop {
        match current.kind() {
            "impl_item" => {
                // `type:` field is the implementing type (e.g. `Worker` in
                // `impl Processor for Worker`); `trait:` is the trait name.
                let type_node = current.child_by_field_name("type")?;
                return match type_node.kind() {
                    "type_identifier" => type_node.utf8_text(source).ok().map(str::to_string),
                    "generic_type" => {
                        // e.g. `impl<T> Container<T>` — base name is first type_identifier child
                        (0..type_node.child_count())
                            .filter_map(|i| type_node.child(i))
                            .find(|c| c.kind() == "type_identifier")
                            .and_then(|c| c.utf8_text(source).ok().map(str::to_string))
                    }
                    _ => None,
                };
            }
            // Don't cross a module boundary — functions inside a mod are free functions.
            "mod_item" => return None,
            _ => {}
        }
        match current.parent() {
            Some(p) => current = p,
            None => return None,
        }
    }
}

// ── Use-tree walker ───────────────────────────────────────────────────────────

/// Recursively walk a `use_declaration` argument subtree and collect every
/// leaf path as a `::`-separated string.
///
/// Handles all use-clause variants present in tree-sitter-rust 0.21:
/// - `identifier`         → `foo`
/// - `scoped_identifier`  → `foo::bar`
/// - `use_as_clause`      → `foo::bar` (alias stripped; graph identity = original path)
/// - `use_list`           → recurse into each item with the same prefix
/// - `scoped_use_list`    → extend prefix, recurse into list
/// - `use_wildcard`       → `foo::*` (included; ResolvesTo resolution skips wildcards)
///
/// `self` inside a `use_list` (re-export of the current module) is skipped.
fn extract_use_paths(
    node: tree_sitter::Node<'_>,
    prefix: &str,
    source: &[u8],
    out: &mut Vec<String>,
) {
    match node.kind() {
        "identifier" => {
            let name = match node.utf8_text(source) {
                Ok(t) if t != "self" => t,
                _ => return,
            };
            out.push(if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}::{name}")
            });
        }
        "scoped_identifier" => {
            // utf8_text gives the full path including `::`, e.g. `std::fmt`
            let text = match node.utf8_text(source) {
                Ok(t) => t,
                Err(_) => return,
            };
            out.push(if prefix.is_empty() {
                text.to_owned()
            } else {
                format!("{prefix}::{text}")
            });
        }
        "use_as_clause" => {
            // `use foo::bar as Baz` — index by the original path, not the local alias.
            if let Some(path_node) = node.child_by_field_name("path") {
                extract_use_paths(path_node, prefix, source, out);
            }
        }
        "use_list" => {
            for i in 0..node.child_count() {
                let Some(child) = node.child(i) else { continue };
                match child.kind() {
                    "{" | "}" | "," => {}
                    _ => extract_use_paths(child, prefix, source, out),
                }
            }
        }
        "scoped_use_list" => {
            // `use foo::{bar, baz}` — extend prefix with the path segment,
            // then recurse into the list.
            let path_text = node
                .child_by_field_name("path")
                .and_then(|p| p.utf8_text(source).ok())
                .unwrap_or("");
            let new_prefix = match (prefix.is_empty(), path_text.is_empty()) {
                (true, _) => path_text.to_owned(),
                (_, true) => prefix.to_owned(),
                _ => format!("{prefix}::{path_text}"),
            };
            if let Some(list) = node.child_by_field_name("list") {
                extract_use_paths(list, &new_prefix, source, out);
            }
        }
        "use_wildcard" => {
            // `use foo::*` — capture the full wildcard text so the store
            // records a Depends edge; ResolvesTo resolution skips `::*` paths.
            let text = match node.utf8_text(source) {
                Ok(t) => t,
                Err(_) => return,
            };
            out.push(if prefix.is_empty() {
                text.to_owned()
            } else {
                format!("{prefix}::{text}")
            });
        }
        _ => {}
    }
}

// ── Node constructors ─────────────────────────────────────────────────────────

fn rust_file_node(corpus: &str, path: &str) -> Node {
    Node::new(rust_vname(corpus, path, "file"), "file")
}

fn rust_fn_node(corpus: &str, path: &str, name: &str) -> Node {
    Node::new(rust_vname(corpus, path, &format!("fn:{name}")), "function")
}

fn rust_method_node(corpus: &str, path: &str, impl_type: &str, method: &str) -> Node {
    Node::new(
        rust_vname(corpus, path, &format!("fn:{impl_type}.{method}")),
        "method",
    )
}

fn rust_struct_node(corpus: &str, path: &str, name: &str) -> Node {
    Node::new(
        rust_vname(corpus, path, &format!("struct:{name}")),
        "struct",
    )
}

fn rust_enum_node(corpus: &str, path: &str, name: &str) -> Node {
    Node::new(rust_vname(corpus, path, &format!("enum:{name}")), "enum")
}

fn rust_trait_node(corpus: &str, path: &str, name: &str) -> Node {
    Node::new(rust_vname(corpus, path, &format!("trait:{name}")), "trait")
}

fn rust_impl_node(corpus: &str, path: &str, name: &str) -> Node {
    Node::new(rust_vname(corpus, path, &format!("impl:{name}")), "impl")
}

fn rust_mod_node(corpus: &str, path: &str, name: &str) -> Node {
    Node::new(rust_vname(corpus, path, &format!("mod:{name}")), "module")
}

fn rust_filemod_node(corpus: &str, path: &str, name: &str) -> Node {
    Node::new(
        rust_vname(corpus, path, &format!("filemod:{name}")),
        "file-module",
    )
}

fn rust_const_node(corpus: &str, path: &str, name: &str) -> Node {
    Node::new(
        rust_vname(corpus, path, &format!("const:{name}")),
        "constant",
    )
}

fn rust_static_node(corpus: &str, path: &str, name: &str) -> Node {
    Node::new(
        rust_vname(corpus, path, &format!("static:{name}")),
        "static",
    )
}

fn rust_use_node(corpus: &str, path: &str, use_path: &str) -> Node {
    Node::new(
        rust_vname(corpus, path, &format!("use:{use_path}")),
        "import",
    )
}

fn rust_vname(corpus: &str, path: &str, signature: &str) -> VName {
    VName::new(corpus, "", path, "rust", signature)
}

// ── FFI marker collection (RFC-005) ──────────────────────────────────────────

/// Walk the AST looking for #[napi] and #[pyfunction] attribute items on functions,
/// emitting FfiMarker records for the ffi_resolver.
pub(crate) fn collect_ffi_markers(
    corpus: &str,
    vname_path: &str,
    source: &[u8],
    tree: &tree_sitter::Tree,
) -> Vec<FfiMarker> {
    let mut markers = Vec::new();
    walk_for_ffi_attrs(corpus, vname_path, source, tree.root_node(), &mut markers);
    markers
}

fn walk_for_ffi_attrs(
    corpus: &str,
    vname_path: &str,
    source: &[u8],
    node: tree_sitter::Node<'_>,
    out: &mut Vec<FfiMarker>,
) {
    if node.kind() == "function_item" {
        // Collect only the attribute_items immediately preceding this function.
        // Reset on any intervening function_item so prior functions' attrs are not inherited.
        let mut attrs: Vec<String> = Vec::new();
        if let Some(parent) = node.parent() {
            let mut running: Vec<String> = Vec::new();
            let mut cursor = parent.walk();
            for child in parent.children(&mut cursor) {
                if child.id() == node.id() {
                    attrs = running;
                    break;
                }
                if child.kind() == "attribute_item" {
                    if let Ok(text) = child.utf8_text(source) {
                        running.push(text.to_string());
                    }
                } else if child.kind() == "function_item" {
                    // Previous function consumed its attrs — start fresh.
                    running.clear();
                }
            }
        }

        let fn_name_node = node.child_by_field_name("name");
        let fn_name = fn_name_node
            .and_then(|n| n.utf8_text(source).ok())
            .unwrap_or("")
            .to_string();
        if fn_name.is_empty() {
            return;
        }

        // Determine fn node_id using the same VName scheme as rust_fn_node.
        let vname =
            travsr_core::VName::new(corpus, "", vname_path, "rust", format!("fn:{fn_name}"));
        let node_id = vname.id();

        for attr in &attrs {
            let attr_clean = attr.trim();

            // #[napi] or #[napi(...)]
            if attr_clean.contains("napi") {
                let bound_name = extract_attr_string_arg(attr_clean, "js_name");
                if let Some(m) = FfiMarker::try_new(
                    node_id,
                    FfiMarkerKind::NapiExport,
                    fn_name.clone(),
                    bound_name,
                    None,
                    None::<String>,
                    corpus,
                ) {
                    out.push(m);
                }
            }
            // #[pyfunction] or #[pyo3(...)]
            else if attr_clean.contains("pyfunction") || attr_clean.contains("pyo3") {
                let bound_name = extract_attr_string_arg(attr_clean, "name");
                if let Some(m) = FfiMarker::try_new(
                    node_id,
                    FfiMarkerKind::PyO3Export,
                    fn_name.clone(),
                    bound_name,
                    None,
                    None::<String>,
                    corpus,
                ) {
                    out.push(m);
                }
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_for_ffi_attrs(corpus, vname_path, source, child, out);
    }
}

/// Extract `key = "value"` from an attribute string like `#[napi(js_name = "greet")]`.
fn extract_attr_string_arg(attr: &str, key: &str) -> Option<String> {
    let needle = format!(r#"{key} = ""#);
    let start = attr.find(&needle)? + needle.len();
    let rest = &attr[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use travsr_core::EdgeKind;

    fn fixture_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust/simple.rs")
    }

    #[test]
    fn oversized_file_returns_err() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.rs");
        let big = vec![b'a'; (MAX_FILE_BYTES + 1) as usize];
        std::fs::write(&path, &big).unwrap();
        let result = parse("", &path, "big.rs");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("too large"));
    }

    #[test]
    fn malformed_rust_still_emits_file_node() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.rs");
        std::fs::write(&path, b"}{struct(){fn").unwrap();
        let out = parse("", &path, "bad.rs").unwrap();
        assert!(!out.nodes.is_empty());
        assert_eq!(out.nodes[0].kind, "file");
    }

    #[test]
    fn parse_file_with_vname_uses_vname_path() {
        let out = parse("", &fixture_path(), "custom/path.rs").unwrap();
        for node in &out.nodes {
            assert_eq!(node.vname.path, "custom/path.rs");
        }
    }

    #[test]
    fn language_field_is_rust() {
        let out = parse("", &fixture_path(), "simple.rs").unwrap();
        for node in &out.nodes {
            assert_eq!(node.vname.language, "rust");
        }
    }

    #[test]
    fn golden_simple_rs_emits_expected_node_kinds() {
        let out = parse("", &fixture_path(), "simple.rs").unwrap();
        let kinds: Vec<&str> = out.nodes.iter().map(|n| n.kind.as_str()).collect();

        assert!(kinds.contains(&"file"), "missing file node");
        assert!(kinds.contains(&"function"), "missing function node");
        assert!(kinds.contains(&"method"), "missing method node");
        assert!(kinds.contains(&"struct"), "missing struct node");
        assert!(kinds.contains(&"enum"), "missing enum node");
        assert!(kinds.contains(&"trait"), "missing trait node");
        assert!(kinds.contains(&"impl"), "missing impl node");
        assert!(kinds.contains(&"module"), "missing module node (inline)");
        assert!(
            kinds.contains(&"file-module"),
            "missing file-module node (mod foo;)"
        );
        assert!(kinds.contains(&"constant"), "missing constant node");
        assert!(kinds.contains(&"static"), "missing static node");
        assert!(kinds.contains(&"import"), "missing import node");
    }

    #[test]
    fn golden_simple_rs_top_level_fn_has_correct_signature() {
        let out = parse("", &fixture_path(), "simple.rs").unwrap();
        assert!(
            out.nodes
                .iter()
                .any(|n| n.vname.signature == "fn:run" && n.kind == "function"),
            "expected fn:run function node"
        );
    }

    #[test]
    fn golden_simple_rs_method_has_impl_qualified_signature() {
        let out = parse("", &fixture_path(), "simple.rs").unwrap();
        assert!(
            out.nodes
                .iter()
                .any(|n| n.vname.signature == "fn:Worker.new" && n.kind == "method"),
            "expected fn:Worker.new method node"
        );
    }

    #[test]
    fn golden_simple_rs_impl_for_trait_method_is_attributed_to_type() {
        let out = parse("", &fixture_path(), "simple.rs").unwrap();
        assert!(
            out.nodes
                .iter()
                .any(|n| n.vname.signature == "fn:Worker.process" && n.kind == "method"),
            "expected fn:Worker.process — impl Trait for Type must use the implementing type"
        );
    }

    #[test]
    fn golden_simple_rs_defines_binding_edges() {
        let out = parse("", &fixture_path(), "simple.rs").unwrap();
        let file_id = out.nodes.iter().find(|n| n.kind == "file").unwrap().id;

        // file → struct:Config
        let struct_id = out
            .nodes
            .iter()
            .find(|n| n.vname.signature == "struct:Config")
            .unwrap()
            .id;
        assert!(
            out.edges.iter().any(|e| e.src == file_id
                && e.dst == struct_id
                && e.kind == EdgeKind::DefinesBinding),
            "expected file → struct:Config DefinesBinding edge"
        );

        // impl:Worker → fn:Worker.new
        let impl_id = out
            .nodes
            .iter()
            .find(|n| n.vname.signature == "impl:Worker")
            .unwrap()
            .id;
        let method_id = out
            .nodes
            .iter()
            .find(|n| n.vname.signature == "fn:Worker.new")
            .unwrap()
            .id;
        assert!(
            out.edges.iter().any(|e| e.src == impl_id
                && e.dst == method_id
                && e.kind == EdgeKind::DefinesBinding),
            "expected impl:Worker → fn:Worker.new DefinesBinding edge"
        );
    }

    #[test]
    fn golden_simple_rs_use_has_depends_edge() {
        let out = parse("", &fixture_path(), "simple.rs").unwrap();
        let file_id = out.nodes.iter().find(|n| n.kind == "file").unwrap().id;
        let use_node = out.nodes.iter().find(|n| n.kind == "import").unwrap();
        assert!(
            out.edges
                .iter()
                .any(|e| e.src == file_id && e.dst == use_node.id && e.kind == EdgeKind::Depends),
            "expected file → use Depends edge"
        );
    }

    #[test]
    fn grouped_use_emits_individual_import_nodes() {
        // simple.rs has `use std::{collections::HashMap, io}` — both paths
        // must produce separate import nodes.
        let out = parse("", &fixture_path(), "simple.rs").unwrap();
        assert!(
            out.nodes
                .iter()
                .any(|n| n.vname.signature == "use:std::collections::HashMap"),
            "expected use:std::collections::HashMap from grouped import"
        );
        assert!(
            out.nodes.iter().any(|n| n.vname.signature == "use:std::io"),
            "expected use:std::io from grouped import"
        );
    }

    #[test]
    fn file_module_declaration_emits_file_module_node() {
        // simple.rs has `mod helpers;` (no body) — must emit kind "file-module".
        let out = parse("", &fixture_path(), "simple.rs").unwrap();
        assert!(
            out.nodes
                .iter()
                .any(|n| n.kind == "file-module" && n.vname.signature == "filemod:helpers"),
            "expected file-module node for `mod helpers;`"
        );
    }

    #[test]
    fn inline_module_still_emits_module_kind() {
        // `pub mod utils { … }` has a body — must still emit kind "module".
        let out = parse("", &fixture_path(), "simple.rs").unwrap();
        assert!(
            out.nodes
                .iter()
                .any(|n| n.kind == "module" && n.vname.signature == "mod:utils"),
            "expected module node for inline `pub mod utils {{ … }}`"
        );
    }

    #[test]
    fn impl_nodes_are_not_duplicated_for_bare_and_trait_impls() {
        // `simple.rs` has `impl Worker { ... }` AND `impl Processor for Worker`.
        // Both fire `impl.name` → `impl:Worker`. After dedup, exactly one node.
        let out = parse("", &fixture_path(), "simple.rs").unwrap();
        let count = out
            .nodes
            .iter()
            .filter(|n| n.vname.signature == "impl:Worker")
            .count();
        assert_eq!(count, 1, "impl:Worker must appear exactly once after dedup");
    }

    #[test]
    fn signature_prefix_is_rust_not_typescript() {
        let out = parse("", &fixture_path(), "simple.rs").unwrap();
        let fn_node = out
            .nodes
            .iter()
            .find(|n| n.kind == "function")
            .expect("expected at least one function node");
        assert!(
            fn_node.vname.signature.starts_with("fn:"),
            "Rust function signature must start with fn:"
        );
        assert_eq!(fn_node.vname.language, "rust");
    }
}
