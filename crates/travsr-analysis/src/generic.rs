//! Config-driven Phase A parser for all generic languages.
//!
//! All Phase A work is structurally identical across languages: load a grammar,
//! run tree-sitter queries, map capture names to node kinds. `LanguageConfig`
//! expresses that as static data so adding a new language requires only a
//! new module with a `CONFIG` constant — no new Rust logic.
//!
//! Callers in `travsr-plugin-host` may cache the compiled `tree_sitter::Query`
//! internally (via `GenericTreeSitterPlugin`) to avoid re-compiling on every
//! file. Direct callers (e.g. per-language `parse()` wrappers) may compile
//! fresh; query compilation is fast (< 1 µs) and acceptable per-file.

use std::path::Path;

use anyhow::Context as _;
use streaming_iterator::StreamingIterator as _;
use travsr_core::{Edge, EdgeKind, Language, Node, VName};
use tree_sitter::{Parser, Query, QueryCursor};

use crate::ParseOutput;

const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024; // 10 MB

/// A complete Phase A language definition expressed as static data.
/// All fields are `'static` so configs can be declared as `const`.
pub struct LanguageConfig {
    pub language: Language,
    pub extensions: &'static [&'static str],
    /// tree-sitter query string compiled at first use.
    pub queries: &'static str,
    /// Maps `(capture_name, node_kind, signature_prefix)`.
    ///
    /// For regular nodes: `sig = "{prefix}:{captured_text}"`.
    /// Special prefix `"import"` → uses the full node text, strips the leading
    /// keyword (`import`, `use`, `require`, `using`) and trailing `;`.
    pub capture_kinds: &'static [(&'static str, &'static str, &'static str)],
    /// Returns the tree-sitter grammar for this language.
    /// Stored as a function pointer so `LanguageConfig` is `const`-constructible
    /// (tree-sitter `Language` itself is not directly `const`-constructible).
    pub get_grammar: fn() -> tree_sitter::Language,
}

/// Parse `abs_path` using the given grammar and `LanguageConfig`.
///
/// If the caller already has a pre-compiled `Query` (e.g. `GenericTreeSitterPlugin`),
/// pass it via `compiled_query`. Pass `None` to compile fresh from `config.queries`.
/// The grammar is obtained from `config.get_grammar()` when not already known.
///
/// # O(n) where n = number of AST nodes matching the query
pub fn parse_with_config(
    config: &LanguageConfig,
    grammar: &tree_sitter::Language,
    compiled_query: Option<&Query>,
    corpus: &str,
    abs_path: &Path,
    vname_path: &str,
) -> anyhow::Result<ParseOutput> {
    let size = std::fs::metadata(abs_path)
        .with_context(|| format!("stat {}", abs_path.display()))?
        .len();
    anyhow::ensure!(size <= MAX_FILE_BYTES, "file too large: {size} bytes");

    let source =
        std::fs::read(abs_path).with_context(|| format!("reading {}", abs_path.display()))?;

    let mut parser = Parser::new();
    parser.set_language(grammar).context("set language")?;
    let tree = parser.parse(&source, None).context("parse timeout")?;

    // Compile fresh or use the caller-supplied pre-compiled query.
    let owned_query;
    let query: &Query = match compiled_query {
        Some(q) => q,
        None => {
            owned_query = Query::new(grammar, config.queries).context("compile query")?;
            &owned_query
        }
    };

    let lang_str = config.language.as_str();
    let file_vname = VName::new(corpus, "", vname_path, lang_str, "file");
    let mut nodes = vec![Node::new(file_vname, "file")];

    let capture_names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut iter = cursor.matches(query, tree.root_node(), source.as_slice());

    while let Some(m) = iter.next() {
        for cap in m.captures {
            let cap_name = *capture_names.get(cap.index as usize).unwrap_or(&"");

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
            // G2: one hop from the name capture to the declaration node gives the full span.
            let end_line = cap
                .node
                .parent()
                .map(|p| p.end_position().row as u32 + 1)
                .unwrap_or(line);

            let sig = if sig_prefix == "import" {
                // Use the full node text, strip leading keyword + trailing semicolons.
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
            let mut node = Node::new(vname, node_kind).with_line(line);
            if sig_prefix != "import" {
                node = node.with_end_line(end_line);
            }
            nodes.push(node);
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
        ffi_markers: vec![],
    })
}
