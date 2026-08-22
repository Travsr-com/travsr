//! Phase A parser for C source files using tree-sitter.

use std::path::Path;

use travsr_core::Language;

use crate::generic::{parse_with_config, LanguageConfig};
use crate::ParseOutput;

pub const CONFIG: LanguageConfig = LanguageConfig {
    language: Language::C,
    extensions: &["c", "h"],
    queries: r#"
(struct_specifier name: (type_identifier) @struct.name)
(union_specifier name: (type_identifier) @union.name)
(enum_specifier name: (type_identifier) @enum.name)
(type_definition declarator: (type_identifier) @typedef.name)
(function_declarator declarator: (identifier) @fn.name)
(field_declaration declarator: (field_identifier) @field.name)
(preproc_def name: (identifier) @macro.name)
(preproc_function_def name: (identifier) @macro.name)
(preproc_include path: (_) @import)
"#,
    capture_kinds: &[
        ("struct.name", "struct", "struct"),
        ("union.name", "union", "struct"),
        ("enum.name", "enum", "enum"),
        ("typedef.name", "typedef", "type"),
        ("fn.name", "function", "fn"),
        // #757: named struct/union members → `field:Owner.name`, contained by
        // their aggregate. A data member's declarator is a bare `field_identifier`
        // (function-pointer members nest it under `function_declarator`, so those
        // are not captured here).
        ("field.name", "field", "field"),
        // N4e: `#define` object-like and function-like macros as first-class nodes.
        ("macro.name", "macro", "macro"),
        ("import", "import", "import"),
    ],
    // C has no methods, but its aggregates enclose fields (#757): list the
    // struct/union nodes so `field` captures owner-qualify. Both are emitted as
    // `struct:Name` (union.name uses the `struct` prefix above), so both map to
    // the `struct` container prefix. No `fn` capture is ever nested in these, so
    // this never turns a free function into a method.
    method_containers: &[
        ("struct_specifier", "struct"),
        ("union_specifier", "struct"),
    ],
    decl_kinds: &["function_definition"],
    type_refinements: &[],
    get_grammar: || tree_sitter::Language::new(tree_sitter_c::LANGUAGE),
};

/// Parse a C source file into graph nodes and edges.
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
        let path = dir.path().join("empty.c");
        std::fs::write(&path, "").unwrap();
        let out = parse("corp", &path, "empty.c").unwrap();
        assert_eq!(out.nodes.len(), 1, "file node only");
        assert!(out.edges.is_empty());
    }

    #[test]
    fn parse_struct_and_function() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.c");
        std::fs::write(&path, "struct Point { int x; int y; };\nvoid foo() {}\n").unwrap();
        let out = parse("corp", &path, "sample.c").unwrap();
        let kinds: Vec<&str> = out.nodes.iter().map(|n| n.kind.as_str()).collect();
        assert!(kinds.contains(&"file"), "file node present");
        assert!(kinds.contains(&"struct"), "struct node present");
        assert!(kinds.contains(&"function"), "function node present");
    }

    #[test]
    fn n4e_define_macros_are_nodes() {
        // N4e: object-like (`#define MAX 100`) and function-like
        // (`#define SQ(x) ((x)*(x))`) macros become `macro:` nodes.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("macros.c");
        std::fs::write(&path, "#define MAX 100\n#define SQ(x) ((x)*(x))\n").unwrap();
        let out = parse("corp", &path, "macros.c").unwrap();
        let sigs: Vec<&str> = out
            .nodes
            .iter()
            .filter(|n| n.kind == "macro")
            .map(|n| n.vname.signature.as_str())
            .collect();
        assert!(sigs.contains(&"macro:MAX"), "got {sigs:?}");
        assert!(sigs.contains(&"macro:SQ"), "got {sigs:?}");
    }
}
