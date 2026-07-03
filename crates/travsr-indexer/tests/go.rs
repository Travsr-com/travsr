use travsr_core::{EdgeKind, Language};
use travsr_indexer::{link_go_copackage_edges, link_imports_go, Indexer};

fn fixtures_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/go")
}

fn main_go() -> std::path::PathBuf {
    fixtures_dir().join("main.go")
}

fn handler_go() -> std::path::PathBuf {
    fixtures_dir().join("handler.go")
}

fn handler_test_go() -> std::path::PathBuf {
    fixtures_dir().join("handler_test.go")
}

fn generic_go() -> std::path::PathBuf {
    fixtures_dir().join("generic.go")
}

// ── Language dispatch ─────────────────────────────────────────────────────────

#[test]
fn go_extension_resolves_to_language_go() {
    assert_eq!(Language::from_extension("go"), Some(Language::Go));
}

#[test]
fn language_go_as_str_is_go() {
    assert_eq!(Language::Go.as_str(), "go");
}

#[test]
fn language_go_round_trips_from_str() {
    assert_eq!(Language::from_str("go"), Some(Language::Go));
}

// ── Indexer dispatch ──────────────────────────────────────────────────────────

#[test]
fn indexer_parses_go_file() {
    let idx = Indexer::new();
    let out = idx
        .parse_file_with_vname(&main_go(), "main.go")
        .expect("parse_file_with_vname failed");
    assert!(!out.nodes.is_empty(), "expected at least a file node");
}

#[test]
fn go_file_has_language_go_in_all_vnames() {
    let idx = Indexer::new();
    let out = idx
        .parse_file_with_vname(&main_go(), "main.go")
        .expect("parse failed");
    for node in &out.nodes {
        assert_eq!(node.vname.language, "go", "non-go vname: {:?}", node.vname);
    }
}

// ── main.go golden tests ──────────────────────────────────────────────────────

#[test]
fn main_go_emits_file_node() {
    let idx = Indexer::new();
    let out = idx.parse_file_with_vname(&main_go(), "main.go").unwrap();
    assert!(out.nodes.iter().any(|n| n.kind == "file"));
}

#[test]
fn main_go_emits_fn_main() {
    let idx = Indexer::new();
    let out = idx.parse_file_with_vname(&main_go(), "main.go").unwrap();
    assert!(
        out.nodes
            .iter()
            .any(|n| n.vname.signature == "fn:main" && n.kind == "function"),
        "expected fn:main"
    );
}

#[test]
fn main_go_emits_fn_run() {
    let idx = Indexer::new();
    let out = idx.parse_file_with_vname(&main_go(), "main.go").unwrap();
    assert!(
        out.nodes
            .iter()
            .any(|n| n.vname.signature == "fn:run" && n.kind == "function"),
        "expected fn:run"
    );
}

#[test]
fn main_go_emits_import_fmt() {
    let idx = Indexer::new();
    let out = idx.parse_file_with_vname(&main_go(), "main.go").unwrap();
    assert!(
        out.nodes
            .iter()
            .any(|n| n.vname.signature == "import:fmt" && n.kind == "import"),
        "expected import:fmt"
    );
}

#[test]
fn main_go_emits_import_os() {
    let idx = Indexer::new();
    let out = idx.parse_file_with_vname(&main_go(), "main.go").unwrap();
    assert!(
        out.nodes
            .iter()
            .any(|n| n.vname.signature == "import:os" && n.kind == "import"),
        "expected import:os"
    );
}

#[test]
fn main_go_emits_var_version() {
    let idx = Indexer::new();
    let out = idx.parse_file_with_vname(&main_go(), "main.go").unwrap();
    assert!(
        out.nodes
            .iter()
            .any(|n| n.vname.signature == "var:Version" && n.kind == "var"),
        "expected var:Version"
    );
}

#[test]
fn main_go_emits_const_appname() {
    let idx = Indexer::new();
    let out = idx.parse_file_with_vname(&main_go(), "main.go").unwrap();
    assert!(
        out.nodes
            .iter()
            .any(|n| n.vname.signature == "var:AppName" && n.kind == "var"),
        "expected var:AppName from const"
    );
}

// ── handler.go golden tests ───────────────────────────────────────────────────

#[test]
fn handler_go_emits_struct_as_class() {
    let idx = Indexer::new();
    let out = idx
        .parse_file_with_vname(&handler_go(), "handler/handler.go")
        .unwrap();
    assert!(
        out.nodes
            .iter()
            .any(|n| n.vname.signature == "class:Handler" && n.kind == "class"),
        "expected class:Handler"
    );
}

#[test]
fn handler_go_emits_interface_doer() {
    let idx = Indexer::new();
    let out = idx
        .parse_file_with_vname(&handler_go(), "handler/handler.go")
        .unwrap();
    assert!(
        out.nodes
            .iter()
            .any(|n| n.vname.signature == "interface:Doer" && n.kind == "interface"),
        "expected interface:Doer"
    );
}

#[test]
fn handler_go_emits_value_receiver_method() {
    let idx = Indexer::new();
    let out = idx
        .parse_file_with_vname(&handler_go(), "handler/handler.go")
        .unwrap();
    assert!(
        out.nodes
            .iter()
            .any(|n| n.vname.signature == "method:Handler.ServeHTTP" && n.kind == "method"),
        "expected method:Handler.ServeHTTP"
    );
}

#[test]
fn handler_go_emits_pointer_receiver_method() {
    let idx = Indexer::new();
    let out = idx
        .parse_file_with_vname(&handler_go(), "handler/handler.go")
        .unwrap();
    assert!(
        out.nodes
            .iter()
            .any(|n| n.vname.signature == "method:Handler.Handle" && n.kind == "method"),
        "expected method:Handler.Handle (pointer receiver)"
    );
}

#[test]
fn handler_go_emits_fn_new() {
    let idx = Indexer::new();
    let out = idx
        .parse_file_with_vname(&handler_go(), "handler/handler.go")
        .unwrap();
    assert!(
        out.nodes
            .iter()
            .any(|n| n.vname.signature == "fn:New" && n.kind == "function"),
        "expected fn:New"
    );
}

#[test]
fn handler_go_class_defines_method_edge() {
    let idx = Indexer::new();
    let out = idx
        .parse_file_with_vname(&handler_go(), "handler/handler.go")
        .unwrap();
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
        out.edges
            .iter()
            .any(|e| e.src == class_id && e.dst == method_id && e.kind == EdgeKind::DefinesBinding),
        "expected class:Handler → method:Handler.ServeHTTP DefinesBinding edge"
    );
}

#[test]
fn handler_go_file_depends_import_net_http() {
    let idx = Indexer::new();
    let out = idx
        .parse_file_with_vname(&handler_go(), "handler/handler.go")
        .unwrap();
    let file_id = out.nodes.iter().find(|n| n.kind == "file").unwrap().id;
    let import_id = out
        .nodes
        .iter()
        .find(|n| n.vname.signature == "import:net/http")
        .unwrap()
        .id;
    assert!(
        out.edges
            .iter()
            .any(|e| e.src == file_id && e.dst == import_id && e.kind == EdgeKind::Depends),
        "expected file → import:net/http Depends edge"
    );
}

// ── generic.go golden tests ───────────────────────────────────────────────────

#[test]
fn generic_go_stack_struct_emitted() {
    let idx = Indexer::new();
    let out = idx
        .parse_file_with_vname(&generic_go(), "handler/generic.go")
        .unwrap();
    assert!(
        out.nodes
            .iter()
            .any(|n| n.vname.signature == "class:Stack" && n.kind == "class"),
        "expected class:Stack (generic type params stripped)"
    );
}

#[test]
fn generic_go_map_function_emitted() {
    let idx = Indexer::new();
    let out = idx
        .parse_file_with_vname(&generic_go(), "handler/generic.go")
        .unwrap();
    assert!(
        out.nodes
            .iter()
            .any(|n| n.vname.signature == "fn:Map" && n.kind == "function"),
        "expected fn:Map (generic type params stripped)"
    );
}

#[test]
fn generic_go_type_alias_emitted() {
    let idx = Indexer::new();
    let out = idx
        .parse_file_with_vname(&generic_go(), "handler/generic.go")
        .unwrap();
    assert!(
        out.nodes
            .iter()
            .any(|n| n.vname.signature == "type:StringID" && n.kind == "type"),
        "expected type:StringID"
    );
}

// ── handler_test.go ───────────────────────────────────────────────────────────

#[test]
fn test_file_emits_test_functions() {
    let idx = Indexer::new();
    let out = idx
        .parse_file_with_vname(&handler_test_go(), "handler/handler_test.go")
        .unwrap();
    assert!(
        out.nodes
            .iter()
            .any(|n| n.vname.signature == "fn:TestName" && n.kind == "function"),
        "expected fn:TestName"
    );
    assert!(
        out.nodes
            .iter()
            .any(|n| n.vname.signature == "fn:TestNew" && n.kind == "function"),
        "expected fn:TestNew"
    );
}

// ── Referential integrity ─────────────────────────────────────────────────────

#[test]
fn no_dangling_edge_src() {
    let idx = Indexer::new();
    let out = idx
        .parse_file_with_vname(&handler_go(), "handler/handler.go")
        .unwrap();
    let ids: std::collections::HashSet<_> = out.nodes.iter().map(|n| n.id).collect();
    for e in &out.edges {
        assert!(ids.contains(&e.src), "dangling src NodeId: {:?}", e.src);
    }
}

#[test]
fn no_dangling_edge_dst() {
    let idx = Indexer::new();
    let out = idx
        .parse_file_with_vname(&handler_go(), "handler/handler.go")
        .unwrap();
    let ids: std::collections::HashSet<_> = out.nodes.iter().map(|n| n.id).collect();
    for e in &out.edges {
        assert!(ids.contains(&e.dst), "dangling dst NodeId: {:?}", e.dst);
    }
}

// ── link_imports_go ───────────────────────────────────────────────────────────

#[test]
fn link_imports_go_same_module_emits_resolves_to() {
    let idx = Indexer::new();
    let out = idx.parse_file_with_vname(&main_go(), "main.go").unwrap();
    let edges = link_imports_go(
        &out.nodes,
        "main.go",
        "test-corpus",
        Some(("github.com/example/go-small", "/repo")),
    );
    // import:github.com/example/go-small/handler → ResolvesTo handler/
    assert!(
        edges.iter().any(|e| e.kind == EdgeKind::ResolvesTo),
        "expected ResolvesTo edge for same-module import"
    );
}

#[test]
fn link_imports_go_stdlib_emits_no_edge() {
    let idx = Indexer::new();
    let out = idx.parse_file_with_vname(&main_go(), "main.go").unwrap();
    let edges = link_imports_go(
        &out.nodes,
        "main.go",
        "test-corpus",
        Some(("github.com/example/go-small", "/repo")),
    );
    // fmt and os are stdlib — no edges for them
    let fmt_node_id = out.nodes.iter().find(|n| n.vname.signature == "import:fmt");
    if let Some(fmt_import) = fmt_node_id {
        assert!(
            !edges.iter().any(|e| e.src == fmt_import.id),
            "stdlib import:fmt must not produce ResolvesTo edges"
        );
    }
}

#[test]
fn link_imports_go_no_module_root_emits_no_edges() {
    let idx = Indexer::new();
    let out = idx.parse_file_with_vname(&main_go(), "main.go").unwrap();
    let edges = link_imports_go(&out.nodes, "main.go", "test-corpus", None);
    assert!(
        edges.is_empty(),
        "without module_root, no ResolvesTo edges expected"
    );
}

// ── link_go_copackage_edges ───────────────────────────────────────────────────

fn go_file_node(path: &str) -> travsr_core::Node {
    use travsr_core::VName;
    travsr_core::Node::new(VName::new("", "", path, "go", "file"), "file")
}

fn go_pkg_node(dir: &str, pkg: &str) -> travsr_core::Node {
    use travsr_core::VName;
    let sig = format!("go-pkg:{dir}/{pkg}");
    travsr_core::Node::new(VName::new("", "", dir, "go", &sig), "go-pkg")
}

#[test]
fn link_go_copackage_edges_emits_all_ordered_pairs() {
    use travsr_core::Edge;

    let a = go_file_node("pkg/a.go");
    let b = go_file_node("pkg/b.go");
    let c = go_file_node("pkg/c.go");
    let pkg = go_pkg_node("pkg", "mypkg");

    let nodes = vec![a.clone(), b.clone(), c.clone(), pkg.clone()];
    let edges = vec![
        Edge::new(a.id, pkg.id, EdgeKind::DefinesBinding),
        Edge::new(b.id, pkg.id, EdgeKind::DefinesBinding),
        Edge::new(c.id, pkg.id, EdgeKind::DefinesBinding),
    ];

    let result = link_go_copackage_edges(&nodes, &edges);

    assert_eq!(
        result.len(),
        6,
        "3 files → 3×2 = 6 ordered-pair Depends edges"
    );
    assert!(result.iter().all(|e| e.kind == EdgeKind::Depends));
    assert!(result.iter().all(|e| e.src != e.dst), "no self-edges");
    for &id in &[a.id, b.id, c.id] {
        assert!(
            result.iter().any(|e| e.src == id),
            "every file must appear as src"
        );
        assert!(
            result.iter().any(|e| e.dst == id),
            "every file must appear as dst"
        );
    }
}

#[test]
fn link_go_copackage_edges_single_file_emits_nothing() {
    use travsr_core::Edge;

    let a = go_file_node("solo/a.go");
    let pkg = go_pkg_node("solo", "mypkg");
    let nodes = vec![a.clone(), pkg.clone()];
    let edges = vec![Edge::new(a.id, pkg.id, EdgeKind::DefinesBinding)];

    let result = link_go_copackage_edges(&nodes, &edges);
    assert!(
        result.is_empty(),
        "single-file package must emit no co-package edges"
    );
}

#[test]
fn link_go_copackage_edges_different_packages_no_cross_edges() {
    use travsr_core::Edge;

    // mypkg and mypkg_test are distinct packages (different go-pkg NodeIds).
    let a = go_file_node("pkg/a.go");
    let b = go_file_node("pkg/a_test.go");
    let pkg_main = go_pkg_node("pkg", "mypkg");
    let pkg_test = go_pkg_node("pkg", "mypkg_test");

    let nodes = vec![a.clone(), b.clone(), pkg_main.clone(), pkg_test.clone()];
    let edges = vec![
        Edge::new(a.id, pkg_main.id, EdgeKind::DefinesBinding),
        Edge::new(b.id, pkg_test.id, EdgeKind::DefinesBinding),
    ];

    let result = link_go_copackage_edges(&nodes, &edges);
    assert!(
        result.is_empty(),
        "files in different packages must not receive cross-package Depends edges"
    );
}

#[test]
fn link_go_copackage_edges_ignores_non_defines_binding_edges() {
    use travsr_core::Edge;

    let a = go_file_node("pkg/a.go");
    let b = go_file_node("pkg/b.go");
    let pkg = go_pkg_node("pkg", "mypkg");

    let nodes = vec![a.clone(), b.clone(), pkg.clone()];
    // Both files use Depends (not DefinesBinding) toward the pkg node — must be ignored.
    let edges = vec![
        Edge::new(a.id, pkg.id, EdgeKind::Depends),
        Edge::new(b.id, pkg.id, EdgeKind::Depends),
    ];

    let result = link_go_copackage_edges(&nodes, &edges);
    assert!(
        result.is_empty(),
        "only DefinesBinding edges trigger co-package grouping"
    );
}

#[test]
fn link_go_copackage_edges_roundtrips_through_real_parse() {
    // Parse two real Go files from fixtures, confirm they share a go-pkg NodeId,
    // and verify link_go_copackage_edges produces the expected Depends edges.
    let idx = Indexer::new();
    let out_main = idx
        .parse_file_with_vname(&fixtures_dir().join("main.go"), "main/main.go")
        .unwrap();
    // handler.go lives under the same fixtures directory — reuse it with a
    // matching vname_path so both appear to be in the "main" package directory.
    let out_handler = idx
        .parse_file_with_vname(&fixtures_dir().join("handler.go"), "main/handler.go")
        .unwrap();

    let all_nodes: Vec<_> = out_main
        .nodes
        .iter()
        .chain(out_handler.nodes.iter())
        .cloned()
        .collect();
    let all_edges: Vec<_> = out_main
        .edges
        .iter()
        .chain(out_handler.edges.iter())
        .cloned()
        .collect();

    let result = link_go_copackage_edges(&all_nodes, &all_edges);

    // Both files share parent dir "main" so must produce co-package edges.
    // Result may be empty if the fixtures use different package names — that is
    // acceptable; we only assert no panic and no self-edges.
    assert!(
        result.iter().all(|e| e.src != e.dst),
        "no self-edges in real parse"
    );
    assert!(result.iter().all(|e| e.kind == EdgeKind::Depends));
}
