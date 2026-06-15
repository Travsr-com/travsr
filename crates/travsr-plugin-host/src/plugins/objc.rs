use super::generic::LanguageConfig;
use travsr_core::Language;

pub const CONFIG: LanguageConfig = LanguageConfig {
    language: Language::ObjectiveC,
    extensions: &["m", "mm"],
    // Class name: "@interface" immediately followed by the first identifier.
    // The positional anchor prevents capturing the superclass identifier,
    // which appears later under the named `superclass:` field.
    //
    // method_identifier . (identifier) anchors to the selector's leading
    // keyword (e.g. "initWithName" in "initWithName:age:"), which is the
    // stable, human-readable part of the ObjC method name.
    queries: r#"
(class_interface "@interface" . (identifier) @class.name)
(class_implementation "@implementation" . (identifier) @impl.name)
(protocol_declaration "@protocol" . (identifier) @protocol.name)
(method_definition (method_identifier . (identifier) @fn.name))
(method_declaration (method_identifier . (identifier) @fn.name))
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
