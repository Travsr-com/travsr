use std::path::Path;

use travsr_core::EdgeKind;
use travsr_indexer::Indexer;

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/ts-small")
        .join(name)
}

fn indexer() -> Indexer {
    Indexer::new()
}

#[test]
fn parse_empty_file_emits_only_file_node() {
    let out = indexer().parse_file(&fixture("empty.ts")).unwrap();
    assert_eq!(out.nodes.len(), 1, "expected exactly one file node");
    assert_eq!(out.nodes[0].kind, "file");
    assert_eq!(out.edges.len(), 0);
}

#[test]
fn parse_class_emits_nodes_and_edges() {
    // a.ts: export class Greeter { hello() { return "hi"; } }
    let out = indexer().parse_file(&fixture("a.ts")).unwrap();

    let kinds: Vec<&str> = out.nodes.iter().map(|n| n.kind.as_str()).collect();
    assert!(kinds.contains(&"file"), "missing file node");
    assert!(kinds.contains(&"class"), "missing class node");
    assert!(kinds.contains(&"method"), "missing method node");

    // file → class (DefinesBinding)
    let file_node = out.nodes.iter().find(|n| n.kind == "file").unwrap();
    let class_node = out.nodes.iter().find(|n| n.kind == "class").unwrap();
    let method_node = out.nodes.iter().find(|n| n.kind == "method").unwrap();

    assert!(
        out.edges.iter().any(|e| e.src == file_node.id
            && e.dst == class_node.id
            && e.kind == EdgeKind::DefinesBinding),
        "expected DefinesBinding edge from file to class"
    );
    // class → method (DefinesBinding) — Tech Lead locked hierarchy
    assert!(
        out.edges.iter().any(|e| e.src == class_node.id
            && e.dst == method_node.id
            && e.kind == EdgeKind::DefinesBinding),
        "expected DefinesBinding edge from class to method"
    );
    // no file → method direct edge
    assert!(
        !out.edges
            .iter()
            .any(|e| e.src == file_node.id && e.dst == method_node.id),
        "unexpected direct file→method edge (should be class→method)"
    );
}

#[test]
fn parse_import_emits_depends_edge() {
    // b.ts: import { Greeter } from "./a"; function go() { ... }
    let out = indexer().parse_file(&fixture("b.ts")).unwrap();

    let kinds: Vec<&str> = out.nodes.iter().map(|n| n.kind.as_str()).collect();
    assert!(kinds.contains(&"file"), "missing file node");
    assert!(kinds.contains(&"import"), "missing import node");
    assert!(kinds.contains(&"function"), "missing function node");

    let file_node = out.nodes.iter().find(|n| n.kind == "file").unwrap();
    let import_node = out.nodes.iter().find(|n| n.kind == "import").unwrap();

    assert!(
        out.edges.iter().any(|e| e.src == file_node.id
            && e.dst == import_node.id
            && e.kind == EdgeKind::Depends),
        "expected Depends edge from file to import"
    );
    assert!(
        import_node.vname.signature.contains("./a"),
        "import node signature should contain the module path"
    );
}

#[test]
fn parse_malformed_file_still_emits_file_node() {
    let tmp = tempfile::NamedTempFile::with_suffix(".ts").unwrap();
    std::fs::write(tmp.path(), b"};; class { )").unwrap();

    let out = indexer().parse_file(tmp.path()).unwrap();
    assert!(
        !out.nodes.is_empty(),
        "expected at least the file node for malformed input"
    );
    assert_eq!(out.nodes[0].kind, "file");
}

#[test]
fn vname_signature_disambiguates_function_and_class() {
    let tmp = tempfile::NamedTempFile::with_suffix(".ts").unwrap();
    std::fs::write(tmp.path(), b"function x() {}\nclass X {}\n").unwrap();

    let out = indexer().parse_file(tmp.path()).unwrap();

    let fn_node = out
        .nodes
        .iter()
        .find(|n| n.kind == "function")
        .expect("expected function node");
    let class_node = out
        .nodes
        .iter()
        .find(|n| n.kind == "class")
        .expect("expected class node");

    assert_ne!(
        fn_node.id, class_node.id,
        "function and class must have distinct NodeIds"
    );
    assert!(
        fn_node.vname.signature.starts_with("fn:"),
        "function signature must start with fn:"
    );
    assert!(
        class_node.vname.signature.starts_with("class:"),
        "class signature must start with class:"
    );
}
