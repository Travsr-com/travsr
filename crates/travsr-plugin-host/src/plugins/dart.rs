use super::generic::LanguageConfig;
use travsr_core::Language;

pub const CONFIG: LanguageConfig = LanguageConfig {
    language: Language::Dart,
    extensions: &["dart"],
    queries: r#"
(class_declaration     name: (identifier) @class.name)
(mixin_declaration     name: (identifier) @mixin.name)
(extension_declaration name: (identifier) @extension.name)
(function_signature    name: (identifier) @fn.name)
(import_or_export)     @import
"#,
    capture_kinds: &[
        ("class.name",     "class",     "class"),
        ("mixin.name",     "mixin",     "class"),
        ("extension.name", "extension", "class"),
        ("fn.name",        "function",  "fn"),
        ("import",         "import",    "import"),
    ],
};
