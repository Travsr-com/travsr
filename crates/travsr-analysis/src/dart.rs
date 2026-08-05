//! Phase A parser for Dart source files using tree-sitter.
//!
//! Phase B (call-site edges via the AOT emitter) lives in `phase_b_dart`.

use std::path::Path;

use travsr_core::Language;

use crate::generic::{parse_with_config, LanguageConfig};
use crate::ParseOutput;

pub const CONFIG: LanguageConfig = LanguageConfig {
    language: Language::Dart,
    extensions: &["dart"],
    queries: r#"
(class_declaration     name: (identifier) @class.name)
(mixin_declaration     name: (identifier) @mixin.name)
(extension_declaration name: (identifier) @extension.name)
(enum_declaration      name: (identifier) @enum.name)
(type_alias . (type_identifier) @typedef.name)
(function_signature    name: (identifier) @fn.name)
(import_or_export)     @import
"#,
    capture_kinds: &[
        ("class.name", "class", "class"),
        ("mixin.name", "mixin", "class"),
        ("extension.name", "extension", "class"),
        ("enum.name", "enum", "enum"),
        ("typedef.name", "typedef", "type"),
        ("fn.name", "function", "fn"),
        ("import", "import", "import"),
    ],
    method_containers: &[
        ("class_declaration", "class"),
        ("mixin_declaration", "class"),
        ("extension_declaration", "class"),
        ("enum_declaration", "enum"),
    ],
    decl_kinds: &[],
    get_grammar: || tree_sitter::Language::new(tree_sitter_dart::LANGUAGE),
};

/// Parse a Dart source file into graph nodes and edges (Phase A structural only).
///
/// For Phase B call-site edges use [`crate::phase_b_dart::extract_native_phase_b`].
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
        let path = dir.path().join("empty.dart");
        std::fs::write(&path, "").unwrap();
        let out = parse("corp", &path, "empty.dart").unwrap();
        assert_eq!(out.nodes.len(), 1);
    }

    #[test]
    fn parse_class_and_enum() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.dart");
        std::fs::write(
            &path,
            "import 'dart:core';\nclass Foo {}\nmixin Bar {}\nenum Status { ok, fail }\n",
        )
        .unwrap();
        let out = parse("corp", &path, "sample.dart").unwrap();
        let kinds: Vec<&str> = out.nodes.iter().map(|n| n.kind.as_str()).collect();
        assert!(kinds.contains(&"class"));
        assert!(kinds.contains(&"mixin"));
        assert!(kinds.contains(&"enum"));
        assert!(kinds.contains(&"import"));
    }
}
