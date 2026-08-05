//! Phase A parser for Kotlin source files using tree-sitter.

use std::path::Path;

use travsr_core::Language;

use crate::generic::{parse_with_config, LanguageConfig};
use crate::ParseOutput;

pub const CONFIG: LanguageConfig = LanguageConfig {
    language: Language::Kotlin,
    extensions: &["kt", "kts"],
    queries: r#"
(class_declaration name: (identifier) @class.name)
(object_declaration name: (identifier) @object.name)
(function_declaration name: (identifier) @fn.name)
(type_alias (identifier) @typealias.name)
(import) @import
"#,
    capture_kinds: &[
        ("class.name", "class", "class"),
        ("object.name", "object", "class"),
        ("fn.name", "function", "fn"),
        ("typealias.name", "type", "type"),
        ("import", "import", "import"),
    ],
    method_containers: &[
        ("class_declaration", "class"),
        ("object_declaration", "class"),
    ],
    decl_kinds: &[],
    get_grammar: || tree_sitter::Language::new(tree_sitter_kotlin_ng::LANGUAGE),
};

/// Parse a Kotlin source file into graph nodes and edges.
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
        let path = dir.path().join("empty.kt");
        std::fs::write(&path, "").unwrap();
        let out = parse("corp", &path, "empty.kt").unwrap();
        assert_eq!(out.nodes.len(), 1);
    }

    #[test]
    fn parse_class_and_function() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.kt");
        std::fs::write(
            &path,
            "import kotlin.text.*\nclass Foo\nobject Bar\nfun baz() {}\n",
        )
        .unwrap();
        let out = parse("corp", &path, "sample.kt").unwrap();
        let kinds: Vec<&str> = out.nodes.iter().map(|n| n.kind.as_str()).collect();
        assert!(kinds.contains(&"class"));
        assert!(kinds.contains(&"object"));
        assert!(kinds.contains(&"function"));
        assert!(kinds.contains(&"import"));
    }
}
