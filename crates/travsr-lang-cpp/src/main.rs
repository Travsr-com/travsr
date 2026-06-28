//! travsr-lang-cpp — Phase A sidecar for C++.
//!
//! Carries the tree-sitter-cpp grammar blob so it does NOT live in the main
//! travsr binary (RFC-013 Direction A, §4). Spawned by travsr-plugin-host via
//! Sidecar::spawn; communicates over stdin/stdout using the plugin wire protocol.

use anyhow::Context as _;
use std::sync::Mutex;
use travsr_plugin_sdk::{
    run_plugin, InvokeRequest, InvokeResponse, Language, Node, ParseRequest, ParseResponse, Plugin,
    VName,
};
use tree_sitter::{Parser, Query, QueryCursor};

const EXTENSIONS: &[&str] = &["cpp", "cc", "cxx", "hpp", "hh", "hxx"];

const QUERIES: &str = r#"
(class_specifier name: (type_identifier) @class.name)
(struct_specifier name: (type_identifier) @struct.name)
(union_specifier name: (type_identifier) @union.name)
(enum_specifier name: (type_identifier) @enum.name)
(namespace_definition name: (namespace_identifier) @namespace.name)
(function_declarator declarator: (identifier) @fn.name)
(function_declarator declarator: (field_identifier) @fn.name)
(preproc_include path: (_) @import)
"#;

const CAPTURE_KINDS: &[(&str, &str, &str)] = &[
    ("class.name", "class", "class"),
    ("struct.name", "struct", "struct"),
    ("union.name", "union", "struct"),
    ("enum.name", "enum", "enum"),
    ("namespace.name", "namespace", "namespace"),
    ("fn.name", "function", "fn"),
    ("import", "import", "import"),
];

const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
const PARSE_TIMEOUT_MICROS: u64 = 5_000_000;

struct CppPlugin {
    grammar: tree_sitter::Language,
    query: Query,
    // Reused across files — avoids Parser::new() + set_language() per file.
    // Mutex because Plugin: Send + Sync; never actually contended (single-threaded sidecar).
    parser: Mutex<Parser>,
}

impl CppPlugin {
    fn new() -> Self {
        let grammar = tree_sitter_cpp::language();
        let query = Query::new(&grammar, QUERIES)
            .expect("invalid C++ tree-sitter query — this is a bug in travsr-lang-cpp");
        let mut parser = Parser::new();
        parser
            .set_language(&grammar)
            .expect("failed to set C++ grammar on parser");
        Self {
            grammar,
            query,
            parser: Mutex::new(parser),
        }
    }
}

impl Plugin for CppPlugin {
    fn language(&self) -> Language {
        Language::Cpp
    }

    fn extensions(&self) -> &[&str] {
        EXTENSIONS
    }

    fn supports_phase_b(&self) -> bool {
        false
    }

    fn parse(&self, req: &ParseRequest) -> ParseResponse {
        parse_cpp(&self.grammar, &self.query, &self.parser, req).unwrap_or_else(|e| {
            tracing::warn!("cpp parse {}: {e}", req.path.display());
            ParseResponse::default()
        })
    }

    fn invoke_phase_b(&self, _req: &InvokeRequest) -> InvokeResponse {
        InvokeResponse::default()
    }
}

fn parse_cpp(
    _grammar: &tree_sitter::Language,
    query: &Query,
    parser_mutex: &Mutex<Parser>,
    req: &ParseRequest,
) -> anyhow::Result<ParseResponse> {
    // Use source bytes sent by the host if available (avoids re-reading the
    // file from inside the bwrap sandbox). Fall back to a direct read for
    // callers that don't supply source (e.g. tests, future CLI tools).
    let source: Vec<u8> = if let Some(s) = req.source.clone() {
        s
    } else {
        let size = std::fs::metadata(&req.path)
            .with_context(|| format!("stat {}", req.path.display()))?
            .len();
        anyhow::ensure!(size <= MAX_FILE_BYTES, "file too large: {} bytes", size);
        std::fs::read(&req.path).with_context(|| format!("reading {}", req.path.display()))?
    };

    anyhow::ensure!(
        source.len() as u64 <= MAX_FILE_BYTES,
        "file too large: {} bytes",
        source.len()
    );

    let tree = {
        let mut parser = parser_mutex.lock().expect("parser mutex poisoned");
        // reset() clears incremental-parse state from the previous file so
        // the parser starts fresh. Far cheaper than Parser::new() each call.
        parser.reset();
        parser.set_timeout_micros(PARSE_TIMEOUT_MICROS);
        parser
            .parse(&source, None)
            .context("parse timed out or returned None")?
    };

    let file_vname = VName::new(&req.corpus, "", &req.vname_path, "cpp", "file");
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
                // Strip leading `#include` keyword, quotes, and angle brackets
                // to produce a stable `import:<header>` signature.
                let cleaned = text
                    .trim_start_matches('#')
                    .trim_start_matches("include")
                    .trim()
                    .trim_matches(|c| c == '<' || c == '>' || c == '"')
                    .trim();
                format!("import:{cleaned}")
            } else {
                format!("{sig_prefix}:{text}")
            };

            let vname = VName::new(&req.corpus, "", &req.vname_path, "cpp", &sig);
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
    run_plugin(CppPlugin::new());
}
