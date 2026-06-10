//! Config-driven Phase A plugin using tree-sitter.
//!
//! All Phase A work is fundamentally identical across languages:
//! load a grammar, run queries, map capture names to node kinds.
//! `GenericTreeSitterPlugin` expresses that as data so new languages
//! require zero new Rust code — only a `LanguageConfig` constant.

use std::path::Path;

use anyhow::Context as _;
use streaming_iterator::StreamingIterator as _;
use travsr_core::{Edge, EdgeKind, Language, Node, VName};
use travsr_plugin_protocol::{InvokeRequest, InvokeResponse, ParseRequest, ParseResponse, Plugin};
use tree_sitter::{Parser, Query, QueryCursor};

const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// A complete Phase A language definition expressed as data.
/// All fields are `'static` so configs can be declared as `const`.
pub struct LanguageConfig {
    pub language: Language,
    pub extensions: &'static [&'static str],
    /// tree-sitter query string. Compiled once at plugin construction.
    pub queries: &'static str,
    /// Maps capture name → (node_kind, signature_prefix).
    ///
    /// For regular nodes: `sig = "{prefix}:{captured_text}"`.
    /// Special prefix `"import"` → use the full node text, strip leading
    /// keyword (`import`, `use`, `require`, `using`) and trailing `;`.
    pub capture_kinds: &'static [(&'static str, &'static str, &'static str)],
}

/// A Phase A plugin driven entirely by a `LanguageConfig` — no per-language
/// Rust logic required. Phase B always returns empty (language-specific
/// invokers live in dedicated plugin crates).
pub struct GenericTreeSitterPlugin {
    config: &'static LanguageConfig,
    grammar: tree_sitter::Language,
    compiled_query: Query,
}

impl GenericTreeSitterPlugin {
    /// Construct and compile the tree-sitter query.
    /// Panics at startup if the query string is invalid — misconfiguration
    /// should be caught at development time, not silently at runtime.
    pub fn new(config: &'static LanguageConfig, grammar: tree_sitter::Language) -> Self {
        let compiled_query = Query::new(&grammar, config.queries).unwrap_or_else(|e| {
            panic!(
                "invalid tree-sitter query for language {:?}: {e}",
                config.language.as_str()
            )
        });
        Self {
            config,
            grammar,
            compiled_query,
        }
    }
}

impl Plugin for GenericTreeSitterPlugin {
    fn language(&self) -> Language {
        self.config.language
    }
    fn extensions(&self) -> &[&str] {
        self.config.extensions
    }
    fn supports_phase_b(&self) -> bool {
        false
    }

    fn parse(&self, req: &ParseRequest) -> ParseResponse {
        parse_generic(
            &self.grammar,
            &self.compiled_query,
            self.config,
            &req.corpus,
            &req.path,
            &req.vname_path,
        )
        .unwrap_or_else(|e| {
            tracing::warn!(
                "{} parse {}: {e}",
                self.config.language.as_str(),
                req.path.display()
            );
            ParseResponse::default()
        })
    }

    fn invoke_phase_b(&self, _req: &InvokeRequest) -> InvokeResponse {
        InvokeResponse::default()
    }
}

// ── Core parsing logic ────────────────────────────────────────────────────────

fn parse_generic(
    grammar: &tree_sitter::Language,
    query: &Query,
    config: &LanguageConfig,
    corpus: &str,
    abs_path: &Path,
    vname_path: &str,
) -> anyhow::Result<ParseResponse> {
    let size = std::fs::metadata(abs_path)
        .with_context(|| format!("stat {}", abs_path.display()))?
        .len();
    anyhow::ensure!(size <= MAX_FILE_BYTES, "file too large: {size} bytes");

    let source =
        std::fs::read(abs_path).with_context(|| format!("reading {}", abs_path.display()))?;

    let mut parser = Parser::new();
    parser.set_language(grammar).context("set language")?;

    let tree = parser.parse(&source, None).context("parse timeout")?;

    let lang_str = config.language.as_str();
    let file_vname = VName::new(corpus, "", vname_path, lang_str, "file");
    let mut nodes = vec![Node::new(file_vname, "file")];

    let capture_names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut iter = cursor.matches(query, tree.root_node(), source.as_slice());

    while let Some(m) = iter.next() {
        for cap in m.captures {
            let cap_name = *capture_names.get(cap.index as usize).unwrap_or(&"");

            // Find the config entry for this capture
            let Some(&(_, node_kind, sig_prefix)) = config
                .capture_kinds
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
                // Use the full node text, strip leading keyword + trailing semicolons
                let cleaned = text
                    .trim_start_matches("import ")
                    .trim_start_matches("use ")
                    .trim_start_matches("require ")
                    .trim_start_matches("using ")
                    .trim_end_matches(';')
                    .trim()
                    .to_string();
                format!("import:{cleaned}")
            } else {
                format!("{sig_prefix}:{text}")
            };

            let vname = VName::new(corpus, "", vname_path, lang_str, &sig);
            nodes.push(Node::new(vname, node_kind).with_line(line));
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
        ffi_markers: vec![],
    })
}
