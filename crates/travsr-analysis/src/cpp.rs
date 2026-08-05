//! Phase A parser for C++ source files using tree-sitter.

use std::path::Path;

use travsr_core::Language;

use crate::generic::{parse_with_config, LanguageConfig};
use crate::ParseOutput;

pub const CONFIG: LanguageConfig = LanguageConfig {
    language: Language::Cpp,
    extensions: &["cpp", "cc", "cxx", "hpp", "hh", "hxx"],
    queries: r#"
(class_specifier name: (type_identifier) @class.name)
(struct_specifier name: (type_identifier) @struct.name)
(union_specifier name: (type_identifier) @union.name)
(enum_specifier name: (type_identifier) @enum.name)
(namespace_definition name: (namespace_identifier) @namespace.name)
(alias_declaration name: (type_identifier) @using.name)
(function_declarator declarator: (identifier) @fn.name)
(function_declarator declarator: (field_identifier) @fn.name)
(preproc_include path: (_) @import)
"#,
    capture_kinds: &[
        ("class.name", "class", "class"),
        ("struct.name", "struct", "struct"),
        ("union.name", "union", "struct"),
        ("enum.name", "enum", "enum"),
        ("namespace.name", "namespace", "namespace"),
        ("using.name", "typedef", "type"),
        ("fn.name", "function", "fn"),
        ("import", "import", "import"),
    ],
    method_containers: &[("class_specifier", "class"), ("struct_specifier", "struct")],
    get_grammar: || tree_sitter::Language::new(tree_sitter_cpp::LANGUAGE),
};

/// Parse a C++ source file into graph nodes and edges.
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
        let path = dir.path().join("empty.cpp");
        std::fs::write(&path, "").unwrap();
        let out = parse("corp", &path, "empty.cpp").unwrap();
        assert_eq!(out.nodes.len(), 1, "file node only");
    }

    #[test]
    fn parse_class_and_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.cpp");
        std::fs::write(&path, "namespace ns { class Foo {}; }\n").unwrap();
        let out = parse("corp", &path, "sample.cpp").unwrap();
        let kinds: Vec<&str> = out.nodes.iter().map(|n| n.kind.as_str()).collect();
        assert!(kinds.contains(&"class"));
        assert!(kinds.contains(&"namespace"));
    }
}
