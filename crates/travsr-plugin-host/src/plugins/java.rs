use std::path::Path;

use anyhow::Context as _;
use travsr_core::{Edge, EdgeKind, Language, Node, VName};
use travsr_plugin_protocol::{
    FfiMarker as WireFfi, FfiMarkerKind as WireKind, InvokeRequest, InvokeResponse, ParseRequest,
    ParseResponse, Plugin,
};
use tree_sitter::{Parser, Query, QueryCursor};

const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

const JAVA_QUERIES: &str = r#"
(class_declaration name: (identifier) @class.name)
(interface_declaration name: (identifier) @interface.name)
(enum_declaration name: (identifier) @enum.name)
(record_declaration name: (identifier) @record.name)
(annotation_type_declaration name: (identifier) @annotation.name)
(method_declaration name: (identifier) @method.name)
(constructor_declaration name: (identifier) @constructor.name)
(import_declaration) @import
"#;

pub struct JavaPlugin;

impl Plugin for JavaPlugin {
    fn language(&self) -> Language {
        Language::Java
    }

    fn extensions(&self) -> &[&str] {
        &["java"]
    }

    fn supports_phase_b(&self) -> bool {
        false
    }

    fn parse(&self, req: &ParseRequest) -> ParseResponse {
        parse_java_file(&req.corpus, &req.path, &req.vname_path).unwrap_or_else(|e| {
            tracing::warn!("java parse {}: {e}", req.path.display());
            ParseResponse::default()
        })
    }

    fn invoke_phase_b(&self, _req: &InvokeRequest) -> InvokeResponse {
        InvokeResponse::default()
    }
}

fn parse_java_file(
    corpus: &str,
    abs_path: &Path,
    vname_path: &str,
) -> anyhow::Result<ParseResponse> {
    let size = std::fs::metadata(abs_path)?.len();
    anyhow::ensure!(size <= MAX_FILE_BYTES, "file too large: {size} bytes");
    let source = std::fs::read(abs_path)?;

    let mut parser = Parser::new();
    let lang_obj = tree_sitter::Language::new(tree_sitter_java::LANGUAGE);
    parser
        .set_language(&lang_obj)
        .context("loading Java grammar")?;

    let tree = parser.parse(&source, None).context("parse timeout")?;
    let query = Query::new(&lang_obj, JAVA_QUERIES).context("building Java query")?;
    let mut cursor = QueryCursor::new();

    let file_vname = VName::new(corpus, "", vname_path, "java", "file");
    let file_node = Node::new(file_vname, "file");
    let mut nodes: Vec<Node> = vec![file_node];
    let mut ffi_markers: Vec<WireFfi> = vec![];
    let names = query.capture_names();

    let mut iter = cursor.matches(&query, tree.root_node(), source.as_slice());
    while let Some(m) = streaming_iterator::StreamingIterator::next(&mut iter) {
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
                    let parent = cap.node.parent();
                    let is_native = parent
                        .and_then(|p| p.utf8_text(&source).ok())
                        .map(|s| s.contains("native "))
                        .unwrap_or(false);
                    let node = Node::new(vn, "method")
                        .with_line(line)
                        .with_end_line(end_line);
                    let node_id = node.id;
                    nodes.push(node);
                    if is_native {
                        ffi_markers.push(WireFfi {
                            source_node_id: node_id.0,
                            kind: WireKind::JniExport,
                            local_name: text.clone(),
                            bound_name: None,
                            arity: None,
                            module: None,
                            corpus: corpus.to_string(),
                        });
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

    Ok(ParseResponse {
        nodes,
        edges,
        ffi_markers,
    })
}
