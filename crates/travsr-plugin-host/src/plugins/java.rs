use std::path::Path;

use anyhow::Context as _;
use travsr_core::{Language, Node, VName};
use travsr_plugin_protocol::{
    FfiMarker as WireFfi, FfiMarkerKind as WireKind, InvokeRequest, InvokeResponse, ParseRequest,
    ParseResponse, Plugin,
};
use tree_sitter::{Parser, Query, QueryCursor};

const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024; // 10 MB
const PARSE_TIMEOUT_MICROS: u64 = 5_000_000; // 5 seconds

const JAVA_QUERIES: &str = r#"
(class_declaration name: (identifier) @class.name)
(interface_declaration name: (identifier) @interface.name)
(enum_declaration name: (identifier) @enum.name)
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
    let lang_obj = tree_sitter_java::language();
    parser
        .set_language(&lang_obj)
        .context("loading Java grammar")?;
    parser.set_timeout_micros(PARSE_TIMEOUT_MICROS);

    let tree = parser.parse(&source, None).context("parse timeout")?;
    let query = Query::new(&lang_obj, JAVA_QUERIES).context("building Java query")?;
    let mut cursor = QueryCursor::new();

    let file_vname = VName::new(corpus, "", vname_path, "java", "file");
    let file_node = Node::new(file_vname, "file");
    let mut nodes: Vec<Node> = vec![file_node];
    let mut ffi_markers: Vec<WireFfi> = vec![];
    let names = query.capture_names();

    for m in cursor.matches(&query, tree.root_node(), source.as_slice()) {
        for cap in m.captures {
            let cap_name = &names[cap.index as usize];
            let text = cap.node.utf8_text(&source).unwrap_or("").to_string();
            let line = cap.node.start_position().row as u32 + 1;

            match *cap_name {
                "class.name" => {
                    let vn = VName::new(corpus, "", vname_path, "java", format!("class:{text}"));
                    nodes.push(Node::new(vn, "class").with_line(line));
                }
                "interface.name" => {
                    let vn =
                        VName::new(corpus, "", vname_path, "java", format!("interface:{text}"));
                    nodes.push(Node::new(vn, "interface").with_line(line));
                }
                "enum.name" => {
                    let vn = VName::new(corpus, "", vname_path, "java", format!("enum:{text}"));
                    nodes.push(Node::new(vn, "enum").with_line(line));
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
                    let node = Node::new(vn, "method").with_line(line);
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
                    nodes.push(Node::new(vn, "constructor").with_line(line));
                }
                "import" => {
                    let raw = cap.node.utf8_text(&source).unwrap_or("").trim().to_string();
                    let module = raw
                        .trim_start_matches("import ")
                        .trim_start_matches("static ")
                        .trim_end_matches(';')
                        .trim()
                        .to_string();
                    let vn =
                        VName::new(corpus, "", vname_path, "java", format!("import:{module}"));
                    nodes.push(Node::new(vn, "import").with_line(line));
                }
                _ => {}
            }
        }
    }

    Ok(ParseResponse {
        nodes,
        edges: vec![],
        ffi_markers,
    })
}
