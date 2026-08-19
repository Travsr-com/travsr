//! Integration tests for the Python tree-sitter indexer (QA-221).
//!
//! Test groups:
//! - **Golden snapshots**: exact node/edge counts and signatures for
//!   `tests/fixtures/python/simple.py` — catches regressions in INDEX-221.
//! - **Idempotency**: parsing the same file twice produces identical output.
//! - **Import resolver** (INDEX-222): `link_imports_python` and
//!   `link_imports_python_fs` correctness for absolute, relative, and
//!   third-party imports.
//! - **Smoke tests**: .pyi stubs, non-Python files, VName field correctness.
//! - **Blocked (INDEX-222)**: ResolvesTo edge assertions stubbed with `#[ignore]`
//!   pending `link_imports_python` integration in the golden fixture test.

use std::collections::BTreeSet;
use std::path::Path;

use travsr_core::EdgeKind;
use travsr_indexer::{link_imports_python, link_imports_python_fs, Indexer};

fn fixture_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/python/simple.py")
}

fn fixture_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/python")
}

// ── Retained liveness / format tests ─────────────────────────────────────────

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
    assert!(out.nodes.is_empty());
    assert!(out.edges.is_empty());
}

// ── Golden snapshots (QA-221) ─────────────────────────────────────────────────
//
// Expected: 16 nodes, 15 edges from tests/fixtures/python/simple.py.
// If these fail, run the test, read the actual counts in the failure message,
// and update the constants here + the comment block in simple.py.

const EXPECTED_NODES: &[(&str, &str)] = &[
    ("file", "file"),
    ("class", "class:Animal"),
    ("class", "class:Dog"),
    ("function", "fn:top_level_fn"),
    ("function", "fn:helper"),
    ("function", "fn:process"),
    ("method", "method:Animal.__init__"),
    ("method", "method:Animal.speak"),
    ("method", "method:Animal.kingdom"),
    ("method", "method:Animal.create"),
    ("method", "method:Dog.speak"),
    ("method", "method:Dog.fetch"),
    ("import", "import:os"),
    ("import", "import:sys"),
    ("import", "import:pathlib"),
    ("import", "import:typing"),
];

#[test]
fn golden_simple_py_node_count() {
    let out = Indexer::new()
        .parse_file_with_vname(&fixture_path(), "src/simple.py")
        .unwrap();
    assert_eq!(
        out.nodes.len(),
        16,
        "expected 16 nodes; got {}. Actual nodes:\n{:#?}",
        out.nodes.len(),
        out.nodes
            .iter()
            .map(|n| (&n.kind, &n.vname.signature))
            .collect::<Vec<_>>()
    );
}

#[test]
fn golden_simple_py_edge_count() {
    let out = Indexer::new()
        .parse_file_with_vname(&fixture_path(), "src/simple.py")
        .unwrap();
    assert_eq!(
        out.edges.len(),
        15,
        "expected 15 edges; got {}",
        out.edges.len()
    );
}

#[test]
fn golden_simple_py_all_expected_nodes_present() {
    let out = Indexer::new()
        .parse_file_with_vname(&fixture_path(), "src/simple.py")
        .unwrap();
    let actual: BTreeSet<(&str, &str)> = out
        .nodes
        .iter()
        .map(|n| (n.kind.as_str(), n.vname.signature.as_str()))
        .collect();
    for &(kind, sig) in EXPECTED_NODES {
        assert!(
            actual.contains(&(kind, sig)),
            "missing node kind={kind:?} sig={sig:?}"
        );
    }
}

#[test]
fn golden_simple_py_no_unexpected_nodes() {
    let out = Indexer::new()
        .parse_file_with_vname(&fixture_path(), "src/simple.py")
        .unwrap();
    let expected: BTreeSet<(&str, &str)> = EXPECTED_NODES.iter().map(|&(k, s)| (k, s)).collect();
    let unexpected: Vec<_> = out
        .nodes
        .iter()
        .map(|n| (n.kind.as_str(), n.vname.signature.as_str()))
        .filter(|pair| !expected.contains(pair))
        .collect();
    assert!(
        unexpected.is_empty(),
        "unexpected nodes in output: {unexpected:#?}"
    );
}

#[test]
fn golden_simple_py_no_duplicate_node_ids() {
    let out = Indexer::new()
        .parse_file_with_vname(&fixture_path(), "src/simple.py")
        .unwrap();
    let mut ids: Vec<_> = out.nodes.iter().map(|n| n.id).collect();
    ids.sort_unstable();
    let before = ids.len();
    ids.dedup();
    assert_eq!(before, ids.len(), "duplicate node ids detected");
}

#[test]
fn golden_simple_py_no_duplicate_edges() {
    use std::collections::HashSet;
    let out = Indexer::new()
        .parse_file_with_vname(&fixture_path(), "src/simple.py")
        .unwrap();
    let mut seen = HashSet::new();
    for e in &out.edges {
        assert!(
            seen.insert((e.src, e.dst, e.kind)),
            "duplicate edge detected: src={:?} dst={:?} kind={:?}",
            e.src,
            e.dst,
            e.kind
        );
    }
}

#[test]
fn golden_simple_py_file_defines_class_animal() {
    let out = Indexer::new()
        .parse_file_with_vname(&fixture_path(), "src/simple.py")
        .unwrap();
    let file_id = out.nodes.iter().find(|n| n.kind == "file").unwrap().id;
    let class_id = out
        .nodes
        .iter()
        .find(|n| n.vname.signature == "class:Animal")
        .unwrap()
        .id;
    assert!(
        out.edges
            .iter()
            .any(|e| e.src == file_id && e.dst == class_id && e.kind == EdgeKind::DefinesBinding),
        "expected file → class:Animal DefinesBinding edge"
    );
}

#[test]
fn golden_simple_py_class_defines_method() {
    let out = Indexer::new()
        .parse_file_with_vname(&fixture_path(), "src/simple.py")
        .unwrap();
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
        out.edges
            .iter()
            .any(|e| e.src == class_id && e.dst == method_id && e.kind == EdgeKind::DefinesBinding),
        "expected class:Animal → method:Animal.speak DefinesBinding edge"
    );
}

#[test]
fn golden_simple_py_subclass_methods_attributed_to_subclass() {
    let out = Indexer::new()
        .parse_file_with_vname(&fixture_path(), "src/simple.py")
        .unwrap();
    assert!(
        out.nodes
            .iter()
            .any(|n| n.vname.signature == "method:Dog.speak" && n.kind == "method"),
        "Dog.speak must be attributed to Dog, not Animal"
    );
    let dog_id = out
        .nodes
        .iter()
        .find(|n| n.vname.signature == "class:Dog")
        .unwrap()
        .id;
    let method_id = out
        .nodes
        .iter()
        .find(|n| n.vname.signature == "method:Dog.speak")
        .unwrap()
        .id;
    assert!(
        out.edges
            .iter()
            .any(|e| e.src == dog_id && e.dst == method_id && e.kind == EdgeKind::DefinesBinding),
        "expected class:Dog → method:Dog.speak DefinesBinding edge"
    );
}

#[test]
fn golden_simple_py_file_depends_on_imports() {
    let out = Indexer::new()
        .parse_file_with_vname(&fixture_path(), "src/simple.py")
        .unwrap();
    let file_id = out.nodes.iter().find(|n| n.kind == "file").unwrap().id;
    for sig in ["import:os", "import:sys", "import:pathlib", "import:typing"] {
        let import_id = out
            .nodes
            .iter()
            .find(|n| n.vname.signature == sig)
            .unwrap_or_else(|| panic!("missing {sig} node"))
            .id;
        assert!(
            out.edges
                .iter()
                .any(|e| e.src == file_id && e.dst == import_id && e.kind == EdgeKind::Depends),
            "expected file → {sig} Depends edge"
        );
    }
}

// ── Idempotency ───────────────────────────────────────────────────────────────

#[test]
fn python_idempotent_reindex_produces_identical_graph() {
    let indexer = Indexer::new();
    let mut out1 = indexer
        .parse_file_with_vname(&fixture_path(), "src/simple.py")
        .unwrap();
    let mut out2 = indexer
        .parse_file_with_vname(&fixture_path(), "src/simple.py")
        .unwrap();

    out1.nodes.sort_unstable_by_key(|n| n.id);
    out2.nodes.sort_unstable_by_key(|n| n.id);
    let ids1: Vec<_> = out1.nodes.iter().map(|n| n.id).collect();
    let ids2: Vec<_> = out2.nodes.iter().map(|n| n.id).collect();
    assert_eq!(
        ids1, ids2,
        "node ids differ between two parses of the same file"
    );

    out1.edges.sort_unstable_by_key(|e| (e.src, e.dst));
    out2.edges.sort_unstable_by_key(|e| (e.src, e.dst));
    assert_eq!(
        out1.edges.len(),
        out2.edges.len(),
        "edge count differs between two parses"
    );
    for (e1, e2) in out1.edges.iter().zip(out2.edges.iter()) {
        assert_eq!(e1.src, e2.src);
        assert_eq!(e1.dst, e2.dst);
        assert_eq!(e1.kind, e2.kind);
    }
}

// ── Import resolver — link_imports_python (INDEX-222) ─────────────────────────

#[test]
fn link_imports_python_absolute_simple_emits_two_candidates() {
    // `import os` → os.py + os/__init__.py candidates
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.py");
    std::fs::write(&path, b"import os\n").unwrap();
    let out = Indexer::new()
        .parse_file_with_vname(&path, "main.py")
        .unwrap();
    let edges = link_imports_python(&out.nodes, "main.py", "");
    assert_eq!(edges.len(), 2, "expected 2 candidates for `import os`");
    assert!(edges.iter().all(|e| e.kind == EdgeKind::ResolvesTo));
}

#[test]
fn link_imports_python_absolute_dotted_emits_two_candidates() {
    // `import os.path` → os/path.py + os/path/__init__.py
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.py");
    std::fs::write(&path, b"import os.path\n").unwrap();
    let out = Indexer::new()
        .parse_file_with_vname(&path, "main.py")
        .unwrap();
    let edges = link_imports_python(&out.nodes, "main.py", "");
    assert_eq!(edges.len(), 2, "expected 2 candidates for `import os.path`");
}

#[test]
fn link_imports_python_relative_single_dot_resolves_to_sibling() {
    // `from .utils import helper` in `pkg/mod.py` → pkg/utils.py + pkg/utils/__init__.py
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mod.py");
    std::fs::write(&path, b"from .utils import helper\n").unwrap();
    let out = Indexer::new()
        .parse_file_with_vname(&path, "pkg/mod.py")
        .unwrap();
    let edges = link_imports_python(&out.nodes, "pkg/mod.py", "");
    assert_eq!(
        edges.len(),
        2,
        "expected 2 candidates for relative single-dot import"
    );
}

#[test]
fn link_imports_python_relative_double_dot_resolves_to_parent() {
    // `from ..utils import helper` in `pkg/sub/deep.py` → pkg/utils.py + pkg/utils/__init__.py
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("deep.py");
    std::fs::write(&path, b"from ..utils import helper\n").unwrap();
    let out = Indexer::new()
        .parse_file_with_vname(&path, "pkg/sub/deep.py")
        .unwrap();
    let edges = link_imports_python(&out.nodes, "pkg/sub/deep.py", "");
    assert_eq!(
        edges.len(),
        2,
        "expected 2 candidates for relative double-dot import"
    );
}

#[test]
fn link_imports_python_relative_bare_dot_resolves_to_init() {
    // `from . import utils` in `pkg/sub/deep.py` → pkg/sub/__init__.py only
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("deep.py");
    std::fs::write(&path, b"from . import utils\n").unwrap();
    let out = Indexer::new()
        .parse_file_with_vname(&path, "pkg/sub/deep.py")
        .unwrap();
    let edges = link_imports_python(&out.nodes, "pkg/sub/deep.py", "");
    assert_eq!(
        edges.len(),
        1,
        "bare-dot import resolves to __init__.py only"
    );
}

#[test]
fn link_imports_python_empty_for_file_with_no_imports() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty.py");
    std::fs::write(&path, b"x = 1\n").unwrap();
    let out = Indexer::new()
        .parse_file_with_vname(&path, "empty.py")
        .unwrap();
    let edges = link_imports_python(&out.nodes, "empty.py", "");
    assert!(edges.is_empty());
}

#[test]
fn link_imports_python_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mod.py");
    std::fs::write(&path, b"import os\nfrom .utils import helper\n").unwrap();
    let out = Indexer::new()
        .parse_file_with_vname(&path, "pkg/mod.py")
        .unwrap();
    let mut e1 = link_imports_python(&out.nodes, "pkg/mod.py", "");
    let mut e2 = link_imports_python(&out.nodes, "pkg/mod.py", "");
    e1.sort_unstable_by_key(|e| (e.src, e.dst));
    e2.sort_unstable_by_key(|e| (e.src, e.dst));
    assert_eq!(
        e1.len(),
        e2.len(),
        "link_imports_python must be deterministic"
    );
    for (a, b) in e1.iter().zip(e2.iter()) {
        assert_eq!(a.src, b.src);
        assert_eq!(a.dst, b.dst);
        assert_eq!(a.kind, b.kind);
    }
}

// ── Import resolver — link_imports_python_fs (FS-aware) ───────────────────────

#[test]
fn link_imports_python_fs_skips_nonexistent_modules() {
    // `import os` — os.py doesn't exist under an empty tempdir → no edges
    let repo = tempfile::tempdir().unwrap();
    let file_dir = tempfile::tempdir().unwrap();
    let path = file_dir.path().join("main.py");
    std::fs::write(&path, b"import os\n").unwrap();
    let out = Indexer::new()
        .parse_file_with_vname(&path, "main.py")
        .unwrap();
    let edges = link_imports_python_fs(&out.nodes, "main.py", "", repo.path());
    assert!(
        edges.is_empty(),
        "third-party/stdlib import must not produce edges when files absent"
    );
}

#[test]
fn link_imports_python_fs_keeps_first_party_module() {
    // Write `utils.py` into repo root → `import utils` must produce an edge
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("utils.py"), b"def f(): pass\n").unwrap();
    let file_dir = tempfile::tempdir().unwrap();
    let path = file_dir.path().join("main.py");
    std::fs::write(&path, b"import utils\n").unwrap();
    let out = Indexer::new()
        .parse_file_with_vname(&path, "main.py")
        .unwrap();
    let edges = link_imports_python_fs(&out.nodes, "main.py", "", repo.path());
    assert_eq!(edges.len(), 1, "first-party module must produce one edge");
    assert_eq!(edges[0].kind, EdgeKind::ResolvesTo);
}

#[test]
fn link_imports_python_fs_accepts_package_init() {
    // `import pkg` resolves to `pkg/__init__.py` when `pkg/` exists
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir(repo.path().join("pkg")).unwrap();
    std::fs::write(repo.path().join("pkg/__init__.py"), b"").unwrap();
    let file_dir = tempfile::tempdir().unwrap();
    let path = file_dir.path().join("main.py");
    std::fs::write(&path, b"import pkg\n").unwrap();
    let out = Indexer::new()
        .parse_file_with_vname(&path, "main.py")
        .unwrap();
    let edges = link_imports_python_fs(&out.nodes, "main.py", "", repo.path());
    assert_eq!(edges.len(), 1, "package __init__.py must produce one edge");
}

#[test]
fn link_imports_python_fs_relative_single_dot_resolves_with_repo_root() {
    // `from .utils import helper` in `pkg/mod.py` with pkg/utils.py present
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir(repo.path().join("pkg")).unwrap();
    std::fs::write(repo.path().join("pkg/utils.py"), b"def helper(): pass\n").unwrap();
    let file_dir = tempfile::tempdir().unwrap();
    let path = file_dir.path().join("mod.py");
    std::fs::write(&path, b"from .utils import helper\n").unwrap();
    let out = Indexer::new()
        .parse_file_with_vname(&path, "pkg/mod.py")
        .unwrap();
    let edges = link_imports_python_fs(&out.nodes, "pkg/mod.py", "", repo.path());
    assert_eq!(
        edges.len(),
        1,
        "relative import to existing sibling must produce one edge"
    );
}

// ── Fixture-based integration smoke ───────────────────────────────────────────

#[test]
fn python_smoke_pkg_sub_deep_resolves_double_dot() {
    // pkg/sub/deep.py has `from ..utils import helper`
    // With the fixture dir as repo root, pkg/utils.py exists → 1 edge
    let fixture = fixture_dir().join("pkg/sub/deep.py");
    let out = Indexer::new()
        .parse_file_with_vname(&fixture, "pkg/sub/deep.py")
        .unwrap();
    let edges = link_imports_python_fs(&out.nodes, "pkg/sub/deep.py", "", &fixture_dir());
    assert_eq!(
        edges.len(),
        1,
        "pkg/sub/deep.py double-dot import must resolve to pkg/utils.py"
    );
}

#[test]
fn python_smoke_standalone_skips_os_keeps_pkg_utils() {
    // standalone.py has `import os` (third-party) and `import pkg.utils` (first-party)
    // Only pkg/utils.py exists under fixture_dir → 1 edge
    let fixture = fixture_dir().join("standalone.py");
    let out = Indexer::new()
        .parse_file_with_vname(&fixture, "standalone.py")
        .unwrap();
    let edges = link_imports_python_fs(&out.nodes, "standalone.py", "", &fixture_dir());
    assert_eq!(
        edges.len(),
        1,
        "standalone.py must skip `import os` and keep `import pkg.utils`"
    );
}

// ── ResolvesTo golden fixture ─────────────────────────────────────────────────

#[test]
fn golden_simple_py_stdlib_imports_produce_no_resolves_to_edges() {
    // simple.py imports only stdlib (os, sys, pathlib, typing).
    // None of those files exist under the fixture dir, so the FS-aware
    // resolver must return zero ResolvesTo edges.
    let out = Indexer::new()
        .parse_file_with_vname(&fixture_path(), "src/simple.py")
        .unwrap();
    let edges = link_imports_python_fs(&out.nodes, "src/simple.py", "", &fixture_dir());
    assert!(
        edges.is_empty(),
        "simple.py imports only stdlib, expected 0 ResolvesTo edges, got {}: {edges:#?}",
        edges.len()
    );
}
