//! Phase A parser for Swift source files using tree-sitter.

use std::path::Path;

use travsr_core::Language;

use crate::generic::{parse_with_config, LanguageConfig};
use crate::ParseOutput;

pub const CONFIG: LanguageConfig = LanguageConfig {
    language: Language::Swift,
    extensions: &["swift"],
    queries: r#"
(class_declaration    name: (type_identifier) @class.name)
(protocol_declaration name: (type_identifier) @protocol.name)
(typealias_declaration name: (type_identifier) @typealias.name)
(function_declaration name: (simple_identifier) @fn.name)
(init_declaration)    @init
(import_declaration)  @import
"#,
    capture_kinds: &[
        ("class.name", "class", "class"),
        ("protocol.name", "protocol", "class"),
        ("typealias.name", "type", "type"),
        ("fn.name", "function", "fn"),
        ("init", "init", "fn"),
        ("import", "import", "import"),
    ],
    get_grammar: || tree_sitter::Language::new(tree_sitter_swift::LANGUAGE),
};

/// Parse a Swift source file into graph nodes and edges.
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
        let path = dir.path().join("empty.swift");
        std::fs::write(&path, "").unwrap();
        let out = parse("corp", &path, "empty.swift").unwrap();
        assert_eq!(out.nodes.len(), 1);
    }

    #[test]
    fn parse_class_and_protocol() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.swift");
        std::fs::write(
            &path,
            "import Foundation\nclass Foo {}\nprotocol Bar {}\nfunc baz() {}\n",
        )
        .unwrap();
        let out = parse("corp", &path, "sample.swift").unwrap();
        let kinds: Vec<&str> = out.nodes.iter().map(|n| n.kind.as_str()).collect();
        assert!(kinds.contains(&"class"));
        assert!(kinds.contains(&"protocol"));
        assert!(kinds.contains(&"function"));
        assert!(kinds.contains(&"import"));
    }
}
