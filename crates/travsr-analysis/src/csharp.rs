//! Phase A parser for C# source files using tree-sitter.

use std::path::Path;

use travsr_core::Language;

use crate::generic::{parse_with_config, LanguageConfig};
use crate::ParseOutput;

pub const CONFIG: LanguageConfig = LanguageConfig {
    language: Language::CSharp,
    extensions: &["cs"],
    queries: r#"
(class_declaration name: (identifier) @class.name)
(interface_declaration name: (identifier) @interface.name)
(struct_declaration name: (identifier) @struct.name)
(enum_declaration name: (identifier) @enum.name)
(record_declaration name: (identifier) @class.name)
(delegate_declaration name: (identifier) @delegate.name)
(method_declaration name: (identifier) @fn.name)
(constructor_declaration name: (identifier) @fn.name)
(using_directive) @import
"#,
    capture_kinds: &[
        ("class.name", "class", "class"),
        ("interface.name", "interface", "interface"),
        ("struct.name", "class", "class"),
        ("enum.name", "enum", "enum"),
        ("delegate.name", "delegate", "type"),
        ("fn.name", "function", "fn"),
        ("import", "import", "import"),
    ],
    method_containers: &[
        ("class_declaration", "class"),
        ("struct_declaration", "class"),
        ("record_declaration", "class"),
        ("interface_declaration", "interface"),
        ("enum_declaration", "enum"),
    ],
    decl_kinds: &[],
    get_grammar: || tree_sitter::Language::new(tree_sitter_c_sharp::LANGUAGE),
};

/// Parse a C# source file into graph nodes and edges.
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
        let path = dir.path().join("empty.cs");
        std::fs::write(&path, "").unwrap();
        let out = parse("corp", &path, "empty.cs").unwrap();
        assert_eq!(out.nodes.len(), 1);
    }

    #[test]
    fn parse_class_and_interface() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.cs");
        std::fs::write(
            &path,
            "using System;\nclass Foo : IFoo { void Bar() {} }\ninterface IFoo {}\n",
        )
        .unwrap();
        let out = parse("corp", &path, "sample.cs").unwrap();
        let kinds: Vec<&str> = out.nodes.iter().map(|n| n.kind.as_str()).collect();
        assert!(kinds.contains(&"class"));
        assert!(kinds.contains(&"interface"));
        assert!(kinds.contains(&"import"));
        // N1: `void Bar()` inside `class Foo` is a method, qualified by its type.
        assert!(
            out.nodes
                .iter()
                .any(|n| n.kind == "method" && n.vname.signature == "method:Foo.Bar"),
            "expected method:Foo.Bar; got {:?}",
            out.nodes
                .iter()
                .map(|n| &n.vname.signature)
                .collect::<Vec<_>>()
        );
    }
}
