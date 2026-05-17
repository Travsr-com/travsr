use std::path::Path;

use travsr_core::EdgeKind;
use travsr_indexer::{hash_file, link_imports, Indexer};

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

#[test]
fn link_imports_emits_resolves_to_for_relative_import() {
    // b.ts imports from "./a" — should produce edges to a.ts and a.tsx candidates
    let vname_path = "fixtures/ts-small/b.ts";
    let out = indexer()
        .parse_file_with_vname(&fixture("b.ts"), vname_path)
        .unwrap();

    let edges = link_imports(&out.nodes, vname_path);

    // Two candidates per relative import: .ts and .tsx
    assert_eq!(edges.len(), 2, "expected one edge per extension candidate");
    assert!(
        edges.iter().all(|e| e.kind == EdgeKind::ResolvesTo),
        "all emitted edges must be ResolvesTo"
    );

    // The import node must be the source of every edge
    let import_node = out.nodes.iter().find(|n| n.kind == "import").unwrap();
    assert!(
        edges.iter().all(|e| e.src == import_node.id),
        "resolves-to edge src must be the import node"
    );

    // One of the candidates must resolve to a.ts
    let expected_target = Indexer::new()
        .parse_file_with_vname(&fixture("a.ts"), "fixtures/ts-small/a.ts")
        .unwrap()
        .nodes
        .into_iter()
        .find(|n| n.kind == "file")
        .unwrap();
    assert!(
        edges.iter().any(|e| e.dst == expected_target.id),
        "one edge must resolve to fixtures/ts-small/a.ts file node"
    );
}

#[test]
fn link_imports_skips_package_imports() {
    // extension.ts imports "vscode" — not relative, must be skipped
    let abs =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/travsr-vscode/src/extension.ts");
    let vname_path = "packages/travsr-vscode/src/extension.ts";
    let out = indexer().parse_file_with_vname(&abs, vname_path).unwrap();

    let edges = link_imports(&out.nodes, vname_path);

    // "vscode" is a package import — skipped.  "./status" is relative — 2 candidates.
    let package_edges: Vec<_> = edges
        .iter()
        .filter(|e| {
            // A resolves-to edge targeting a node whose path contains "vscode" package
            // would be wrong; we detect by checking that all targets are under the src tree
            let dst_sig = out
                .nodes
                .iter()
                .find(|n| n.id == e.dst)
                .map(|n| n.vname.path.as_str())
                .unwrap_or("");
            dst_sig.contains("node_modules") || dst_sig == "vscode"
        })
        .collect();
    assert!(
        package_edges.is_empty(),
        "package imports must not produce resolves-to edges"
    );

    // "./status" → 2 candidates (status.ts, status.tsx)
    assert_eq!(
        edges.len(),
        2,
        "only the relative './status' import should produce edges"
    );
}

#[test]
fn link_imports_empty_for_file_with_no_imports() {
    let out = indexer().parse_file(&fixture("empty.ts")).unwrap();
    let edges = link_imports(&out.nodes, "fixtures/ts-small/empty.ts");
    assert!(edges.is_empty());
}

#[test]
fn hash_file_is_deterministic() {
    let h1 = hash_file(&fixture("a.ts")).unwrap();
    let h2 = hash_file(&fixture("a.ts")).unwrap();
    assert_eq!(h1, h2, "same file must produce the same hash");
}

#[test]
fn hash_file_differs_on_change() {
    let h1 = hash_file(&fixture("a.ts")).unwrap();
    let tmp = tempfile::NamedTempFile::with_suffix(".ts").unwrap();
    std::fs::write(tmp.path(), b"export class Different {}").unwrap();
    let h2 = hash_file(tmp.path()).unwrap();
    assert_ne!(h1, h2, "different content must produce a different hash");
}

#[test]
fn parse_file_with_vname_uses_vname_path() {
    let out = indexer()
        .parse_file_with_vname(&fixture("a.ts"), "custom/path.ts")
        .unwrap();
    for node in &out.nodes {
        assert_eq!(
            node.vname.path, "custom/path.ts",
            "all nodes must carry the supplied vname_path"
        );
    }
}
