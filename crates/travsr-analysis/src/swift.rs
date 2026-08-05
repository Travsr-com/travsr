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
(property_declaration name: (pattern bound_identifier: (simple_identifier) @var.name))
"#,
    capture_kinds: &[
        ("class.name", "class", "class"),
        ("protocol.name", "protocol", "class"),
        ("typealias.name", "type", "type"),
        ("fn.name", "function", "fn"),
        ("init", "init", "fn"),
        ("import", "import", "import"),
        // #449: properties (`static let shared`) gives `ClassC.shared` a
        // tree-sitter node so the Phase B field node unifies onto it and dotted
        // queries resolve.
        ("var.name", "field", "var"),
    ],
    method_containers: &[
        ("class_declaration", "class"),
        ("protocol_declaration", "class"),
    ],
    decl_kinds: &[],
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

    #[test]
    fn parse_property_declaration() {
        // #449: `static let shared` must produce a field node (sig `var:shared`)
        // so Phase B `swift::ClassC.shared` unifies and dotted queries resolve.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("singleton.swift");
        std::fs::write(
            &path,
            "class ClassC {\n    static let shared = ClassC()\n    var count: Int = 0\n}\n",
        )
        .unwrap();
        let out = parse("corp", &path, "singleton.swift").unwrap();
        let fields: Vec<&travsr_core::Node> =
            out.nodes.iter().filter(|n| n.kind == "field").collect();
        let sigs: Vec<&str> = fields.iter().map(|n| n.vname.signature.as_str()).collect();
        assert!(sigs.contains(&"var:shared"), "got field sigs: {sigs:?}");
        assert!(sigs.contains(&"var:count"), "got field sigs: {sigs:?}");
    }
}
