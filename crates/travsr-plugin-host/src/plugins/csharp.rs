use super::generic::LanguageConfig;
use travsr_core::Language;

pub const CONFIG: LanguageConfig = LanguageConfig {
    language: Language::CSharp,
    extensions: &["cs"],
    queries: r#"
(class_declaration name: (identifier) @class.name)
(interface_declaration name: (identifier) @interface.name)
(struct_declaration name: (identifier) @struct.name)
(enum_declaration name: (identifier) @enum.name)
(record_declaration name: (identifier) @class.name)
(method_declaration name: (identifier) @fn.name)
(constructor_declaration name: (identifier) @fn.name)
(using_directive) @import
"#,
    capture_kinds: &[
        ("class.name", "class", "class"),
        ("interface.name", "interface", "interface"),
        ("struct.name", "class", "class"),
        ("enum.name", "enum", "enum"),
        ("fn.name", "function", "fn"),
        ("import", "import", "import"),
    ],
};
