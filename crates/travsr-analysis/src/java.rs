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
    let mut nodes: Vec<Node> = vec![file_node];
    let mut ffi_markers: Vec<FfiMarker> = vec![];

    let names = query.capture_names();
    let mut iter = cursor.matches(&query, tree.root_node(), source.as_slice());

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
                    nodes.push(
                        Node::new(vn, "class")
                            .with_line(line)
                            .with_end_line(end_line),
                    );
                }
                "interface.name" => {
                    let vn =
                        VName::new(corpus, "", vname_path, "java", format!("interface:{text}"));
                    nodes.push(
                        Node::new(vn, "interface")
                            .with_line(line)
                            .with_end_line(end_line),
                    );
                }
                "enum.name" => {
                    let vn = VName::new(corpus, "", vname_path, "java", format!("enum:{text}"));
                    nodes.push(
                        Node::new(vn, "enum")
                            .with_line(line)
                            .with_end_line(end_line),
                    );
                }
                "record.name" => {
                    // Java 14+ records are class-like — `class:` keeps the G1
                    // matcher's class-candidate list closed.
                    let vn = VName::new(corpus, "", vname_path, "java", format!("class:{text}"));
                    nodes.push(
                        Node::new(vn, "class")
                            .with_line(line)
                            .with_end_line(end_line),
                    );
                }
                "annotation.name" => {
                    // `@interface` annotation types are interface-like.
                    let vn =
                        VName::new(corpus, "", vname_path, "java", format!("interface:{text}"));
                    nodes.push(
                        Node::new(vn, "interface")
                            .with_line(line)
                            .with_end_line(end_line),
                    );
                }
                "method.name" => {
                    let vn = VName::new(corpus, "", vname_path, "java", format!("fn:{text}"));
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
                    nodes.push(node);
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
                    let vn = VName::new(corpus, "", vname_path, "java", format!("fn:{text}"));
                    nodes.push(
                        Node::new(vn, "constructor")
                            .with_line(line)
                            .with_end_line(end_line),
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
                    nodes.push(Node::new(vn, "import").with_line(line));
                }
                _ => {}
            }
        }
    }

    let file_id = nodes[0].id;
    let edges: Vec<Edge> = nodes[1..]
        .iter()
        .map(|n| {
            let kind = if n.kind == "import" {
                EdgeKind::Depends
            } else {
                EdgeKind::DefinesBinding
            };
            Edge::new(file_id, n.id, kind)
        })
        .collect();

    Ok(ParseOutput {
        nodes,
        edges,
        ffi_markers,
    })
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
