use travsr_indexer::Indexer;

fn fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/python/simple.py")
}

#[test]
fn indexer_parses_python_via_parse_file() {
    let indexer = Indexer::new();
    let out = indexer.parse_file(&fixture_path()).unwrap();
    assert!(!out.nodes.is_empty(), "expected at least one node");
    assert!(
        out.nodes.iter().any(|n| n.kind == "file"),
        "expected file node"
    );
    assert!(
        out.nodes.iter().any(|n| n.kind == "class"),
        "expected class node"
    );
    assert!(
        out.nodes.iter().any(|n| n.kind == "function"),
        "expected function node"
    );
    assert!(
        out.nodes.iter().any(|n| n.kind == "method"),
        "expected method node"
    );
}

#[test]
fn indexer_python_language_field() {
    let indexer = Indexer::with_corpus("github.com/test/repo");
    let out = indexer
        .parse_file_with_vname(&fixture_path(), "src/simple.py")
        .unwrap();
    for node in &out.nodes {
        assert_eq!(node.vname.language, "python");
        assert_eq!(node.vname.corpus, "github.com/test/repo");
        assert_eq!(node.vname.path, "src/simple.py");
    }
}

#[test]
fn indexer_python_has_edges() {
    let indexer = Indexer::new();
    let out = indexer.parse_file(&fixture_path()).unwrap();
    assert!(!out.edges.is_empty(), "expected at least one edge");
}

#[test]
fn indexer_pyi_extension_is_parsed_as_python() {
    // .pyi stub files use the same Language::Python dispatch.
    let dir = tempfile::tempdir().unwrap();
    let stub = dir.path().join("types.pyi");
    std::fs::write(&stub, b"def foo(x: int) -> None: ...\nclass Bar: ...\n").unwrap();
    let indexer = Indexer::new();
    let out = indexer.parse_file(&stub).unwrap();
    assert!(
        out.nodes.iter().any(|n| n.kind == "function"),
        "expected function node from .pyi stub"
    );
}

#[test]
fn indexer_non_python_file_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let txt = dir.path().join("notes.txt");
    std::fs::write(&txt, b"hello world").unwrap();
    let indexer = Indexer::new();
    let out = indexer.parse_file(&txt).unwrap();
    // Unknown extension returns empty ParseOutput.
    assert!(out.nodes.is_empty());
    assert!(out.edges.is_empty());
}
