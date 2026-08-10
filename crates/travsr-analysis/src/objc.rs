//! Phase A parser for Objective-C source files using tree-sitter.
//!
//! `.mm` files (ObjC++) are accepted; the grammar parses ObjC constructs
//! correctly and silently skips C++-only syntax.

use std::path::Path;

use travsr_core::Language;

use crate::generic::{parse_with_config, LanguageConfig};
use crate::ParseOutput;

pub const CONFIG: LanguageConfig = LanguageConfig {
    language: Language::ObjectiveC,
    // `.mm` files are ObjC++ (C++ mixed with ObjC). tree-sitter-objc parses ObjC
    // constructs correctly but silently skips C++-only syntax (templates, lambdas,
    // `std::` usage). Phase A nodes will be incomplete for `.mm` files that lean
    // heavily on C++ — this is acceptable for structural indexing purposes.
    extensions: &["m", "mm"],
    // Class name: "@interface" immediately followed by the first identifier.
    // The positional anchor prevents capturing the superclass identifier.
    //
    // Method name: in tree-sitter-objc's CST, `method_selector_no_list` and
    // `keyword_selector` are NOT named nodes — the method name collapses into
    // a direct `(identifier)` child of `method_definition`/`method_declaration`.
    // We anchor immediately after `(method_type)` to get the selector's leading
    // keyword only (e.g. "setWidth" in "setWidth:(int)w height:(int)h"), skipping
    // the trailing keyword identifiers that also appear as direct children.
    queries: r#"
(class_interface "@interface" . (identifier) @class.name)
(class_implementation "@implementation" . (identifier) @impl.name)
(protocol_declaration "@protocol" . (identifier) @protocol.name)
(method_definition (method_type) . (identifier) @fn.name)
(method_declaration (method_type) . (identifier) @fn.name)
(function_definition declarator: (function_declarator declarator: (identifier) @fn.name))
(preproc_include path: (_) @import)
(module_import (identifier) @import)
"#,
    capture_kinds: &[
        ("class.name", "class", "class"),
        ("impl.name", "impl", "impl"),
        ("protocol.name", "protocol", "protocol"),
        ("fn.name", "function", "fn"),
        ("import", "import", "import"),
    ],
    method_containers: &[
        ("class_implementation", "impl"),
        ("class_interface", "class"),
        ("protocol_declaration", "protocol"),
    ],
    decl_kinds: &["function_definition"],
    type_refinements: &[],
    get_grammar: || tree_sitter::Language::new(tree_sitter_objc::LANGUAGE),
};

/// Parse an Objective-C source file into graph nodes and edges.
pub fn parse(corpus: &str, abs_path: &Path, vname_path: &str) -> anyhow::Result<ParseOutput> {
    let grammar = (CONFIG.get_grammar)();
    parse_with_config(&CONFIG, &grammar, None, corpus, abs_path, vname_path)
}

/// Bytes of a header inspected by [`header_is_objc`]. Declarations that
/// identify a header's dialect appear near the top; this bounds the read on a
/// generated header without changing the answer for a real one.
const HEADER_SNIFF_BYTES: usize = 64 * 1024;

/// Whether an ambiguous `.h` header is Objective-C, judged from its own text.
///
/// `.h` is shared by C, C++ and Objective-C, and the extension cannot say
/// which. The caller previously decided this from a single repo-wide "does any
/// `.m`/`.mm` exist" flag applied to every header at once, so one Objective-C
/// file anywhere claimed every header in the repo — including C++ headers in
/// unrelated directories.
///
/// The cost is silent symbol loss, not a broken edge: the Objective-C grammar
/// cannot parse C++ declarations, so a misfiled header yields a file node and
/// nothing else. `class Animal { public: void speak(); };` contributed no
/// `fn:speak`, and `search_symbol` / `find_references` / `get_callers` then
/// answered "not found" for a symbol that is plainly in the source, with no
/// error to explain why.
///
/// The header's own text settles it: Objective-C declarations have no C or C++
/// spelling, so finding one is conclusive. `None` means the text carries no
/// dialect marker either way (a plain C-style declarations header), leaving the
/// choice to the caller's repo-level signal — which is the previous behaviour,
/// and the right default for a repo already known to be Objective-C.
///
/// Sniffing is deliberately lexical: raw substring matching, with no comment or
/// string-literal stripping. A marker inside a comment counts, `#import` counts
/// even though clang accepts it in C and C++, and `class ` matches an
/// Objective-C `@class Foo;` forward declaration. All of those are rare, all
/// only reachable inside a repo already known to contain Objective-C, and the
/// alternative is a second parser to decide which parser to use.
///
/// `Some(false)` means "not Objective-C", which routes to the C grammar. It
/// does not mean the header is parsed as C++ — plain `.h` maps to
/// `Language::C` regardless.
pub fn header_is_objc(source: &str) -> Option<bool> {
    let head = source.get(..HEADER_SNIFF_BYTES).unwrap_or(source);

    // Objective-C wins outright when present: a header carrying `@interface`
    // is Objective-C even if it also uses C++ constructs (ObjC++ headers do).
    const OBJC: &[&str] = &[
        "@interface",
        "@protocol",
        "@implementation",
        "@property",
        "@end",
        "NS_ASSUME_NONNULL",
        "#import",
    ];
    if OBJC.iter().any(|m| head.contains(m)) {
        return Some(true);
    }

    // C++-only spellings. None of these parse as Objective-C's supersets of C,
    // so their presence rules Objective-C out.
    const CPP: &[&str] = &[
        "template<",
        "template <",
        "namespace ",
        "public:",
        "private:",
        "protected:",
        "std::",
        "class ",
    ];
    if CPP.iter().any(|m| head.contains(m)) {
        return Some(false);
    }

    None
}

#[cfg(test)]
mod header_sniff_tests {
    use super::{header_is_objc, HEADER_SNIFF_BYTES};

    #[test]
    fn objc_declarations_are_conclusive() {
        assert_eq!(header_is_objc("@interface Animal\n@end\n"), Some(true));
        assert_eq!(header_is_objc("@protocol Speaker\n@end\n"), Some(true));
        assert_eq!(
            header_is_objc("#import <Foundation/Foundation.h>\n"),
            Some(true)
        );
    }

    #[test]
    fn cpp_declarations_rule_objc_out() {
        // #610: the exact shape that was being misfiled. A C++ header in a repo
        // that happens to contain an unrelated `.m` file.
        assert_eq!(
            header_is_objc("#pragma once\nclass Animal { public: void speak(); };\n"),
            Some(false)
        );
        assert_eq!(
            header_is_objc("template <typename T> T id(T x);\n"),
            Some(false)
        );
        assert_eq!(header_is_objc("namespace zoo { void f(); }\n"), Some(false));
    }

    #[test]
    fn objc_wins_over_cpp_markers_in_an_objcpp_header() {
        // ObjC++ headers legitimately carry both; the Objective-C parser is the
        // one that can read `@interface`, so it must win.
        let src = "#import <Foundation/Foundation.h>\nclass Impl;\n@interface Wrapper\n@end\n";
        assert_eq!(header_is_objc(src), Some(true));
    }

    #[test]
    fn a_plain_c_header_is_ambiguous_and_defers_to_the_caller() {
        // No dialect marker: the caller's repo-level signal decides, which
        // preserves the pre-#610 behaviour for genuinely ambiguous headers.
        assert_eq!(
            header_is_objc("#pragma once\nint add(int a, int b);\n"),
            None
        );
        assert_eq!(header_is_objc(""), None);
    }

    #[test]
    fn a_non_char_boundary_cut_falls_back_instead_of_panicking() {
        // `get(..N)` returns None when byte 65536 lands inside a multi-byte
        // character, and the whole string is scanned instead. This covers that
        // fallback, not truncation — the marker here is still found.
        let src = format!("// {}\n@interface Late\n@end\n", "é".repeat(40_000));
        assert_eq!(header_is_objc(&src), Some(true));
    }

    #[test]
    fn a_marker_past_the_sniff_bound_is_not_seen() {
        // The truncation path proper: ASCII padding, so HEADER_SNIFF_BYTES is a
        // clean boundary and the slice really is cut. A marker beyond it is
        // invisible, which yields None and leaves the decision to the caller's
        // repo-level signal rather than to a partial read.
        let src = format!(
            "// {}\n@interface Late\n@end\n",
            "x".repeat(HEADER_SNIFF_BYTES)
        );
        assert_eq!(header_is_objc(&src), None);
        // The same marker inside the bound is found, so the difference above is
        // the bound and not the content.
        assert_eq!(header_is_objc("// x\n@interface Early\n@end\n"), Some(true));
    }

    /// The routing boolean is only half the claim. #630 was a *symbol* loss:
    /// the recovered header has to actually yield `fn:speak`.
    ///
    /// Worth pinning at parse level because plain `.h` maps to `Language::C`,
    /// not `Cpp` — only `.hpp`/`.hh`/`.hxx` map to C++ — so the symbol survives
    /// on tree-sitter-c error-recovering `void speak();` out of a C++ class
    /// body. That works today, and a grammar bump could take it away with every
    /// boolean assertion still green.
    #[test]
    fn a_recovered_cpp_header_still_yields_its_symbols() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("animal.h");
        std::fs::write(
            &path,
            "#pragma once\nclass Animal { public: void speak(); };\n",
        )
        .unwrap();

        let out = crate::c::parse("", &path, "cpp/animal.h").expect("C grammar parses the header");
        assert!(
            out.nodes.iter().any(|n| n.vname.signature == "fn:speak"),
            "the C grammar must still recover fn:speak from a C++ header: {:?}",
            out.nodes
                .iter()
                .map(|n| &n.vname.signature)
                .collect::<Vec<_>>()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.m");
        std::fs::write(&path, "").unwrap();
        let out = parse("corp", &path, "empty.m").unwrap();
        assert_eq!(out.nodes.len(), 1);
    }

    #[test]
    fn parse_interface_and_impl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.m");
        std::fs::write(
            &path,
            "#import <Foundation/Foundation.h>\n\
             @interface MyClass : NSObject\n- (void)doThing;\n@end\n\
             @implementation MyClass\n- (void)doThing {}\n@end\n",
        )
        .unwrap();
        let out = parse("corp", &path, "sample.m").unwrap();
        let kinds: Vec<&str> = out.nodes.iter().map(|n| n.kind.as_str()).collect();
        assert!(kinds.contains(&"class"), "class from @interface");
        assert!(kinds.contains(&"impl"), "impl from @implementation");
        assert!(kinds.contains(&"import"), "import node");
        // N1: the selector is qualified by its enclosing type.
        assert!(
            out.nodes
                .iter()
                .any(|n| n.kind == "method" && n.vname.signature == "method:MyClass.doThing"),
            "expected method:MyClass.doThing; got {:?}",
            out.nodes
                .iter()
                .map(|n| &n.vname.signature)
                .collect::<Vec<_>>()
        );
    }
}
