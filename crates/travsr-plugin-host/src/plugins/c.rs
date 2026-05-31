use super::generic::LanguageConfig;
use travsr_core::Language;

pub const CONFIG: LanguageConfig = LanguageConfig {
    language: Language::C,
    extensions: &["c", "h"],
    queries: r#"
(struct_specifier name: (type_identifier) @struct.name)
(union_specifier name: (type_identifier) @union.name)
(enum_specifier name: (type_identifier) @enum.name)
(type_definition declarator: (type_identifier) @typedef.name)
(function_declarator declarator: (identifier) @fn.name)
(preproc_include path: (_) @import)
"#,
    capture_kinds: &[
        ("struct.name", "struct", "struct"),
        ("union.name", "union", "struct"),
        ("enum.name", "enum", "enum"),
        ("typedef.name", "typedef", "type"),
        ("fn.name", "function", "fn"),
        ("import", "import", "import"),
    ],
};
