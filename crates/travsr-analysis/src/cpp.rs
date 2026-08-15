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
(preproc_def name: (identifier) @macro.name)
(preproc_function_def name: (identifier) @macro.name)
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
        // N4e: `#define` object-like and function-like macros as first-class nodes.
        ("macro.name", "macro", "macro"),
        ("import", "import", "import"),
    ],
    method_containers: &[("class_specifier", "class"), ("struct_specifier", "struct")],
    decl_kinds: &["function_definition"],
    type_refinements: &[],
    get_grammar: || tree_sitter::Language::new(tree_sitter_cpp::LANGUAGE),
};

/// Whether a `.h` header is C++ rather than C.
///
/// `.h` is genuinely ambiguous and the ecosystem leans C++: LLVM, Google style
/// and most C++ projects use `.h`, not `.hpp`. Extension alone sends all of
/// them to the C grammar, which cannot see a class, a namespace or a template,
/// so a header-declared API is invisible to the graph and its SCIP definitions
/// have nothing to unify against.
///
/// Decided on content, the way clangd and linguist do it, because nothing in
/// the filename can answer it.
///
/// The markers are ones C **cannot** contain. Deliberately excluded:
///
/// - `#ifdef __cplusplus` and `extern "C"`, which are how a *C* header
///   announces it is safe to include from C++. Treating them as C++ markers
///   would misclassify exactly the headers that took care to be portable.
/// - `//` comments and `bool`, which are C99 and later.
///
/// False negatives are the safe direction: a C++ header with none of these is
/// declaration-only and parses acceptably as C, which is today's behaviour.
pub fn header_is_cxx(source: &str) -> bool {
    // Scope resolution is impossible in C and appears in almost any C++ header
    // that declares something qualified.
    if source.contains("::") {
        return true;
    }
    const MARKERS: &[&str] = &[
        "template<",
        "template <",
        "namespace ",
        "class ",
        "public:",
        "private:",
        "protected:",
        "virtual ",
        "operator",
        "friend ",
        "constexpr ",
        "nullptr",
    ];
    MARKERS.iter().any(|m| source.contains(m))
}

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

    #[test]
    fn n4e_define_macros_are_nodes() {
        // N4e: object-like and function-like `#define` macros become `macro:` nodes.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("macros.cpp");
        std::fs::write(&path, "#define PI 3.14\n#define SQ(x) ((x)*(x))\n").unwrap();
        let out = parse("corp", &path, "macros.cpp").unwrap();
        let sigs: Vec<&str> = out
            .nodes
            .iter()
            .filter(|n| n.kind == "macro")
            .map(|n| n.vname.signature.as_str())
            .collect();
        assert!(sigs.contains(&"macro:PI"), "got {sigs:?}");
        assert!(sigs.contains(&"macro:SQ"), "got {sigs:?}");
    }
}
