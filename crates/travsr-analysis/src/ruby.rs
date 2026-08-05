//! Phase A parser for Ruby source files using tree-sitter.

use std::path::Path;

use travsr_core::Language;

use crate::generic::{parse_with_config, LanguageConfig};
use crate::ParseOutput;

pub const CONFIG: LanguageConfig = LanguageConfig {
    language: Language::Ruby,
    extensions: &["rb", "rake", "gemspec"],
    queries: r#"
(class name: (constant) @class.name)
(module name: (constant) @module.name)
(method name: (identifier) @fn.name)
(singleton_method name: (identifier) @fn.name)
"#,
    capture_kinds: &[
        ("class.name", "class", "class"),
        ("module.name", "class", "class"),
        ("fn.name", "function", "fn"),
    ],
    method_containers: &[("class", "class"), ("module", "class")],
    decl_kinds: &[],
    get_grammar: || tree_sitter::Language::new(tree_sitter_ruby::LANGUAGE),
};

/// Parse a Ruby source file into graph nodes and edges.
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
        let path = dir.path().join("empty.rb");
        std::fs::write(&path, "").unwrap();
        let out = parse("corp", &path, "empty.rb").unwrap();
        assert_eq!(out.nodes.len(), 1);
    }

    #[test]
    fn parse_class_and_methods() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.rb");
        std::fs::write(
            &path,
            "module M\nclass Foo\ndef bar; end\ndef self.baz; end\nend\nend\n",
        )
        .unwrap();
        let out = parse("corp", &path, "sample.rb").unwrap();
        let kinds: Vec<&str> = out.nodes.iter().map(|n| n.kind.as_str()).collect();
        assert!(kinds.contains(&"class"));
        // N1: both methods are qualified by `Foo`. `def self.baz` no longer
        // collides with a same-named instance method (the ruby.rs defect).
        assert_eq!(kinds.iter().filter(|&&k| k == "method").count(), 2);
        let sigs: Vec<&str> = out
            .nodes
            .iter()
            .map(|n| n.vname.signature.as_str())
            .collect();
        assert!(sigs.contains(&"method:Foo.bar"), "got {sigs:?}");
        assert!(sigs.contains(&"method:Foo.baz"), "got {sigs:?}");
    }
}
