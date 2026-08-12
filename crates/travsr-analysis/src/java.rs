//! Phase A parser for Java source files using tree-sitter.
//!
//! In addition to structural nodes (classes, interfaces, methods), this parser
//! detects `native` method declarations and emits `JniExport` FFI markers so
//! the FFI resolver can wire them to their JNI C/C++ counterparts.

use std::path::Path;

use anyhow::Context as _;
use streaming_iterator::StreamingIterator as _;
use travsr_core::{Edge, EdgeKind, Language, Node, VName};
use tree_sitter::{Parser, Query, QueryCursor};

use crate::ffi::{FfiMarker, FfiMarkerKind};
use crate::ParseOutput;

const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024; // 10 MB

const QUERIES: &str = r#"
(class_declaration name: (identifier) @class.name)
(interface_declaration name: (identifier) @interface.name)
(enum_declaration name: (identifier) @enum.name)
(record_declaration name: (identifier) @record.name)
(annotation_type_declaration name: (identifier) @annotation.name)
(method_declaration name: (identifier) @method.name)
(constructor_declaration name: (identifier) @constructor.name)
(import_declaration) @import
"#;

/// Parse a Java source file into graph nodes, edges, and JNI FFI markers.
pub fn parse(corpus: &str, abs_path: &Path, vname_path: &str) -> anyhow::Result<ParseOutput> {
    let size = std::fs::metadata(abs_path)
        .with_context(|| format!("stat {}", abs_path.display()))?
        .len();
    anyhow::ensure!(size <= MAX_FILE_BYTES, "file too large: {size} bytes");

    let source =
        std::fs::read(abs_path).with_context(|| format!("reading {}", abs_path.display()))?;

    let mut parser = Parser::new();
    let grammar = tree_sitter::Language::new(tree_sitter_java::LANGUAGE);
    parser
        .set_language(&grammar)
        .context("loading Java grammar")?;

    let tree = parser.parse(&source, None).context("parse timeout")?;
    let query = Query::new(&grammar, QUERIES).context("building Java query")?;
    let mut cursor = QueryCursor::new();

    let file_vname = VName::new(corpus, "", vname_path, "java", "file");
    let file_node = Node::new(file_vname, "file");
    let file_id = file_node.id;
    let mut nodes: Vec<Node> = vec![file_node];
    let mut edges: Vec<Edge> = vec![];
    let mut ffi_markers: Vec<FfiMarker> = vec![];

    // N1/N3: a helper to push a node with its `DefinesBinding` edge parented to
    // `parent` (the file, or the enclosing type node for methods/constructors).
    let push =
        |nodes: &mut Vec<Node>, edges: &mut Vec<Edge>, node: Node, parent: travsr_core::NodeId| {
            let kind = if node.kind == "import" {
                EdgeKind::Depends
            } else {
                EdgeKind::DefinesBinding
            };
            edges.push(Edge::new(parent, node.id, kind));
            nodes.push(node);
        };

    let names = query.capture_names();
    let mut iter = cursor.matches(&query, tree.root_node(), source.as_slice());

    // #479: a JUnit `@Test`/`@ParameterizedTest`/`@RepeatedTest`/`@TestFactory`
    // annotation is a decisive test entry point (annotation, not name). A file
    // under `/src/test/…` is additionally a `@test.scope` so its non-annotated
    // members (helpers/fixtures) classify as `Support`.
    let mut test_signals = crate::test_role::TestSignals::default();
    if java_is_test_path(vname_path) {
        test_signals.push_scope_span(0, tree.root_node().end_position().row);
    }

    while let Some(m) = iter.next() {
        for cap in m.captures {
            let cap_name = &names[cap.index as usize];
            let text = cap.node.utf8_text(&source).unwrap_or("").to_string();
            let line = cap.node.start_position().row as u32 + 1;
            // G2: one hop from name identifier to declaration node gives the full span.
            let end_line = cap
                .node
                .parent()
                .map(|p| p.end_position().row as u32 + 1)
                .unwrap_or(line);

            match *cap_name {
                "class.name" => {
                    let vn = VName::new(corpus, "", vname_path, "java", format!("class:{text}"));
                    push(
                        &mut nodes,
                        &mut edges,
                        Node::new(vn, "class")
                            .with_line(line)
                            .with_end_line(end_line),
                        file_id,
                    );
                }
                "interface.name" => {
                    let vn =
                        VName::new(corpus, "", vname_path, "java", format!("interface:{text}"));
                    push(
                        &mut nodes,
                        &mut edges,
                        Node::new(vn, "interface")
                            .with_line(line)
                            .with_end_line(end_line),
                        file_id,
                    );
                }
                "enum.name" => {
                    let vn = VName::new(corpus, "", vname_path, "java", format!("enum:{text}"));
                    push(
                        &mut nodes,
                        &mut edges,
                        Node::new(vn, "enum")
                            .with_line(line)
                            .with_end_line(end_line),
                        file_id,
                    );
                }
                "record.name" => {
                    // Java 14+ records are class-like — `class:` keeps the G1
                    // matcher's class-candidate list closed.
                    let vn = VName::new(corpus, "", vname_path, "java", format!("class:{text}"));
                    push(
                        &mut nodes,
                        &mut edges,
                        Node::new(vn, "class")
                            .with_line(line)
                            .with_end_line(end_line),
                        file_id,
                    );
                }
                "annotation.name" => {
                    // `@interface` annotation types are interface-like.
                    let vn =
                        VName::new(corpus, "", vname_path, "java", format!("interface:{text}"));
                    push(
                        &mut nodes,
                        &mut edges,
                        Node::new(vn, "interface")
                            .with_line(line)
                            .with_end_line(end_line),
                        file_id,
                    );
                }
                "method.name" => {
                    // N1: qualify by the enclosing type (`method:Type.name`) so
                    // two same-named methods never collide on one VName.
                    let (sig, parent) = match java_enclosing_type(cap.node, &source) {
                        Some((prefix, ty)) => (
                            format!("method:{ty}.{text}"),
                            VName::new(corpus, "", vname_path, "java", format!("{prefix}:{ty}"))
                                .id(),
                        ),
                        None => (format!("fn:{text}"), file_id),
                    };
                    let vn = VName::new(corpus, "", vname_path, "java", sig);
                    // Check for native modifier on the parent method_declaration node.
                    let is_native = cap
                        .node
                        .parent()
                        .and_then(|p| p.utf8_text(&source).ok())
                        .map(|s| s.contains("native "))
                        .unwrap_or(false);
                    let node = Node::new(vn, "method")
                        .with_line(line)
                        .with_end_line(end_line);
                    let node_id = node.id;
                    push(&mut nodes, &mut edges, node, parent);
                    // #479: `@Test`-family annotation ⇒ test entry point.
                    if cap
                        .node
                        .parent()
                        .is_some_and(|md| java_method_is_test_entry(md, &source))
                    {
                        let r = cap.node.start_position().row;
                        test_signals.push_entry_span(r, r);
                    }
                    if is_native {
                        if let Some(m) = FfiMarker::try_new(
                            node_id,
                            FfiMarkerKind::JniExport,
                            text.clone(),
                            None::<String>,
                            None,
                            None::<String>,
                            corpus.to_string(),
                        ) {
                            ffi_markers.push(m);
                        }
                    }
                }
                "constructor.name" => {
                    let (sig, parent) = match java_enclosing_type(cap.node, &source) {
                        Some((prefix, ty)) => (
                            format!("method:{ty}.{text}"),
                            VName::new(corpus, "", vname_path, "java", format!("{prefix}:{ty}"))
                                .id(),
                        ),
                        None => (format!("fn:{text}"), file_id),
                    };
                    let vn = VName::new(corpus, "", vname_path, "java", sig);
                    push(
                        &mut nodes,
                        &mut edges,
                        Node::new(vn, "constructor")
                            .with_line(line)
                            .with_end_line(end_line),
                        parent,
                    );
                }
                "import" => {
                    let raw = cap.node.utf8_text(&source).unwrap_or("").trim().to_string();
                    let module = raw
                        .trim_start_matches("import ")
                        .trim_start_matches("static ")
                        .trim_end_matches(';')
                        .trim()
                        .to_string();
                    let vn = VName::new(corpus, "", vname_path, "java", format!("import:{module}"));
                    push(
                        &mut nodes,
                        &mut edges,
                        Node::new(vn, "import").with_line(line),
                        file_id,
                    );
                }
                _ => {}
            }
        }
    }

    // #479: language-agnostic post-pass sets test_role from the collected signals.
    crate::test_role::apply_test_roles(&test_signals, &mut nodes);

    Ok(ParseOutput {
        nodes,
        edges,
        ffi_markers,
    })
}

/// #479: true when a Java file lives under a `src/test/…` source root (Maven /
/// Gradle layout). Separator-agnostic so Windows store paths match.
fn java_is_test_path(vname_path: &str) -> bool {
    let p = vname_path.replace('\\', "/");
    p.contains("/src/test/") || p.starts_with("src/test/")
}

/// #479: true when a `method_declaration` carries a JUnit test annotation —
/// `@Test`, `@ParameterizedTest`, `@RepeatedTest`, or `@TestFactory` — in its
/// `modifiers`. Annotation-decisive: a production method merely named `testX`
/// never matches (asymmetric-cost rule).
fn java_method_is_test_entry(method_decl: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    // Annotations live inside the `modifiers` node (its kind, not a named field
    // in every grammar revision). Scanning only that subtree avoids picking up
    // annotations on locals inside the method body.
    method_decl
        .children(&mut method_decl.walk())
        .find(|c| c.kind() == "modifiers")
        .is_some_and(|modifiers| {
            modifiers
                .children(&mut modifiers.walk())
                .any(|c| java_annotation_is_test(c, source))
        })
}

/// True when `node` is a `marker_annotation`/`annotation` whose name is one of
/// the JUnit test annotations.
fn java_annotation_is_test(node: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    if !matches!(node.kind(), "marker_annotation" | "annotation") {
        return false;
    }
    node.child_by_field_name("name")
        .and_then(|n| n.utf8_text(source).ok())
        .map(|n| n.rsplit('.').next().unwrap_or(n))
        .is_some_and(|n| {
            matches!(
                n,
                "Test" | "ParameterizedTest" | "RepeatedTest" | "TestFactory"
            )
        })
}

/// Walk up from a method/constructor name identifier to the nearest enclosing
/// type declaration, returning `(sig_prefix, type_name)` so the caller can
/// build both the qualified `method:Type.name` signature and the container's
/// own VName (`{prefix}:{type_name}`) for the containment edge. The prefix
/// mirrors the top-level capture arms: records map to `class`, annotation
/// types to `interface`.
fn java_enclosing_type(
    node: tree_sitter::Node<'_>,
    source: &[u8],
) -> Option<(&'static str, String)> {
    let mut cur = node.parent();
    while let Some(n) = cur {
        let prefix = match n.kind() {
            "class_declaration" | "record_declaration" => Some("class"),
            "interface_declaration" | "annotation_type_declaration" => Some("interface"),
            "enum_declaration" => Some("enum"),
            _ => None,
        };
        if let Some(prefix) = prefix {
            if let Some(name) = n
                .child_by_field_name("name")
                .and_then(|c| c.utf8_text(source).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
            {
                return Some((prefix, name));
            }
        }
        cur = n.parent();
    }
    None
}

/// Extensions handled by this parser.
pub const EXTENSIONS: &[&str] = &["java"];

/// `Language` tag.
pub const LANGUAGE: Language = Language::Java;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Empty.java");
        std::fs::write(&path, "").unwrap();
        let out = parse("corp", &path, "Empty.java").unwrap();
        assert_eq!(out.nodes.len(), 1, "file node only");
        assert!(out.edges.is_empty());
    }

    #[test]
    fn parse_class_and_methods() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Foo.java");
        std::fs::write(
            &path,
            "import java.util.List;\n\
             public class Foo {\n\
               public void bar() {}\n\
               public Foo() {}\n\
             }\n\
             interface IFoo {}\n",
        )
        .unwrap();
        let out = parse("corp", &path, "Foo.java").unwrap();
        let kinds: Vec<&str> = out.nodes.iter().map(|n| n.kind.as_str()).collect();
        assert!(kinds.contains(&"class"));
        assert!(kinds.contains(&"interface"));
        assert!(kinds.contains(&"method"));
        assert!(kinds.contains(&"constructor"));
        assert!(kinds.contains(&"import"));
        assert!(out.ffi_markers.is_empty(), "no JNI markers in pure Java");
    }

    #[test]
    fn native_method_emits_jni_marker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Native.java");
        std::fs::write(
            &path,
            "public class Native {\n  public native void nativeOp();\n}\n",
        )
        .unwrap();
        let out = parse("corp", &path, "Native.java").unwrap();
        assert_eq!(out.ffi_markers.len(), 1, "one JNI marker");
        assert!(
            matches!(out.ffi_markers[0].kind, FfiMarkerKind::JniExport),
            "correct marker kind"
        );
        assert_eq!(out.ffi_markers[0].local_name, "nativeOp");
    }
}
