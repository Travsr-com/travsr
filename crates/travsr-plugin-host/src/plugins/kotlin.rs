use std::path::Path;

use anyhow::Context as _;
use tree_sitter::{Parser, Query, QueryCursor};
use travsr_core::{Language, Node, VName};
use travsr_plugin_protocol::{
    InvokeRequest, InvokeResponse, ParseRequest, ParseResponse, Plugin,
};

const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024; // 10 MB
const PARSE_TIMEOUT_MICROS: u64 = 5_000_000; // 5 seconds

const KOTLIN_QUERIES: &str = r#"
(class_declaration (type_identifier) @class.name)
(object_declaration (type_identifier) @object.name)
(function_declaration (simple_identifier) @fn.name)
(import_header) @import
"#;

pub struct KotlinPlugin;

impl Plugin for KotlinPlugin {
    fn language(&self) -> Language {
        Language::Kotlin
    }

    fn extensions(&self) -> &[&str] {
        &["kt", "kts"]
    }

    fn supports_phase_b(&self) -> bool {
        false
    }

    fn parse(&self, req: &ParseRequest) -> ParseResponse {
        parse_kotlin_file(&req.corpus, &req.path, &req.vname_path).unwrap_or_else(|e| {
            tracing::warn!("kotlin parse {}: {e}", req.path.display());
            ParseResponse::default()
        })
    }

    fn invoke_phase_b(&self, _req: &InvokeRequest) -> InvokeResponse {
        InvokeResponse::default()
    }
}

fn parse_kotlin_file(
    corpus: &str,
    abs_path: &Path,
    vname_path: &str,
) -> anyhow::Result<ParseResponse> {
    let size = std::fs::metadata(abs_path)?.len();
    anyhow::ensure!(size <= MAX_FILE_BYTES, "file too large: {size} bytes");
    let source = std::fs::read(abs_path)?;

    let mut parser = Parser::new();
    let lang_obj: tree_sitter::Language = tree_sitter_kotlin::language().into();
    parser
        .set_language(&lang_obj)
        .context("loading Kotlin grammar")?;
    parser.set_timeout_micros(PARSE_TIMEOUT_MICROS);

    let tree = parser.parse(&source, None).context("parse timeout")?;
    let query = Query::new(&lang_obj, KOTLIN_QUERIES).context("building Kotlin query")?;
    let mut cursor = QueryCursor::new();

    let file_vname = VName::new(corpus, "", vname_path, "kotlin", "file");
    let file_node = Node::new(file_vname, "file");
    let mut nodes: Vec<Node> = vec![file_node];
    let names = query.capture_names();

    for m in cursor.matches(&query, tree.root_node(), source.as_slice()) {
        for cap in m.captures {
            let cap_name = &names[cap.index as usize];
            let text = cap.node.utf8_text(&source).unwrap_or("").to_string();
            let line = cap.node.start_position().row as u32 + 1;

            match *cap_name {
                "class.name" => {
                    let vn =
                        VName::new(corpus, "", vname_path, "kotlin", &format!("class:{text}"));
                    nodes.push(Node::new(vn, "class").with_line(line));
                }
                "object.name" => {
                    // Kotlin objects (singletons) use class: prefix for consistency.
                    let vn =
                        VName::new(corpus, "", vname_path, "kotlin", &format!("class:{text}"));
                    nodes.push(Node::new(vn, "object").with_line(line));
                }
                "fn.name" => {
                    let vn =
                        VName::new(corpus, "", vname_path, "kotlin", &format!("fn:{text}"));
                    nodes.push(Node::new(vn, "function").with_line(line));
                }
                "import" => {
                    let raw = cap.node.utf8_text(&source).unwrap_or("").trim().to_string();
                    let module = raw.trim_start_matches("import ").trim().to_string();
                    let vn = VName::new(
                        corpus,
                        "",
                        vname_path,
                        "kotlin",
                        &format!("import:{module}"),
                    );
                    nodes.push(Node::new(vn, "import").with_line(line));
                }
                _ => {}
            }
        }
    }

    Ok(ParseResponse {
        nodes,
        edges: vec![],
        ffi_markers: vec![],
    })
}
