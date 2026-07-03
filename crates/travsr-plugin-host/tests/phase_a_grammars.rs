//! Phase A golden fixtures for sidecar language plugins (RFC-013 Direction A).
//!
//! Non-builtin languages parse via `travsr-lang-*` sidecar binaries; these
//! tests prove the full host→sidecar dispatch path (PATH resolution, lazy
//! sandboxed spawn, wire round-trip, node conversion) produces the same
//! structural nodes and RFC-014 G1 signature prefixes the in-process path
//! produced. The underlying parse code is `travsr-analysis`, shared by both.
//!
//! Each test skips automatically when its sidecar binary is not on PATH so
//! standard CI (which builds the workspace but does not install sidecar
//! binaries) still passes. To run locally:
//!   cargo build -p travsr-lang-scala && \
//!   PATH="target/debug:$PATH" cargo test -p travsr-plugin-host --test phase_a_grammars

use std::collections::HashSet;
use std::io::Write as _;

use travsr_plugin_host::registry::PHASE_A_SIDECARS;
use travsr_plugin_host::PluginIndexer;

const CORPUS: &str = "github.com/travsr/test";

/// Look up the sidecar binary for `ext` in the Phase A catalog and check PATH.
/// Returns false (test skipped) when the binary is not installed.
fn sidecar_available(ext: &str) -> bool {
    let entry = PHASE_A_SIDECARS
        .iter()
        .find(|e| e.extensions.contains(&ext))
        .unwrap_or_else(|| panic!("no Phase A sidecar catalog entry for extension {ext:?}"));
    let on_path = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .any(|dir| dir.join(entry.binary).is_file());
    if !on_path {
        eprintln!(
            "SKIP: {} not on PATH — build it first to run this test",
            entry.binary
        );
    }
    on_path
}

/// Parse `source` through the full PluginIndexer dispatch path (with Phase A
/// sidecars registered) and return the produced nodes.
/// Returns None when the language's sidecar binary is not on PATH.
fn parse_nodes(source: &str, ext: &str) -> Option<Vec<travsr_core::Node>> {
    if !sidecar_available(ext) {
        return None;
    }

    let mut f = tempfile::Builder::new()
        .suffix(&format!(".{ext}"))
        .tempfile()
        .expect("tempfile");
    f.write_all(source.as_bytes()).expect("write fixture");
    f.flush().expect("flush fixture");

    let repo_root = f.path().parent().unwrap().to_path_buf();
    let mut indexer = PluginIndexer::new(CORPUS);
    indexer.register_phase_a_sidecars(&repo_root);

    Some(
        indexer
            .parse_file_with_vname(f.path(), &format!("fixture.{ext}"))
            .expect("parse")
            .nodes,
    )
}

/// Node kinds produced for `source`. None = sidecar not installed (skip).
fn kinds(source: &str, ext: &str) -> Option<HashSet<String>> {
    parse_nodes(source, ext).map(|nodes| nodes.into_iter().map(|n| n.kind).collect())
}

/// VName signatures produced for `source` — proves the sig_prefix mapping the
/// RFC-014 G1 unifier matches against. None = sidecar not installed (skip).
fn sigs(source: &str, ext: &str) -> Option<HashSet<String>> {
    parse_nodes(source, ext).map(|nodes| nodes.into_iter().map(|n| n.vname.signature).collect())
}

fn assert_kinds(source: &str, ext: &str, expected: &[&str]) {
    let Some(k) = kinds(source, ext) else { return };
    for want in expected {
        assert!(k.contains(*want), "{ext}: missing {want:?} node — got {k:?}");
    }
}

fn assert_sigs(source: &str, ext: &str, expected: &[&str]) {
    let Some(s) = sigs(source, ext) else { return };
    for want in expected {
        assert!(
            s.contains(*want),
            "{ext}: missing signature {want:?} — got {s:?}"
        );
    }
}

#[test]
fn scala_phase_a_extracts_structure() {
    assert_kinds(
        r#"
package com.acme.app
import scala.collection.mutable
class Greeter { def hi(): String = "hi" }
object Main { def run(): Unit = () }
trait Logger { def log(): Unit }
"#,
        "scala",
        &["class", "object", "trait", "function", "import"],
    );
}

#[test]
fn c_phase_a_extracts_structure() {
    assert_kinds(
        r#"
#include <stdio.h>
struct Point { int x; int y; };
enum Color { RED, GREEN };
typedef int Integer;
int add(int a, int b) { return a + b; }
"#,
        "c",
        &["struct", "enum", "typedef", "function", "import"],
    );
}

#[test]
fn cpp_phase_a_extracts_structure() {
    assert_kinds(
        r#"
#include <vector>
namespace app {
class Widget { public: void draw(); };
struct Pod { int n; };
int compute(int x) { return x * 2; }
}
"#,
        "cpp",
        &["class", "struct", "namespace", "function", "import"],
    );
}

#[test]
fn go_phase_a_extracts_structure() {
    assert_kinds(
        r#"
package main

import "fmt"

type Point struct{ X, Y int }

func add(a, b int) int { return a + b }
"#,
        "go",
        &["file", "struct", "function", "import"],
    );
}

#[test]
fn java_phase_a_extracts_structure() {
    assert_kinds(
        r#"
import java.util.List;
public class Greeter {
    public Greeter() {}
    public String hi() { return "hi"; }
}
interface Greets { String hi(); }
"#,
        "java",
        &["file", "class", "interface", "function", "import"],
    );
}

#[test]
fn kotlin_phase_a_extracts_structure() {
    assert_kinds(
        r#"
import java.time.Instant
class PaymentService {
    fun charge(amount: Int): Boolean = amount > 0
}
object Registry { fun lookup(name: String): String = name }
"#,
        "kt",
        &["class", "object", "function", "import"],
    );
}

#[test]
fn ruby_phase_a_extracts_structure() {
    assert_kinds(
        r#"
module Billing
  class Invoice
    def total
      42
    end
  end
end
"#,
        "rb",
        &["class", "function"],
    );
}

#[test]
fn php_phase_a_extracts_structure() {
    assert_kinds(
        r#"<?php
namespace App;
use App\Services\Mailer;
class Greeter { public function hi(): string { return "hi"; } }
function helper() { return 1; }
"#,
        "php",
        &["class", "function", "import"],
    );
}

// RFC-014 #317: type-defining constructs SCIP emits as `#` symbols must have
// Phase A nodes with G1-matchable signature prefixes.

#[test]
fn kotlin_typealias_emitted() {
    assert_sigs(
        "typealias Name = String\nclass Foo\n",
        "kt",
        &["type:Name", "class:Foo"],
    );
}

#[test]
fn scala_type_definition_emitted() {
    assert_sigs(
        "object O { }\ntype Handler = String => Unit\n",
        "scala",
        &["type:Handler"],
    );
}

#[test]
fn csharp_delegate_emitted() {
    assert_sigs(
        "public delegate int Transformer(int x);\npublic class Worker { }\n",
        "cs",
        &["type:Transformer", "class:Worker"],
    );
}

#[test]
fn cpp_using_alias_emitted() {
    assert_sigs("using IntVec = int;\nclass Widget { };\n", "cpp", &["type:IntVec"]);
}

#[test]
fn dart_enum_and_typedef_emitted() {
    assert_sigs(
        "enum Color { red, green }\ntypedef StringMap = Map<String, String>;\nclass Box { }\n",
        "dart",
        &["enum:Color", "type:StringMap"],
    );
}

#[test]
fn swift_typealias_and_struct_emitted() {
    // tree-sitter-swift parses struct/enum/actor as class_declaration — the
    // existing class capture must cover them (sig prefix `class:`).
    assert_sigs(
        "typealias Velocity = Double\nstruct Point { var x: Double }\nenum Direction { case north }\n",
        "swift",
        &["type:Velocity", "class:Point", "class:Direction"],
    );
}

#[test]
fn java_record_and_annotation_emitted() {
    assert_sigs(
        "public record Point(int x, int y) { }\n@interface Marker { }\nclass Plain { }\n",
        "java",
        &["class:Point", "interface:Marker"],
    );
}

#[test]
fn objc_phase_a_extracts_structure() {
    // Covers @interface, @implementation, @protocol, instance method (-),
    // class method (+), C-style function, and #import.
    assert_kinds(
        r#"
#import <Foundation/Foundation.h>

@protocol Printable
- (void)print;
@end

@interface MyClass : NSObject <Printable>
- (void)printName;
+ (MyClass *)sharedInstance;
@end

@implementation MyClass
- (void)printName { }
+ (MyClass *)sharedInstance { return nil; }
- (void)print { }
@end

void freeFunction(void) { }
"#,
        "m",
        &["file", "class", "impl", "protocol", "function", "import"],
    );
}

#[test]
fn objc_class_impl_and_protocol_sigs_emitted() {
    assert_sigs(
        r#"
@protocol Drawable
- (void)draw;
@end

@interface Shape : NSObject
- (void)render;
+ (Shape *)unit;
@end

@implementation Shape
- (void)render { }
+ (Shape *)unit { return nil; }
- (void)draw { }
@end
"#,
        "m",
        &[
            "class:Shape",
            "impl:Shape",
            "protocol:Drawable",
            "fn:render",
            "fn:unit",
            "fn:draw",
        ],
    );
}
