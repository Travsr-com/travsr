use std::path::Path;

use anyhow::Context as _;
use travsr_core::{Node, VName};
use tree_sitter::{Parser, Query, QueryCursor};

use crate::emit;
use crate::ParseOutput;

/// Reject files larger than this before reading them into memory.
const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024; // 10 MB

/// Kill the Tree-sitter parse if it hasn't finished within this window.
const PARSE_TIMEOUT_MICROS: u64 = 5_000_000; // 5 seconds

const _: () = {
    assert!(PARSE_TIMEOUT_MICROS >= 1_000_000);
    assert!(PARSE_TIMEOUT_MICROS <= 30_000_000);
};

// Captures: fn, struct, enum, trait, impl, mod, const, static, use.
// Phase A (Sprint 8): structural definitions only; semantic call/ref edges
// are deferred to Phase B (Sprint 9, rust-analyzer LSIF).
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
(use_declaration argument: (scoped_identifier) @use.path)
(use_declaration argument: (identifier) @use.path)
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

        match cap_name.as_str() {
            "fn.name" => {
                // Functions inside impl blocks become methods; the parent impl
                // type is the namespace so signatures are `fn:TypeName.method`.
                let parent_impl = find_parent_impl_type(capture.node, source.as_slice());
                let (node, src_id) = if let Some(impl_type) = parent_impl {
                    let impl_id = rust_impl_node(corpus, vname_path, &impl_type).id;
                    let n = rust_method_node(corpus, vname_path, &impl_type, text);
                    (n, impl_id)
                } else {
                    let n = rust_fn_node(corpus, vname_path, text);
                    (n, file_id)
                };
                output.edges.push(emit::defines_edge(src_id, node.id));
                output.nodes.push(node);
            }
            "struct.name" => {
                let node = rust_struct_node(corpus, vname_path, text);
                output.edges.push(emit::defines_edge(file_id, node.id));
                output.nodes.push(node);
            }
            "enum.name" => {
                let node = rust_enum_node(corpus, vname_path, text);
                output.edges.push(emit::defines_edge(file_id, node.id));
                output.nodes.push(node);
            }
            "trait.name" => {
                let node = rust_trait_node(corpus, vname_path, text);
                output.edges.push(emit::defines_edge(file_id, node.id));
                output.nodes.push(node);
            }
            "impl.name" => {
                let node = rust_impl_node(corpus, vname_path, text);
                output.edges.push(emit::defines_edge(file_id, node.id));
                output.nodes.push(node);
            }
            "mod.name" => {
                let node = rust_mod_node(corpus, vname_path, text);
                output.edges.push(emit::defines_edge(file_id, node.id));
                output.nodes.push(node);
            }
            "const.name" => {
                let node = rust_const_node(corpus, vname_path, text);
                output.edges.push(emit::defines_edge(file_id, node.id));
                output.nodes.push(node);
            }
            "static.name" => {
                let node = rust_static_node(corpus, vname_path, text);
                output.edges.push(emit::defines_edge(file_id, node.id));
                output.nodes.push(node);
            }
            "use.path" => {
                let node = rust_use_node(corpus, vname_path, text);
                output.edges.push(emit::depends_edge(file_id, node.id));
                output.nodes.push(node);
            }
            _ => {}
        }
    }

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
        assert!(kinds.contains(&"module"), "missing module node");
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
