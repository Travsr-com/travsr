use super::generic::LanguageConfig;
use travsr_core::Language;

pub const CONFIG: LanguageConfig = LanguageConfig {
    language: Language::Kotlin,
    extensions: &["kt", "kts"],
    queries: r#"
(class_declaration name: (identifier) @class.name)
(object_declaration name: (identifier) @object.name)
(function_declaration name: (identifier) @fn.name)
(import) @import
"#,
    capture_kinds: &[
        ("class.name", "class", "class"),
        ("object.name", "object", "class"),
        ("fn.name", "function", "fn"),
        ("import", "import", "import"),
    ],
};
