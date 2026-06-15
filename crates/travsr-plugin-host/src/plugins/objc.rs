use super::generic::LanguageConfig;
use travsr_core::Language;

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
};
