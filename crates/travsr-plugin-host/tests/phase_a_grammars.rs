//! Phase A golden fixtures for the grammars added to support the travsr-lang
//! Phase B packages: Scala, C, and C++.
//!
//! These prove two things at once:
//!   1. `register_builtins` compiles each grammar's tree-sitter query (a bad
//!      query panics at construction), and
//!   2. the `GenericTreeSitterPlugin` actually extracts the expected structural
//!      nodes — the ADR-017 Rule 4 "fixture-gated" condition for an in-process
//!      grammar.

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
int compute(int x) { return x * 2; }
}
"#;
    let k = kinds(src, "cpp");
    for expected in ["class", "struct", "namespace", "function", "import"] {
        assert!(
            k.contains(expected),
            "cpp: missing {expected:?} node — got {k:?}"
        );
    }
}

/// Like `kinds`, but returns VName signatures — proves the sig_prefix mapping
/// the RFC-014 G1 unifier matches against, not just the node kind.
fn sigs(source: &str, ext: &str) -> HashSet<String> {
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

    nodes.into_iter().map(|n| n.vname.signature).collect()
}

// RFC-014 #317: type-defining constructs SCIP emits as `#` symbols must have
// Phase A nodes with G1-matchable signature prefixes.

#[test]
fn kotlin_typealias_emitted() {
    let s = sigs("typealias Name = String\nclass Foo\n", "kt");
    assert!(
        s.contains("type:Name"),
        "kotlin: missing type:Name — got {s:?}"
    );
    assert!(
        s.contains("class:Foo"),
        "kotlin: missing class:Foo — got {s:?}"
    );
}

#[test]
fn scala_type_definition_emitted() {
    let s = sigs("object O { }\ntype Handler = String => Unit\n", "scala");
    assert!(
        s.contains("type:Handler"),
        "scala: missing type:Handler — got {s:?}"
    );
}

#[test]
fn csharp_delegate_emitted() {
    let s = sigs(
        "public delegate int Transformer(int x);\npublic class Worker { }\n",
        "cs",
    );
    assert!(
        s.contains("type:Transformer"),
        "csharp: missing type:Transformer — got {s:?}"
    );
    assert!(
        s.contains("class:Worker"),
        "csharp: missing class:Worker — got {s:?}"
    );
}

#[test]
fn cpp_using_alias_emitted() {
    let s = sigs("using IntVec = int;\nclass Widget { };\n", "cpp");
    assert!(
        s.contains("type:IntVec"),
        "cpp: missing type:IntVec — got {s:?}"
    );
}

#[test]
fn dart_enum_and_typedef_emitted() {
    let s = sigs(
        "enum Color { red, green }\ntypedef StringMap = Map<String, String>;\nclass Box { }\n",
        "dart",
    );
    assert!(
        s.contains("enum:Color"),
        "dart: missing enum:Color — got {s:?}"
    );
    assert!(
        s.contains("type:StringMap"),
        "dart: missing type:StringMap — got {s:?}"
    );
}

#[test]
fn swift_typealias_and_struct_emitted() {
    // tree-sitter-swift parses struct/enum/actor as class_declaration — the
    // existing class capture must cover them (sig prefix `class:`).
    let s = sigs(
        "typealias Velocity = Double\nstruct Point { var x: Double }\nenum Direction { case north }\n",
        "swift",
    );
    assert!(
        s.contains("type:Velocity"),
        "swift: missing type:Velocity — got {s:?}"
    );
    assert!(
        s.contains("class:Point"),
        "swift: struct must emit class:Point — got {s:?}"
    );
    assert!(
        s.contains("class:Direction"),
        "swift: enum must emit class:Direction — got {s:?}"
    );
}

#[test]
fn java_record_and_annotation_emitted() {
    let s = sigs(
        "public record Point(int x, int y) { }\n@interface Marker { }\nclass Plain { }\n",
        "java",
    );
    assert!(
        s.contains("class:Point"),
        "java: record must emit class:Point — got {s:?}"
    );
    assert!(
        s.contains("interface:Marker"),
        "java: annotation type must emit interface:Marker — got {s:?}"
    );
}

#[test]
fn objc_phase_a_extracts_structure() {
    // Covers @interface, @implementation, @protocol, instance method (-),
    // class method (+), C-style function, and #import.
    let src = r#"
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
"#;
    let k = kinds(src, "m");
    for expected in ["file", "class", "impl", "protocol", "function", "import"] {
        assert!(
            k.contains(expected),
            "objc: missing node kind '{expected}' — got {k:?}"
        );
    }
}

#[test]
fn objc_class_impl_and_protocol_sigs_emitted() {
    let src = r#"
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
"#;
    let s = sigs(src, "m");
    for expected in [
        "class:Shape",
        "impl:Shape",
        "protocol:Drawable",
        "fn:render",
        "fn:unit",
        "fn:draw",
    ] {
        assert!(
            s.contains(expected),
            "objc: missing signature '{expected}' — got {s:?}"
        );
    }
}
