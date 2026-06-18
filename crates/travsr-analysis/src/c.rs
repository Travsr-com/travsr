//! Phase A parser for C source files using tree-sitter.

use std::path::Path;

use travsr_core::Language;

use crate::generic::{parse_with_config, LanguageConfig};
use crate::ParseOutput;

pub const CONFIG: LanguageConfig = LanguageConfig {
    language: Language::C,
    extensions: &["c", "h"],
    queries: r#"
(struct_specifier name: (type_identifier) @struct.name)
(union_specifier name: (type_identifier) @union.name)
(enum_specifier name: (type_identifier) @enum.name)
(type_definition declarator: (type_identifier) @typedef.name)
(function_declarator declarator: (identifier) @fn.name)
(preproc_include path: (_) @import)
"#,
    capture_kinds: &[
        ("struct.name", "struct", "struct"),
        ("union.name", "union", "struct"),
        ("enum.name", "enum", "enum"),
        ("typedef.name", "typedef", "type"),
        ("fn.name", "function", "fn"),
        ("import", "import", "import"),
    ],
    get_grammar: || tree_sitter::Language::new(tree_sitter_c::LANGUAGE),
};

/// Parse a C source file into graph nodes and edges.
pub fn parse(corpus: &str, abs_path: &Path, vname_path: &str) -> anyhow::Result<ParseOutput> {
    let grammar = (CONFIG.get_grammar)();
    parse_with_config(&CONFIG, &grammar, None, corpus, abs_path, vname_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.c");
        std::fs::write(&path, "").unwrap();
        let out = parse("corp", &path, "empty.c").unwrap();
        assert_eq!(out.nodes.len(), 1, "file node only");
        assert!(out.edges.is_empty());
    }

    #[test]
    fn parse_struct_and_function() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.c");
        std::fs::write(&path, "struct Point { int x; int y; };\nvoid foo() {}\n").unwrap();
        let out = parse("corp", &path, "sample.c").unwrap();
        let kinds: Vec<&str> = out.nodes.iter().map(|n| n.kind.as_str()).collect();
        assert!(kinds.contains(&"file"), "file node present");
        assert!(kinds.contains(&"struct"), "struct node present");
        assert!(kinds.contains(&"function"), "function node present");
    }
}
