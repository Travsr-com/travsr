//! Integration tests for the Rust tree-sitter indexer and link_imports_rust.
//!
//! Test groups:
//! - **Golden snapshots** (QA-201): exact node/edge counts and signatures for
//!   `tests/fixtures/rust/simple.rs`; hand-curated to catch regressions in the
//!   Phase A structural parser.
//! - **Smoke test** (QA-201): index real Travsr source files, assert liveness.
//! - **Idempotency** (QA-201): parsing the same file twice produces identical
//!   NodeIds and edges.
//! - **Import resolution** (INDEX-202): link_imports_rust correctness.

use std::path::Path;

use travsr_core::EdgeKind;
use travsr_indexer::{link_imports_rust, link_imports_rust_fs, Indexer};

fn fixture_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust/simple.rs")
}

fn parse_fixture() -> travsr_indexer::ParseOutput {
    Indexer::new()
        .parse_file_with_vname(&fixture_path(), "src/simple.rs")
        .unwrap()
}

// ── link_imports_rust — self:: resolution ─────────────────────────────────────

#[test]
fn link_imports_rust_self_emits_resolves_to_edges() {
    // Synthetic nodes with self:: paths.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mod.rs");
    std::fs::write(&path, b"use self::storage;\nuse self::query::Engine;").unwrap();
    let out = Indexer::new()
        .parse_file_with_vname(&path, "src/mod.rs")
        .unwrap();

    let edges = link_imports_rust(&out.nodes, "src/mod.rs", "");

    // use:self::storage → src/storage.rs + src/storage/mod.rs
    assert!(
        edges.iter().any(|e| e.kind == EdgeKind::ResolvesTo),
        "expected at least one ResolvesTo edge"
    );
}

// ── link_imports_rust — super:: resolution ────────────────────────────────────

#[test]
fn link_imports_rust_super_traverses_parent_dir() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let path = sub.join("mod.rs");
    std::fs::write(&path, b"use super::utils;").unwrap();
    let out = Indexer::new()
        .parse_file_with_vname(&path, "src/sub/mod.rs")
        .unwrap();

    let edges = link_imports_rust(&out.nodes, "src/sub/mod.rs", "");

    // super::utils → src/utils.rs + src/utils/mod.rs
    let use_node = out
        .nodes
        .iter()
        .find(|n| n.vname.signature == "use:super::utils")
        .expect("expected use:super::utils node");

    assert!(
        edges
            .iter()
            .any(|e| e.src == use_node.id && e.kind == EdgeKind::ResolvesTo),
        "expected ResolvesTo from use:super::utils"
    );
}

// ── link_imports_rust — wildcard skipped ─────────────────────────────────────

#[test]
fn link_imports_rust_wildcard_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lib.rs");
    std::fs::write(&path, b"use crate::prelude::*;").unwrap();
    let out = Indexer::new()
        .parse_file_with_vname(&path, "src/lib.rs")
        .unwrap();

    // `use crate::prelude::*` is external (crate::) and wildcard — both skip.
    let edges = link_imports_rust(&out.nodes, "src/lib.rs", "");
    assert!(
        edges.iter().all(|e| e.kind != EdgeKind::ResolvesTo),
        "wildcard/crate paths must produce no ResolvesTo edges"
    );
}

// ── link_imports_rust — external crate skipped ───────────────────────────────

#[test]
fn link_imports_rust_external_crate_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lib.rs");
    std::fs::write(
        &path,
        b"use std::collections::HashMap;\nuse serde::Serialize;",
    )
    .unwrap();
    let out = Indexer::new()
        .parse_file_with_vname(&path, "src/lib.rs")
        .unwrap();

    let edges = link_imports_rust(&out.nodes, "src/lib.rs", "");
    assert!(
        edges.iter().all(|e| e.kind != EdgeKind::ResolvesTo),
        "external crate imports must produce no ResolvesTo edges"
    );
}

// ── link_imports_rust — file-module declaration ───────────────────────────────

#[test]
fn link_imports_rust_filemod_emits_two_candidates() {
    // The fixture has `mod helpers;` — link_imports_rust should produce
    // ResolvesTo edges for src/helpers.rs AND src/helpers/mod.rs.
    let out = parse_fixture();
    let edges = link_imports_rust(&out.nodes, "src/simple.rs", "my_corpus");

    let filemod_node = out
        .nodes
        .iter()
        .find(|n| n.kind == "file-module")
        .expect("expected a file-module node from mod helpers;");

    let resolves: Vec<_> = edges
        .iter()
        .filter(|e| e.src == filemod_node.id && e.kind == EdgeKind::ResolvesTo)
        .collect();

    assert_eq!(
        resolves.len(),
        2,
        "mod foo; must produce exactly 2 ResolvesTo candidates (foo.rs + foo/mod.rs)"
    );
}

// ── link_imports_rust — grouped use imports ───────────────────────────────────

#[test]
fn link_imports_rust_grouped_self_imports_resolve() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lib.rs");
    std::fs::write(&path, b"use self::{store, query};").unwrap();
    let out = Indexer::new()
        .parse_file_with_vname(&path, "src/lib.rs")
        .unwrap();

    // Both self::store and self::query should be present as import nodes.
    assert!(
        out.nodes
            .iter()
            .any(|n| n.vname.signature == "use:self::store"),
        "expected use:self::store"
    );
    assert!(
        out.nodes
            .iter()
            .any(|n| n.vname.signature == "use:self::query"),
        "expected use:self::query"
    );

    let edges = link_imports_rust(&out.nodes, "src/lib.rs", "");
    // Each self:: import → 2 candidates
    assert!(
        edges
            .iter()
            .filter(|e| e.kind == EdgeKind::ResolvesTo)
            .count()
            >= 4,
        "expected ≥4 ResolvesTo edges for two self:: imports (2 candidates each)"
    );
}

// ── link_imports_rust — chained super:: ──────────────────────────────────────

#[test]
fn link_imports_rust_double_super_traverses_two_levels() {
    let dir = tempfile::tempdir().unwrap();
    let sub_sub = dir.path().join("a").join("b");
    std::fs::create_dir_all(&sub_sub).unwrap();
    let path = sub_sub.join("mod.rs");
    std::fs::write(&path, b"use super::super::utils;").unwrap();
    let out = Indexer::new()
        .parse_file_with_vname(&path, "src/a/b/mod.rs")
        .unwrap();

    let edges = link_imports_rust(&out.nodes, "src/a/b/mod.rs", "");

    // super::super::utils from src/a/b/mod.rs: the module src/utils.rs or
    // src/utils/mod.rs, or an item `utils` in the crate root (lib.rs, main.rs).
    assert!(
        edges.iter().any(|e| e.kind == EdgeKind::ResolvesTo),
        "super::super:: should resolve two directory levels up"
    );
    assert_eq!(
        edges
            .iter()
            .filter(|e| e.kind == EdgeKind::ResolvesTo)
            .count(),
        4,
        "double super:: must produce exactly 4 ResolvesTo candidates"
    );
}

// ── #1130: module directories, item paths, crate:: ───────────────────────────

/// The files a parsed snippet's imports resolve to, as repo-relative paths.
fn resolved_targets(source: &str, vname_path: &str, candidates: &[&str]) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snippet.rs");
    std::fs::write(&path, source).unwrap();
    let out = Indexer::new()
        .parse_file_with_vname(&path, vname_path)
        .unwrap();
    let edges = link_imports_rust(&out.nodes, vname_path, "c");
    candidates
        .iter()
        .filter(|c| {
            let id = travsr_core::VName::new("c", "", **c, "rust", "file").id();
            edges
                .iter()
                .any(|e| e.kind == EdgeKind::ResolvesTo && e.dst == id)
        })
        .map(|c| (*c).to_string())
        .collect()
}

#[test]
fn super_from_an_ordinary_file_resolves_beside_it() {
    // In ui/terminal.rs (module ui::terminal), `super` is `ui`, whose items
    // live in ui/. This used to resolve from src/, so no Rust `use` reached a
    // file and the file-level graph had no edges.
    let got = resolved_targets(
        "use super::files::CLI_SLOT_WAIT;
use super::ApiError;
",
        "crates/cli/src/ui/terminal.rs",
        &[
            "crates/cli/src/ui/files.rs",
            "crates/cli/src/files.rs",
            "crates/cli/src/ui/files/CLI_SLOT_WAIT.rs",
        ],
    );
    assert!(
        got.contains(&"crates/cli/src/ui/files.rs".to_string()),
        "{got:?}"
    );
    assert!(
        !got.contains(&"crates/cli/src/files.rs".to_string()),
        "not one level too high: {got:?}"
    );
}

#[test]
fn an_item_path_resolves_to_its_module_file() {
    // The last segment may be an item; the module before it is emitted too.
    let got = resolved_targets(
        "use self::engine::Core;
",
        "src/lib.rs",
        &["src/engine.rs", "src/engine/mod.rs", "src/engine/Core.rs"],
    );
    assert!(got.contains(&"src/engine.rs".to_string()), "{got:?}");
    assert!(got.contains(&"src/engine/mod.rs".to_string()), "{got:?}");
}

#[test]
fn crate_paths_resolve_from_the_crate_src_dir() {
    let got = resolved_targets(
        "use crate::daemon_client::try_query;
use crate::ui::terminal::run;
",
        "crates/cli/src/ui/terminal.rs",
        &[
            "crates/cli/src/daemon_client.rs",
            "crates/cli/src/ui/terminal.rs",
            "crates/cli/src/ui/mod.rs",
        ],
    );
    assert!(
        got.contains(&"crates/cli/src/daemon_client.rs".to_string()),
        "{got:?}"
    );
    assert!(
        got.contains(&"crates/cli/src/ui/terminal.rs".to_string()),
        "{got:?}"
    );
    // Two readings only (module, or item in the module before it): a
    // three-segment path links ui/terminal.rs, not also the enclosing ui/mod.rs.
    assert!(
        !got.contains(&"crates/cli/src/ui/mod.rs".to_string()),
        "{got:?}"
    );
}

#[test]
fn an_integration_tests_crate_paths_stay_in_tests() {
    // An integration test is its own crate, rooted in tests/: `crate::` there
    // is not the library's src/.
    let got = resolved_targets(
        "use crate::helpers::setup;\n",
        "crates/cli/tests/it.rs",
        &["crates/cli/tests/helpers.rs", "crates/cli/src/helpers.rs"],
    );
    assert_eq!(got, vec!["crates/cli/tests/helpers.rs".to_string()]);
}

#[test]
fn a_crate_path_in_no_crate_layout_is_skipped() {
    let got = resolved_targets(
        "use crate::helpers::setup;\n",
        "scripts/tool.rs",
        &["scripts/helpers.rs", "helpers.rs"],
    );
    assert!(got.is_empty(), "{got:?}");
}

/// Review of #1132: every crate root owns its directory, not only lib.rs and
/// main.rs. This repo's crates/travsr-mcp/tests/conformance.rs declares
/// `mod support;`, which lives in tests/support/mod.rs.
#[test]
fn mod_in_any_crate_root_lives_beside_it() {
    for (root, child) in [
        (
            "crates/mcp/tests/conformance.rs",
            "crates/mcp/tests/support/mod.rs",
        ),
        (
            "crates/mcp/examples/demo.rs",
            "crates/mcp/examples/support/mod.rs",
        ),
        (
            "crates/mcp/benches/speed.rs",
            "crates/mcp/benches/support/mod.rs",
        ),
        (
            "crates/mcp/src/bin/tool.rs",
            "crates/mcp/src/bin/support/mod.rs",
        ),
        ("crates/mcp/build.rs", "crates/mcp/support/mod.rs"),
    ] {
        let got = resolved_targets(
            "mod support;\n",
            root,
            &[child, "crates/mcp/tests/conformance/support/mod.rs"],
        );
        assert_eq!(got, vec![child.to_string()], "{root}");
    }
}

#[test]
fn a_binarys_crate_paths_resolve_in_src_bin() {
    let got = resolved_targets(
        "use crate::cli::Args;\n",
        "crates/mcp/src/bin/tool.rs",
        &["crates/mcp/src/bin/cli.rs", "crates/mcp/src/cli.rs"],
    );
    assert_eq!(got, vec!["crates/mcp/src/bin/cli.rs".to_string()]);
}

#[test]
fn a_tests_module_inside_src_is_an_ordinary_module() {
    // src/tests/ is a module of the library, not an integration-test folder.
    let got = resolved_targets(
        "mod extra;\nuse crate::util;\n",
        "crates/mcp/src/tests/foo.rs",
        &[
            "crates/mcp/src/tests/foo/extra.rs",
            "crates/mcp/src/tests/extra.rs",
            "crates/mcp/src/util.rs",
            "crates/mcp/src/tests/util.rs",
        ],
    );
    assert_eq!(
        got,
        vec![
            "crates/mcp/src/tests/foo/extra.rs".to_string(),
            "crates/mcp/src/util.rs".to_string()
        ]
    );
}

#[test]
fn a_mod_declared_in_an_ordinary_file_lives_in_its_directory() {
    // Rust 2018: bar.rs keeps `mod foo;` in bar/foo.rs.
    let got = resolved_targets(
        "mod foo;
",
        "src/bar.rs",
        &["src/bar/foo.rs", "src/bar/foo/mod.rs", "src/foo.rs"],
    );
    assert!(got.contains(&"src/bar/foo.rs".to_string()), "{got:?}");
    assert!(!got.contains(&"src/foo.rs".to_string()), "{got:?}");
}

/// Review of #1132: Cargo roots a `main.rs` in a subdirectory of `src/bin`,
/// `tests`, `examples` or `benches` in that subdirectory.
#[test]
fn a_main_rs_in_a_subdirectory_is_its_own_crate_root() {
    for (file, want, wrong) in [
        (
            "crates/x/src/bin/tool/main.rs",
            "crates/x/src/bin/tool/cli.rs",
            "crates/x/src/bin/cli.rs",
        ),
        (
            "crates/x/tests/foo/main.rs",
            "crates/x/tests/foo/cli.rs",
            "crates/x/tests/cli.rs",
        ),
        (
            "crates/x/examples/demo/main.rs",
            "crates/x/examples/demo/cli.rs",
            "crates/x/examples/cli.rs",
        ),
    ] {
        let got = resolved_targets("use crate::cli::Args;\n", file, &[want, wrong]);
        assert_eq!(got, vec![want.to_string()], "{file}");
    }
}

/// Review of #1132: with one segment after `super::` / `crate::`, the item
/// lives in the starting module, whose file was never linked.
#[test]
fn a_single_segment_path_names_the_enclosing_module() {
    let got = resolved_targets(
        "use super::ApiError;\n",
        "crates/cli/src/ui/terminal.rs",
        &[
            "crates/cli/src/ui.rs",
            "crates/cli/src/ui/mod.rs",
            "crates/cli/src/lib.rs",
        ],
    );
    assert_eq!(
        got,
        vec![
            "crates/cli/src/ui.rs".to_string(),
            "crates/cli/src/ui/mod.rs".to_string()
        ]
    );

    // At the crate root the module is the root file.
    let got = resolved_targets(
        "use crate::Config;\n",
        "crates/cli/src/ui/terminal.rs",
        &[
            "crates/cli/src/lib.rs",
            "crates/cli/src/main.rs",
            "crates/cli/src.rs",
            "crates/cli/src/mod.rs",
        ],
    );
    assert_eq!(
        got,
        vec![
            "crates/cli/src/lib.rs".to_string(),
            "crates/cli/src/main.rs".to_string()
        ]
    );
    let got = resolved_targets(
        "use super::Config;\n",
        "crates/cli/src/ui/mod.rs",
        &["crates/cli/src/lib.rs", "crates/cli/src.rs"],
    );
    assert_eq!(got, vec!["crates/cli/src/lib.rs".to_string()]);

    // A file never links to itself, and a flat test's own root is itself.
    let got = resolved_targets(
        "use crate::Config;\n",
        "crates/cli/src/lib.rs",
        &["crates/cli/src/lib.rs", "crates/cli/src/main.rs"],
    );
    assert_eq!(got, vec!["crates/cli/src/main.rs".to_string()]);
    let got = resolved_targets(
        "use crate::Helper;\n",
        "crates/cli/tests/it.rs",
        &["crates/cli/tests/it.rs", "crates/cli/tests.rs"],
    );
    assert!(got.is_empty(), "{got:?}");
}

/// Like [`resolved_targets`], linking against a repository laid out on disk
/// (`files` are created empty, `Cargo.toml` included).
fn resolved_targets_fs(
    files: &[&str],
    source: &str,
    vname_path: &str,
    candidates: &[&str],
) -> Vec<String> {
    let repo = tempfile::tempdir().unwrap();
    for f in files {
        let p = repo.path().join(f);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "").unwrap();
    }
    let snippet = repo.path().join("snippet.rs");
    std::fs::write(&snippet, source).unwrap();
    let out = Indexer::new()
        .parse_file_with_vname(&snippet, vname_path)
        .unwrap();
    let edges = link_imports_rust_fs(&out.nodes, vname_path, "c", repo.path());
    candidates
        .iter()
        .filter(|c| {
            let id = travsr_core::VName::new("c", "", **c, "rust", "file").id();
            edges
                .iter()
                .any(|e| e.kind == EdgeKind::ResolvesTo && e.dst == id)
        })
        .map(|c| (*c).to_string())
        .collect()
}

/// Review of #1132: a package nested under a top-level `src/` (rust-lang/rust's
/// `src/tools/clippy`) is told apart from a module inside a crate's `src/` by
/// the nearest `Cargo.toml`, which the path alone cannot do.
#[test]
fn the_nearest_manifest_decides_the_crate() {
    let layout = [
        "Cargo.toml",
        "src/main.rs",
        "src/tools/clippy/Cargo.toml",
        "src/tools/clippy/src/lib.rs",
        "src/tools/clippy/tests/ui.rs",
    ];
    let got = resolved_targets_fs(
        &layout,
        "use crate::helpers::run;\nmod support;\n",
        "src/tools/clippy/tests/ui.rs",
        &[
            "src/tools/clippy/tests/helpers.rs",
            "src/tools/clippy/tests/support.rs",
            "src/helpers.rs",
            "src/tools/clippy/tests/ui/support.rs",
        ],
    );
    assert_eq!(
        got,
        vec![
            "src/tools/clippy/tests/helpers.rs".to_string(),
            "src/tools/clippy/tests/support.rs".to_string()
        ]
    );

    // Control: under the root package's own src/, tests/ is a module.
    let got = resolved_targets_fs(
        &layout,
        "use crate::util::f;\n",
        "src/tests/foo.rs",
        &["src/util.rs", "src/tests/util.rs"],
    );
    assert_eq!(got, vec!["src/util.rs".to_string()]);
}

#[test]
fn a_multi_file_test_is_found_by_its_main_rs() {
    let layout = [
        "Cargo.toml",
        "src/lib.rs",
        "tests/foo/main.rs",
        "tests/foo/helper.rs",
        "tests/common/mod.rs",
        "tests/it.rs",
    ];
    // A module of the tests/foo crate resolves `crate::` in tests/foo.
    let got = resolved_targets_fs(
        &layout,
        "use crate::common::setup;\n",
        "tests/foo/helper.rs",
        &["tests/foo/common.rs", "tests/common.rs"],
    );
    assert_eq!(got, vec!["tests/foo/common.rs".to_string()]);
    // ...and its root file is that main.rs, which the path alone cannot know.
    let got = resolved_targets_fs(
        &layout,
        "use crate::Args;
",
        "tests/foo/helper.rs",
        &["tests/foo/main.rs", "tests/foo.rs", "src/lib.rs"],
    );
    assert_eq!(got, vec!["tests/foo/main.rs".to_string()]);
    // tests/common/ has no main.rs: it is a module shared by flat tests.
    let got = resolved_targets_fs(
        &layout,
        "use crate::fixtures::load;\n",
        "tests/common/mod.rs",
        &["tests/fixtures.rs", "tests/common/fixtures.rs"],
    );
    assert_eq!(got, vec!["tests/fixtures.rs".to_string()]);
}

// ── use_as_clause — alias stripped ────────────────────────────────────────────

#[test]
fn use_as_clause_indexes_original_path_not_alias() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lib.rs");
    std::fs::write(&path, b"use self::engine::Core as CoreEngine;").unwrap();
    let out = Indexer::new()
        .parse_file_with_vname(&path, "src/lib.rs")
        .unwrap();

    assert!(
        out.nodes
            .iter()
            .any(|n| n.vname.signature == "use:self::engine::Core"),
        "use_as_clause must index the original path"
    );
    assert!(
        !out.nodes
            .iter()
            .any(|n| n.vname.signature.contains("CoreEngine")),
        "alias must not appear in the graph"
    );
}

// ── QA-201: Golden snapshots — simple.rs fixture ─────────────────────────────
//
// These tests are the regression gate for the Phase A structural Rust parser.
// If the fixture or the parser changes, these counts MUST be updated
// deliberately — a silent drift is a bug.
//
// Fixture: crates/travsr-indexer/tests/fixtures/rust/simple.rs
// Expected structure (hand-curated, verified 2026-05-23; #757 added field×1,
// enum members added variant×2):
//   file×1  struct×2  field×1  enum×1  variant×2  trait×1  impl×2  module×1
//   file-module×1  function×2  method×3  constant×1  static×1  import×3
//   = 22 nodes, 21 edges
//   (#757: `struct Config { pub name }` now emits `field:Config.name` + its
//    containment edge; `struct Worker;` is a unit struct with no fields.
//    `enum Status { Active, Inactive }` emits `variant:Status.Active` and
//    `variant:Status.Inactive`, each contained by `enum:Status`.)

/// Exact node count for the simple.rs fixture must remain 22.
/// Update this number deliberately when the fixture or parser changes.
#[test]
fn golden_simple_fixture_node_count() {
    let out = parse_fixture();
    assert_eq!(
        out.nodes.len(),
        22,
        "expected exactly 22 nodes from simple.rs, update golden if fixture changed.\n\
         Actual nodes:\n{}",
        out.nodes
            .iter()
            .map(|n| format!("  {:12}  {}", n.kind, n.vname.signature))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Exact edge count for the simple.rs fixture must remain 21.
#[test]
fn golden_simple_fixture_edge_count() {
    let out = parse_fixture();
    assert_eq!(
        out.edges.len(),
        21,
        "expected exactly 21 edges from simple.rs, update golden if fixture changed."
    );
}

/// Every expected (kind, signature) pair must be present in the simple.rs output.
/// This guards against regressions where a node kind is silently dropped.
#[test]
fn golden_simple_fixture_all_expected_nodes_present() {
    let out = parse_fixture();

    let expected: &[(&str, &str)] = &[
        ("file", "file"),
        ("struct", "struct:Config"),
        ("field", "field:Config.name"),
        ("struct", "struct:Worker"),
        ("enum", "enum:Status"),
        ("variant", "variant:Status.Active"),
        ("variant", "variant:Status.Inactive"),
        ("trait", "trait:Processor"),
        ("impl", "impl:Config"),
        ("impl", "impl:Worker"),
        ("module", "mod:utils"),
        ("file-module", "filemod:helpers"),
        ("function", "fn:helper"),
        ("function", "fn:run"),
        ("method", "method:Worker.new"),
        ("method", "method:Worker.process"),
        ("method", "method:Config.fmt"),
        ("constant", "const:MAX_RETRIES"),
        ("static", "static:APP_NAME"),
        ("import", "use:std::fmt"),
        ("import", "use:std::collections::HashMap"),
        ("import", "use:std::io"),
    ];

    for (kind, sig) in expected {
        assert!(
            out.nodes
                .iter()
                .any(|n| n.kind == *kind && n.vname.signature == *sig),
            "missing expected node  kind={kind:12}  sig={sig}\n\
             Actual nodes:\n{}",
            out.nodes
                .iter()
                .map(|n| format!("  {:12}  {}", n.kind, n.vname.signature))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}

/// No duplicate NodeIds in the simple.rs output.
/// Duplicates would corrupt BFS traversal (double-counting, phantom cycles).
#[test]
fn golden_simple_fixture_no_duplicate_node_ids() {
    let out = parse_fixture();
    let mut ids: Vec<_> = out.nodes.iter().map(|n| n.id).collect();
    ids.sort_unstable();
    let before = ids.len();
    ids.dedup();
    assert_eq!(
        ids.len(),
        before,
        "duplicate NodeIds detected in simple.rs output, dedup in rust::parse() may be broken"
    );
}

/// No duplicate edges (src, dst, kind) in the simple.rs output.
#[test]
fn golden_simple_fixture_no_duplicate_edges() {
    let out = parse_fixture();
    let mut keys: Vec<_> = out.edges.iter().map(|e| (e.src, e.dst, e.kind)).collect();
    keys.sort_unstable_by_key(|(s, d, _)| (*s, *d));
    let before = keys.len();
    keys.dedup();
    assert_eq!(
        keys.len(),
        before,
        "duplicate edges detected in simple.rs output"
    );
}

/// No unexpected extra nodes in the simple.rs output.
///
/// Complements `golden_simple_fixture_all_expected_nodes_present` by catching
/// nodes that were added by the parser but are NOT in the expected set.
/// The count test also catches this, but this test names the offending signature
/// directly — much faster to diagnose on a regression.
#[test]
fn golden_simple_fixture_no_unexpected_nodes() {
    let out = parse_fixture();

    let expected_sigs: std::collections::BTreeSet<&str> = [
        "file",
        "struct:Config",
        "field:Config.name",
        "struct:Worker",
        "enum:Status",
        "variant:Status.Active",
        "variant:Status.Inactive",
        "trait:Processor",
        "impl:Config",
        "impl:Worker",
        "mod:utils",
        "filemod:helpers",
        "fn:helper",
        "fn:run",
        "method:Worker.new",
        "method:Worker.process",
        "method:Config.fmt",
        "const:MAX_RETRIES",
        "static:APP_NAME",
        "use:std::fmt",
        "use:std::collections::HashMap",
        "use:std::io",
    ]
    .into_iter()
    .collect();

    let actual_sigs: std::collections::BTreeSet<&str> = out
        .nodes
        .iter()
        .map(|n| n.vname.signature.as_str())
        .collect();

    let unexpected: Vec<_> = actual_sigs.difference(&expected_sigs).collect();
    assert!(
        unexpected.is_empty(),
        "unexpected nodes in simple.rs output (update golden if fixture changed): {unexpected:?}"
    );
}

// ── QA-201: Idempotency — parsing the same file twice is stable ───────────────

/// Parsing the same file twice must produce identical NodeIds and edges.
/// This guards against non-deterministic VName hashing or PRNG-seeded IDs.
#[test]
fn idempotent_reindex_produces_identical_graph() {
    let first = parse_fixture();
    let second = parse_fixture();

    let mut ids_a: Vec<_> = first.nodes.iter().map(|n| n.id).collect();
    let mut ids_b: Vec<_> = second.nodes.iter().map(|n| n.id).collect();
    ids_a.sort_unstable();
    ids_b.sort_unstable();
    assert_eq!(
        ids_a, ids_b,
        "NodeIds differ between first and second parse"
    );

    let mut edges_a: Vec<_> = first.edges.iter().map(|e| (e.src, e.dst, e.kind)).collect();
    let mut edges_b: Vec<_> = second
        .edges
        .iter()
        .map(|e| (e.src, e.dst, e.kind))
        .collect();
    edges_a.sort_unstable_by_key(|(s, d, _)| (*s, *d));
    edges_b.sort_unstable_by_key(|(s, d, _)| (*s, *d));
    assert_eq!(
        edges_a, edges_b,
        "Edge (src, dst, kind) triples differ between first and second parse"
    );
}

// ── QA-201: Smoke test — index real Travsr source files ──────────────────────

/// Parse the actual travsr-core/src/lib.rs from this workspace and assert
/// liveness: at least one struct, one function, and no panics.
/// This is the minimal "Travsr indexes itself" smoke test.
/// CI budget: < 5s (single-file parse, no I/O beyond reading one file).
#[test]
fn smoke_index_travsr_core_lib() {
    let core_lib = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent() // crates/
        .and_then(|p| p.parent()) // workspace root
        .expect("could not find workspace root")
        .join("crates/travsr-core/src/lib.rs");

    // Skip gracefully if running in an environment without the full workspace
    // (e.g. a vendor-only CI that only ships travsr-indexer).
    if !core_lib.exists() {
        eprintln!("smoke_index_travsr_core_lib: skipped, {core_lib:?} not found");
        return;
    }

    let out = Indexer::new()
        .parse_file_with_vname(&core_lib, "crates/travsr-core/src/lib.rs")
        .expect("parse must not fail on real Travsr source");

    assert!(
        out.nodes.len() > 5,
        "expected > 5 nodes from travsr-core/src/lib.rs, got {}",
        out.nodes.len()
    );
    assert!(
        out.nodes
            .iter()
            .any(|n| n.kind == "struct" || n.kind == "function"),
        "expected at least one struct or function node in travsr-core/src/lib.rs"
    );
    assert!(
        out.edges.len() > 3,
        "expected > 3 edges from travsr-core/src/lib.rs, got {}",
        out.edges.len()
    );
}
