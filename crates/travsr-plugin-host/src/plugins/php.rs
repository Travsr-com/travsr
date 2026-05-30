use travsr_core::Language;
use super::generic::LanguageConfig;

pub const CONFIG: LanguageConfig = LanguageConfig {
    language: Language::Php,
    extensions: &["php", "phtml", "php8"],
    queries: r#"
(class_declaration name: (name) @class.name)
(interface_declaration name: (name) @interface.name)
(trait_declaration name: (name) @class.name)
(enum_declaration name: (name) @enum.name)
(function_definition name: (name) @fn.name)
(method_declaration name: (name) @fn.name)
(namespace_use_clause) @import
"#,
    capture_kinds: &[
        ("class.name",     "class",     "class"),
        ("interface.name", "interface", "interface"),
        ("enum.name",      "enum",      "enum"),
        ("fn.name",        "function",  "fn"),
        ("import",         "import",    "import"),
    ],
};
