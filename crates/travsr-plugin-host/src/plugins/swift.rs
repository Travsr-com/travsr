use super::generic::LanguageConfig;
use travsr_core::Language;

pub const CONFIG: LanguageConfig = LanguageConfig {
    language: Language::Swift,
    extensions: &["swift"],
    queries: r#"
(class_declaration    name: (type_identifier) @class.name)
(protocol_declaration name: (type_identifier) @protocol.name)
(function_declaration name: (simple_identifier) @fn.name)
(init_declaration)    @init
(import_declaration)  @import
"#,
    capture_kinds: &[
        ("class.name", "class", "class"),
        ("protocol.name", "protocol", "class"),
        ("fn.name", "function", "fn"),
        ("init", "init", "fn"),
        ("import", "import", "import"),
    ],
};
