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
  method: (identifier) @require_relative.kw
  arguments: (argument_list (string (string_content) @import))
  (#eq? @require_relative.kw "require_relative"))
(call
  method: (identifier) @require.kw
  arguments: (argument_list (string (string_content) @import.gem))
  (#eq? @require.kw "require"))
(class
  superclass: (superclass (scope_resolution name: (constant) @_rs))
  (#any-of? @_rs "Test" "TestCase")) @test.scope
(class
  superclass: (superclass (scope_resolution name: (constant) @_rs2))
  body: (body_statement (method name: (identifier) @test.entry))
  (#any-of? @_rs2 "Test" "TestCase")
  (#match? @test.entry "^test"))
"#,
    capture_kinds: &[
        ("class.name", "class", "class"),
        ("module.name", "class", "class"),
        ("fn.name", "function", "fn"),
        // N4b/#614: `require_relative` and `require` are split into two
        // patterns/captures so the emitted signature records which keyword
        // produced the import. `require_relative` is importer-relative
        // (`import:<spec>`); `require` is load-path (gem/stdlib/in-repo lib),
        // tagged `import:gem:<spec>` so RubyResolver can tell them apart
        // (#614) instead of both collapsing to `import:<spec>`. Both keep
        // node kind `"import"`, so `EdgeKind::Depends` is unchanged. The
        // `#eq?` predicates are auto-applied by tree-sitter's match iterator,
        // so `@require.kw`/`@require_relative.kw` (no capture_kinds entry)
        // are ignored and any other call (e.g. `puts`) never matches either
        // pattern.
        ("import", "import", "import"),
        ("import.gem", "import", "import:gem"),
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
        // N4b/#614: `require` and `require_relative` produce distinct import
        // signatures, file->import `Depends` edges (Ruby previously emitted
        // zero, #582), and both stay `kind == "import"`. A non-require call
        // (`puts`) must NOT produce an import, the `#eq?` predicates filter
        // the match on each pattern.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("boot.rb");
        std::fs::write(
            &path,
            "require 'set'\nrequire_relative './foo'\nrequire_relative 'bar'\nputs 'hi'\n",
        )
        .unwrap();
        let out = parse("corp", &path, "boot.rb").unwrap();

        let imports: Vec<&travsr_core::Node> =
            out.nodes.iter().filter(|n| n.kind == "import").collect();
        let import_sigs: Vec<&str> = imports.iter().map(|n| n.vname.signature.as_str()).collect();
        assert!(
            import_sigs.contains(&"import:gem:set"),
            "got {import_sigs:?}"
        );
        assert!(import_sigs.contains(&"import:./foo"), "got {import_sigs:?}");
        assert!(import_sigs.contains(&"import:bar"), "got {import_sigs:?}");
        assert_eq!(
            import_sigs.len(),
            3,
            "no import for `puts`: {import_sigs:?}"
        );
        assert!(
            imports.iter().all(|n| n.kind == "import"),
            "both require forms stay kind=import: {import_sigs:?}"
        );

        let depends = out
            .edges
            .iter()
            .filter(|e| e.kind == travsr_core::EdgeKind::Depends)
            .count();
        assert_eq!(depends, 3, "one Depends edge per require");
    }

    #[test]
    fn gem_import_has_no_end_line() {
        // Mutation-test guard: generic.rs's `is_import_prefix` helper governs
        // both the signature-cleanup match arm AND the end_line guard for
        // *every* import-scheme prefix, not just the bare "import" one. The
        // n4b test above only checks emitted signatures, so a regression
        // that scopes the end_line guard back to `sig_prefix != "import"`
        // (leaving `import:gem:*` nodes to fall through and pick up an
        // end_line) would go undetected. Import nodes are single-line
        // requires, not spans, so `import:gem:*` must stay end_line-free
        // exactly like the bare `import:*` (require_relative) form.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("boot.rb");
        std::fs::write(&path, "require 'set'\nrequire_relative './foo'\n").unwrap();
        let out = parse("corp", &path, "boot.rb").unwrap();
        let imports: Vec<&travsr_core::Node> =
            out.nodes.iter().filter(|n| n.kind == "import").collect();
        assert_eq!(imports.len(), 2, "got {imports:?}");
        for n in &imports {
            assert_eq!(
                n.end_line, None,
                "import node {:?} must not carry an end_line",
                n.vname.signature
            );
        }
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
