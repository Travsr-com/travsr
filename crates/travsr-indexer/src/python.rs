use std::path::Path;

use anyhow::Context as _;
use travsr_core::{Node, VName};
use tree_sitter::{Parser, Query, QueryCursor};

use crate::emit;
use crate::ParseOutput;

const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024; // 10 MB

const PARSE_TIMEOUT_MICROS: u64 = 5_000_000; // 5 seconds

const _: () = {
    assert!(PARSE_TIMEOUT_MICROS >= 1_000_000);
    assert!(PARSE_TIMEOUT_MICROS <= 30_000_000);
};

// Captures: function_definition, class_definition, import_statement, import_from_statement.
// Phase A (Sprint 10): structural definitions only; semantic call/ref edges
// are deferred to Phase B (LSIF, future sprint).
//
// Methods inside a class are captured via @fn.name; the parse() match arm
// checks for an enclosing `class_definition` to qualify the signature as
// `method:ClassName.method_name` vs bare `fn:name`.
const QUERIES: &str = "
(function_definition name: (identifier) @fn.name)
(class_definition name: (identifier) @class.name)
(import_statement) @import
(import_from_statement) @from_import
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

    let language = tree_sitter_python::language();
    let file_node = py_file_node(corpus, vname_path);
    let file_id = file_node.id;

    let mut output = ParseOutput {
        nodes: vec![file_node],
        edges: vec![],
    };

    let mut parser = Parser::new();
    parser
        .set_language(&language)
        .context("loading Python grammar")?;
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

    let query = Query::new(&language, QUERIES).context("compiling Python tree-sitter query")?;

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

        match cap_name.as_str() {
            "fn.name" => {
                let text = match capture.node.utf8_text(source.as_slice()) {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                // capture.node is the identifier; its parent is function_definition.
                // Pass function_definition so find_parent_class looks at the outer context.
                let parent_class = capture
                    .node
                    .parent()
                    .and_then(|fd| find_parent_class(fd, source.as_slice()));
                let (node, src_id) = if let Some(class_name) = parent_class {
                    let class_id = py_class_node(corpus, vname_path, &class_name).id;
                    let n = py_method_node(corpus, vname_path, &class_name, text);
                    (n, class_id)
                } else {
                    let n = py_fn_node(corpus, vname_path, text);
                    (n, file_id)
                };
                output.edges.push(emit::defines_edge(src_id, node.id));
                output.nodes.push(node);
            }
            "class.name" => {
                let text = match capture.node.utf8_text(source.as_slice()) {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                let node = py_class_node(corpus, vname_path, text);
                output.edges.push(emit::defines_edge(file_id, node.id));
                output.nodes.push(node);
            }
            "import" => {
                // `import os, sys` — emit one import node per dotted name.
                for module in extract_import_names(capture.node, source.as_slice()) {
                    let node = py_import_node(corpus, vname_path, &module);
                    output.edges.push(emit::depends_edge(file_id, node.id));
                    output.nodes.push(node);
                }
            }
            "from_import" => {
                // `from pathlib import Path` → `pathlib`
                if let Some(module) = extract_from_import_module(capture.node, source.as_slice()) {
                    let node = py_import_node(corpus, vname_path, &module);
                    output.edges.push(emit::depends_edge(file_id, node.id));
                    output.nodes.push(node);
                }
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

    Ok(output)
}

/// Walk up the AST from a `function_definition` node to find the nearest
/// enclosing `class_definition`. Returns the class name, or `None` if the
/// function is module-level or nested inside another function (closure).
fn find_parent_class(fn_def: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let mut current = fn_def.parent()?;
    loop {
        match current.kind() {
            "class_definition" => {
                let name_node = current.child_by_field_name("name")?;
                return name_node.utf8_text(source).ok().map(str::to_string);
            }
            // Don't cross a function boundary — nested functions aren't methods.
            "function_definition" => return None,
            _ => {}
        }
        match current.parent() {
            Some(p) => current = p,
            None => return None,
        }
    }
}

/// Extract the module names from an `import_statement` node.
/// `import os, sys` yields `["os", "sys"]`.
/// `import os.path` yields `["os.path"]`.
fn extract_import_names(node: tree_sitter::Node<'_>, source: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    for i in 0..node.child_count() {
        let Some(child) = node.child(i) else { continue };
        match child.kind() {
            // `dotted_name` covers `os`, `os.path`, etc.
            "dotted_name" => {
                if let Ok(t) = child.utf8_text(source) {
                    names.push(t.to_owned());
                }
            }
            // `aliased_import`: `import os as operating_system` — index by original.
            "aliased_import" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    if let Ok(t) = name_node.utf8_text(source) {
                        names.push(t.to_owned());
                    }
                }
            }
            _ => {}
        }
    }
    names
}

/// Extract the module name from a `from X import Y` statement.
/// `from pathlib import Path` yields `Some("pathlib")`.
/// `from . import foo` yields `None` (relative imports skipped for now).
fn extract_from_import_module(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    // tree-sitter-python: `module_name` field holds the dotted_name; if the
    // import is relative-only (`from . import foo`) there is no module_name.
    let module_node = node.child_by_field_name("module_name")?;
    module_node.utf8_text(source).ok().map(str::to_string)
}

// ── Node constructors ─────────────────────────────────────────────────────────

fn py_file_node(corpus: &str, path: &str) -> Node {
    Node::new(py_vname(corpus, path, "file"), "file")
}

fn py_fn_node(corpus: &str, path: &str, name: &str) -> Node {
    Node::new(py_vname(corpus, path, &format!("fn:{name}")), "function")
}

fn py_method_node(corpus: &str, path: &str, class_name: &str, method: &str) -> Node {
    Node::new(
        py_vname(corpus, path, &format!("method:{class_name}.{method}")),
        "method",
    )
}

fn py_class_node(corpus: &str, path: &str, name: &str) -> Node {
    Node::new(py_vname(corpus, path, &format!("class:{name}")), "class")
}

fn py_import_node(corpus: &str, path: &str, module: &str) -> Node {
    Node::new(
        py_vname(corpus, path, &format!("import:{module}")),
        "import",
    )
}

fn py_vname(corpus: &str, path: &str, signature: &str) -> VName {
    VName::new(corpus, "", path, "python", signature)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/python/simple.py")
    }

    #[test]
    fn oversized_file_returns_err() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.py");
        let big = vec![b'a'; (MAX_FILE_BYTES + 1) as usize];
        std::fs::write(&path, &big).unwrap();
        let result = parse("", &path, "big.py");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("too large"));
    }

    #[test]
    fn malformed_python_still_emits_file_node() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.py");
        std::fs::write(&path, b"def(class(import").unwrap();
        let out = parse("", &path, "bad.py").unwrap();
        assert!(!out.nodes.is_empty());
        assert_eq!(out.nodes[0].kind, "file");
    }

    #[test]
    fn language_field_is_python() {
        let out = parse("", &fixture_path(), "simple.py").unwrap();
        for node in &out.nodes {
            assert_eq!(node.vname.language, "python");
        }
    }

    #[test]
    fn vname_path_is_respected() {
        let out = parse("", &fixture_path(), "custom/path.py").unwrap();
        for node in &out.nodes {
            assert_eq!(node.vname.path, "custom/path.py");
        }
    }

    #[test]
    fn golden_simple_py_emits_expected_node_kinds() {
        let out = parse("", &fixture_path(), "simple.py").unwrap();
        let kinds: Vec<&str> = out.nodes.iter().map(|n| n.kind.as_str()).collect();

        assert!(kinds.contains(&"file"), "missing file node");
        assert!(kinds.contains(&"function"), "missing function node");
        assert!(kinds.contains(&"method"), "missing method node");
        assert!(kinds.contains(&"class"), "missing class node");
        assert!(kinds.contains(&"import"), "missing import node");
    }

    #[test]
    fn top_level_fn_has_fn_signature() {
        let out = parse("", &fixture_path(), "simple.py").unwrap();
        assert!(
            out.nodes
                .iter()
                .any(|n| n.vname.signature == "fn:top_level_fn" && n.kind == "function"),
            "expected fn:top_level_fn function node"
        );
    }

    #[test]
    fn method_has_class_qualified_signature() {
        let out = parse("", &fixture_path(), "simple.py").unwrap();
        assert!(
            out.nodes
                .iter()
                .any(|n| n.vname.signature == "method:Animal.__init__" && n.kind == "method"),
            "expected method:Animal.__init__ method node"
        );
    }

    #[test]
    fn class_node_emitted() {
        let out = parse("", &fixture_path(), "simple.py").unwrap();
        assert!(
            out.nodes
                .iter()
                .any(|n| n.vname.signature == "class:Animal" && n.kind == "class"),
            "expected class:Animal class node"
        );
    }

    #[test]
    fn import_nodes_emitted() {
        let out = parse("", &fixture_path(), "simple.py").unwrap();
        // `import os`
        assert!(
            out.nodes
                .iter()
                .any(|n| n.vname.signature == "import:os" && n.kind == "import"),
            "expected import:os"
        );
        // `from pathlib import Path` → module "pathlib"
        assert!(
            out.nodes
                .iter()
                .any(|n| n.vname.signature == "import:pathlib" && n.kind == "import"),
            "expected import:pathlib from from-import"
        );
    }

    #[test]
    fn file_defines_class_edge() {
        use travsr_core::EdgeKind;
        let out = parse("", &fixture_path(), "simple.py").unwrap();
        let file_id = out.nodes.iter().find(|n| n.kind == "file").unwrap().id;
        let class_id = out
            .nodes
            .iter()
            .find(|n| n.vname.signature == "class:Animal")
            .unwrap()
            .id;
        assert!(
            out.edges.iter().any(|e| e.src == file_id
                && e.dst == class_id
                && e.kind == EdgeKind::DefinesBinding),
            "expected file → class:Animal DefinesBinding edge"
        );
    }

    #[test]
    fn class_defines_method_edge() {
        use travsr_core::EdgeKind;
        let out = parse("", &fixture_path(), "simple.py").unwrap();
        let class_id = out
            .nodes
            .iter()
            .find(|n| n.vname.signature == "class:Animal")
            .unwrap()
            .id;
        let method_id = out
            .nodes
            .iter()
            .find(|n| n.vname.signature == "method:Animal.speak")
            .unwrap()
            .id;
        assert!(
            out.edges.iter().any(|e| e.src == class_id
                && e.dst == method_id
                && e.kind == EdgeKind::DefinesBinding),
            "expected class:Animal → method:Animal.speak DefinesBinding edge"
        );
    }

    #[test]
    fn file_depends_import_edge() {
        use travsr_core::EdgeKind;
        let out = parse("", &fixture_path(), "simple.py").unwrap();
        let file_id = out.nodes.iter().find(|n| n.kind == "file").unwrap().id;
        let import_id = out
            .nodes
            .iter()
            .find(|n| n.vname.signature == "import:os")
            .unwrap()
            .id;
        assert!(
            out.edges
                .iter()
                .any(|e| e.src == file_id && e.dst == import_id && e.kind == EdgeKind::Depends),
            "expected file → import:os Depends edge"
        );
    }

    #[test]
    fn subclass_methods_attributed_to_subclass() {
        let out = parse("", &fixture_path(), "simple.py").unwrap();
        assert!(
            out.nodes
                .iter()
                .any(|n| n.vname.signature == "method:Dog.speak" && n.kind == "method"),
            "expected method:Dog.speak — overridden method must use subclass name"
        );
    }
}
