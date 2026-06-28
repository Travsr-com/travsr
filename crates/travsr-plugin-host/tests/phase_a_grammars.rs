//! Phase A golden fixtures for the grammars that register as in-process builtins:
//! Scala and C.
//!
//! These prove two things at once:
//!   1. `register_builtins` compiles each grammar's tree-sitter query (a bad
//!      query panics at construction), and
//!   2. the `GenericTreeSitterPlugin` actually extracts the expected structural
//!      nodes — the ADR-017 Rule 4 "fixture-gated" condition for an in-process
//!      grammar.
//!
//! C++ and Kotlin are no longer in-process grammars (RFC-013 Direction A). Their
//! Phase A parsers live in sidecar binaries (`travsr-lang-cpp`, `travsr-lang-kotlin`).
//! The sidecar tests below run only when those binaries are on PATH so CI without
//! them still passes.

use std::collections::HashSet;
use std::io::Write as _;

use travsr_plugin_host::PluginIndexer;

const CORPUS: &str = "github.com/travsr/test";

/// Parse `source` (written to a temp file with `ext`) through the full
/// PluginIndexer dispatch path and return the node kinds produced.
fn kinds(source: &str, ext: &str) -> HashSet<String> {
    let mut f = tempfile::Builder::new()
        .suffix(&format!(".{ext}"))
        .tempfile()
        .expect("tempfile");
    f.write_all(source.as_bytes()).expect("write fixture");
    f.flush().expect("flush fixture");

    let nodes = PluginIndexer::new(CORPUS)
        .parse_file_with_vname(f.path(), &format!("fixture.{ext}"))
        .expect("parse")
        .nodes;

    nodes.into_iter().map(|n| n.kind).collect()
}

#[test]
fn scala_phase_a_extracts_structure() {
    let src = r#"
package com.acme.app
import scala.collection.mutable
class Greeter { def hi(): String = "hi" }
object Main { def run(): Unit = () }
trait Logger { def log(): Unit }
"#;
    let k = kinds(src, "scala");
    for expected in ["class", "object", "trait", "function", "import"] {
        assert!(
            k.contains(expected),
            "scala: missing {expected:?} node — got {k:?}"
        );
    }
}

#[test]
fn c_phase_a_extracts_structure() {
    let src = r#"
#include <stdio.h>
struct Point { int x; int y; };
enum Color { RED, GREEN };
typedef int Integer;
int add(int a, int b) { return a + b; }
"#;
    let k = kinds(src, "c");
    for expected in ["struct", "enum", "typedef", "function", "import"] {
        assert!(
            k.contains(expected),
            "c: missing {expected:?} node — got {k:?}"
        );
    }
}

/// Integration test for the travsr-lang-kotlin Phase A sidecar (RFC-013 Direction A).
///
/// Skipped automatically when `travsr-lang-kotlin` is not on PATH so standard CI
/// (which builds the workspace but does not install sidecar binaries) still passes.
/// To run: `cargo build -p travsr-lang-kotlin && cargo test -p travsr-plugin-host kotlin_sidecar`
#[test]
fn kotlin_phase_a_sidecar_extracts_structure() {
    let sidecar_on_path = std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )
    .any(|dir| dir.join("travsr-lang-kotlin").is_file());

    if !sidecar_on_path {
        eprintln!("SKIP: travsr-lang-kotlin not on PATH — build it first to run this test");
        return;
    }

    let fixture = std::path::Path::new("tests/fixtures/Hello.kt");
    if !fixture.exists() {
        panic!("fixture missing: tests/fixtures/Hello.kt");
    }

    let repo_root = fixture.parent().unwrap();
    let mut indexer = PluginIndexer::new(CORPUS);
    indexer.register_phase_a_sidecars(repo_root);

    let nodes = indexer
        .parse_file_with_vname(fixture, "src/PaymentService.kt")
        .expect("parse")
        .nodes;
    let k: HashSet<String> = nodes.iter().map(|n| n.kind.clone()).collect();

    for expected in ["file", "class", "function", "import"] {
        assert!(
            k.contains(expected),
            "kotlin sidecar: missing {expected:?} node — got {k:?}"
        );
    }

    assert!(
        nodes
            .iter()
            .any(|n| n.kind == "class" && n.vname.signature.contains("PaymentService")),
        "kotlin sidecar: missing PaymentService class\nnodes: {:?}",
        nodes
            .iter()
            .map(|n| (&n.kind, &n.vname.signature))
            .collect::<Vec<_>>()
    );
}

/// Integration test for the travsr-lang-cpp Phase A sidecar (RFC-013 Direction A).
///
/// Skipped automatically when `travsr-lang-cpp` is not on PATH so standard CI
/// (which builds the workspace but does not install sidecar binaries) still passes.
/// To run: `cargo build -p travsr-lang-cpp && cargo test -p travsr-plugin-host cpp_sidecar`
#[test]
fn cpp_phase_a_sidecar_extracts_structure() {
    // Skip if the sidecar binary is not installed.
    let sidecar_on_path = std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )
    .any(|dir| dir.join("travsr-lang-cpp").is_file());

    if !sidecar_on_path {
        eprintln!("SKIP: travsr-lang-cpp not on PATH — build it first to run this test");
        return;
    }

    let src = r#"
#include <vector>
namespace app {
class Widget { public: void draw(); };
struct Pod { int n; };
int compute(int x) { return x * 2; }
}
"#;
    let mut f = tempfile::Builder::new()
        .suffix(".cpp")
        .tempfile()
        .expect("tempfile");
    f.write_all(src.as_bytes()).expect("write fixture");
    f.flush().expect("flush fixture");

    let repo_root = f.path().parent().unwrap();
    let mut indexer = PluginIndexer::new(CORPUS);
    indexer.register_phase_a_sidecars(repo_root);

    let nodes = indexer
        .parse_file_with_vname(f.path(), "fixture.cpp")
        .expect("parse")
        .nodes;
    let k: HashSet<String> = nodes.into_iter().map(|n| n.kind).collect();

    for expected in ["class", "struct", "namespace", "function", "import"] {
        assert!(
            k.contains(expected),
            "cpp sidecar: missing {expected:?} node — got {k:?}"
        );
    }
}
