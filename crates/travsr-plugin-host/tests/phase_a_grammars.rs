//! Phase A golden fixtures for the grammars that register as in-process builtins.
//!
//! These prove two things at once:
//!   1. `register_builtins` compiles each grammar's tree-sitter query (a bad
//!      query panics at construction), and
//!   2. the `GenericTreeSitterPlugin` actually extracts the expected structural
//!      nodes — the ADR-017 Rule 4 "fixture-gated" condition for an in-process
//!      grammar.
//!
//! Phase A is fully in-process. The `travsr-lang-<lang>` sidecar binaries are
//! Phase B providers only (SCIP tool wrappers) — see `phase_b::catalog`.

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

#[test]
fn cpp_phase_a_extracts_structure() {
    let src = r#"
#include <vector>
namespace app {
class Widget { public: void draw(); };
struct Pod { int n; };
enum Mode { A, B };
int compute(int x) { return x * 2; }
}
"#;
    let k = kinds(src, "cpp");
    for expected in ["class", "struct", "namespace", "enum", "function", "import"] {
        assert!(
            k.contains(expected),
            "cpp: missing {expected:?} node — got {k:?}"
        );
    }
}

#[test]
fn kotlin_phase_a_extracts_structure() {
    let src = r#"
import java.time.Instant

class PaymentService {
    fun charge(amount: Int): Boolean = amount > 0
}

object Registry {
    fun lookup(name: String): String = name
}
"#;
    let k = kinds(src, "kt");
    for expected in ["class", "object", "function", "import"] {
        assert!(
            k.contains(expected),
            "kotlin: missing {expected:?} node — got {k:?}"
        );
    }
}
