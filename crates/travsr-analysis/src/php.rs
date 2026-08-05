//! Phase A parser for PHP source files using tree-sitter.

use std::path::Path;

use travsr_core::Language;

use crate::generic::{parse_with_config, LanguageConfig};
use crate::ParseOutput;

pub const CONFIG: LanguageConfig = LanguageConfig {
    language: Language::Php,
    extensions: &["php", "phtml", "php8"],
    queries: r#"
(class_declaration name: (name) @class.name)
(interface_declaration name: (name) @interface.name)
(trait_declaration name: (name) @class.name)
(enum_declaration name: (name) @enum.name)
(function_definition name: (name) @fn.name)
(method_declaration name: (name) @fn.name)
(namespace_use_clause) @import
"#,
    capture_kinds: &[
        ("class.name", "class", "class"),
        ("interface.name", "interface", "interface"),
        ("enum.name", "enum", "enum"),
        ("fn.name", "function", "fn"),
        ("import", "import", "import"),
    ],
    method_containers: &[
        ("class_declaration", "class"),
        ("interface_declaration", "interface"),
        ("trait_declaration", "class"),
        ("enum_declaration", "enum"),
    ],
    get_grammar: || tree_sitter::Language::new(tree_sitter_php::LANGUAGE_PHP),
};

/// Parse a PHP source file into graph nodes and edges.
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
        let path = dir.path().join("empty.php");
        std::fs::write(&path, "<?php\n").unwrap();
        let out = parse("corp", &path, "empty.php").unwrap();
        assert_eq!(out.nodes.len(), 1);
    }

    #[test]
    fn parse_class_and_function() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.php");
        std::fs::write(
            &path,
            "<?php\nuse App\\Models\\User;\nclass Foo {}\ninterface Bar {}\nfunction baz() {}\n",
        )
        .unwrap();
        let out = parse("corp", &path, "sample.php").unwrap();
        let kinds: Vec<&str> = out.nodes.iter().map(|n| n.kind.as_str()).collect();
        assert!(kinds.contains(&"class"));
        assert!(kinds.contains(&"interface"));
        assert!(kinds.contains(&"function"));
        assert!(kinds.contains(&"import"));
    }
}
