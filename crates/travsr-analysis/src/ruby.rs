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
(call
  method: (identifier) @require.kw
  arguments: (argument_list (string (string_content) @import))
  (#any-of? @require.kw "require" "require_relative"))
"#,
    capture_kinds: &[
        ("class.name", "class", "class"),
        ("module.name", "class", "class"),
        ("fn.name", "function", "fn"),
        // N4b: `require`/`require_relative` string args become import nodes +
        // Depends edges. The `#any-of?` predicate is auto-applied by
        // tree-sitter's match iterator, so only require calls match; the
        // `@require.kw` capture has no capture_kinds entry and is ignored.
        ("import", "import", "import"),
    ],
    method_containers: &[("class", "class"), ("module", "class")],
    decl_kinds: &[],
    type_refinements: &[],
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
    fn n4b_require_imports_and_depends_edges() {
        // N4b: `require`/`require_relative` produce `import:` nodes and
        // file->import `Depends` edges (Ruby previously emitted zero). A
        // non-require call (`puts`) must NOT produce an import — the
        // `#any-of?` predicate filters the match.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("boot.rb");
        std::fs::write(
            &path,
            "require 'set'\nrequire_relative './foo'\nputs 'hi'\n",
        )
        .unwrap();
        let out = parse("corp", &path, "boot.rb").unwrap();

        let import_sigs: Vec<&str> = out
            .nodes
            .iter()
            .filter(|n| n.kind == "import")
            .map(|n| n.vname.signature.as_str())
            .collect();
        assert!(import_sigs.contains(&"import:set"), "got {import_sigs:?}");
        assert!(import_sigs.contains(&"import:./foo"), "got {import_sigs:?}");
        assert_eq!(
            import_sigs.len(),
            2,
            "no import for `puts`: {import_sigs:?}"
        );

        let depends = out
            .edges
            .iter()
            .filter(|e| e.kind == travsr_core::EdgeKind::Depends)
            .count();
        assert_eq!(depends, 2, "one Depends edge per require");
    }

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
