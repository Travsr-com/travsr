//! travsr-lang-kotlin — Phase A sidecar for Kotlin.
//!
//! Carries the tree-sitter-kotlin grammar blob so it does NOT live in the main
//! travsr binary (RFC-013 Direction A, §4). Spawned by travsr-plugin-host via
//! Sidecar::spawn; communicates over stdin/stdout using the plugin wire protocol.

use anyhow::Context as _;
use travsr_plugin_sdk::{
    run_plugin, InvokeRequest, InvokeResponse, Language, Node, ParseRequest, ParseResponse, Plugin,
    VName,
};
use tree_sitter::{Parser, Query, QueryCursor};

const EXTENSIONS: &[&str] = &["kt", "kts"];

const QUERIES: &str = r#"
(class_declaration (type_identifier) @class.name)
(object_declaration (type_identifier) @object.name)
(function_declaration (simple_identifier) @fn.name)
(import_header) @import
"#;

const CAPTURE_KINDS: &[(&str, &str, &str)] = &[
    ("class.name", "class", "class"),
    ("object.name", "object", "class"),
    ("fn.name", "function", "fn"),
    ("import", "import", "import"),
];

const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
const PARSE_TIMEOUT_MICROS: u64 = 5_000_000;

struct KotlinPlugin {
    grammar: tree_sitter::Language,
    query: Query,
}

impl KotlinPlugin {
    fn new() -> Self {
        let grammar = tree_sitter_kotlin::language();
        let query = Query::new(&grammar, QUERIES)
            .expect("invalid Kotlin tree-sitter query — this is a bug in travsr-lang-kotlin");
        Self { grammar, query }
    }
}

impl Plugin for KotlinPlugin {
    fn language(&self) -> Language {
        Language::Kotlin
    }

    fn extensions(&self) -> &[&str] {
        EXTENSIONS
    }

    fn supports_phase_b(&self) -> bool {
        false
    }

    fn parse(&self, req: &ParseRequest) -> ParseResponse {
        parse_kotlin(&self.grammar, &self.query, req).unwrap_or_else(|e| {
            tracing::warn!("kotlin parse {}: {e}", req.path.display());
            ParseResponse::default()
        })
    }

    fn invoke_phase_b(&self, _req: &InvokeRequest) -> InvokeResponse {
        InvokeResponse::default()
    }
}

fn parse_kotlin(
    grammar: &tree_sitter::Language,
    query: &Query,
    req: &ParseRequest,
) -> anyhow::Result<ParseResponse> {
    let size = std::fs::metadata(&req.path)
        .with_context(|| format!("stat {}", req.path.display()))?
        .len();
    anyhow::ensure!(
        size <= MAX_FILE_BYTES,
        "file too large: {} bytes",
        size
    );

    let source =
        std::fs::read(&req.path).with_context(|| format!("reading {}", req.path.display()))?;

    let mut parser = Parser::new();
    parser.set_language(grammar).context("set Kotlin grammar")?;
    parser.set_timeout_micros(PARSE_TIMEOUT_MICROS);

    let tree = parser
        .parse(&source, None)
        .context("parse timed out or returned None")?;

    let file_vname = VName::new(&req.corpus, "", &req.vname_path, "kotlin", "file");
    let mut nodes = vec![Node::new(file_vname, "file")];

    let capture_names = query.capture_names();
    let mut cursor = QueryCursor::new();

    for m in cursor.matches(query, tree.root_node(), source.as_slice()) {
        for cap in m.captures {
            let cap_name = *capture_names.get(cap.index as usize).unwrap_or(&"");
            let Some(&(_, node_kind, sig_prefix)) = CAPTURE_KINDS
                .iter()
                .find(|(name, _, _)| *name == cap_name)
            else {
                continue;
            };

            let text = cap.node.utf8_text(&source).unwrap_or("").trim();
            if text.is_empty() {
                continue;
            }
            let line = cap.node.start_position().row as u32 + 1;

            let sig = if sig_prefix == "import" {
                // Strip the leading `import ` keyword and trailing semicolon
                // to produce a stable `import:<fully.qualified.Name>` signature.
                let cleaned = text
                    .trim_start_matches("import ")
                    .trim_end_matches(';')
                    .trim();
                format!("import:{cleaned}")
            } else {
                format!("{sig_prefix}:{text}")
            };

            let vname = VName::new(&req.corpus, "", &req.vname_path, "kotlin", &sig);
            nodes.push(Node::new(vname, node_kind).with_line(line));
        }
    }

    Ok(ParseResponse {
        nodes,
        edges: vec![],
        ffi_markers: vec![],
    })
}

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("error")),
        )
        .init();
    run_plugin(KotlinPlugin::new());
}
