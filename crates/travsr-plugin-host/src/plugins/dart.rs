use super::generic::LanguageConfig;
use travsr_core::Language;

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
};
