//! Integration tests for the Rust tree-sitter indexer and link_imports_rust.

use std::path::Path;

use travsr_core::EdgeKind;
use travsr_indexer::{link_imports_rust, Indexer};

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
