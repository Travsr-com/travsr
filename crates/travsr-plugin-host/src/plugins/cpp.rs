use super::generic::LanguageConfig;
use travsr_core::Language;

pub const CONFIG: LanguageConfig = LanguageConfig {
    language: Language::Cpp,
    extensions: &["cpp", "cc", "cxx", "hpp", "hh", "hxx"],
    queries: r#"
(class_specifier name: (type_identifier) @class.name)
(struct_specifier name: (type_identifier) @struct.name)
(union_specifier name: (type_identifier) @union.name)
(enum_specifier name: (type_identifier) @enum.name)
(namespace_definition name: (namespace_identifier) @namespace.name)
(function_declarator declarator: (identifier) @fn.name)
(function_declarator declarator: (field_identifier) @fn.name)
(preproc_include path: (_) @import)
"#,
    capture_kinds: &[
        ("class.name", "class", "class"),
        ("struct.name", "struct", "struct"),
        ("union.name", "union", "struct"),
        ("enum.name", "enum", "enum"),
        ("namespace.name", "namespace", "namespace"),
        ("fn.name", "function", "fn"),
        ("import", "import", "import"),
    ],
};
