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
    get_grammar: || tree_sitter::Language::new(tree_sitter_objc::LANGUAGE),
};

/// Parse an Objective-C source file into graph nodes and edges.
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
        assert!(kinds.contains(&"function"), "method node");
        assert!(kinds.contains(&"import"), "import node");
    }
}
