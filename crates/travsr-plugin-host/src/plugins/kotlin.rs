use travsr_core::Language;
use super::generic::LanguageConfig;

pub const CONFIG: LanguageConfig = LanguageConfig {
    language: Language::Kotlin,
    extensions: &["kt", "kts"],
    queries: r#"
(class_declaration (type_identifier) @class.name)
(object_declaration (type_identifier) @object.name)
(function_declaration (simple_identifier) @fn.name)
(import_header) @import
"#,
    capture_kinds: &[
        ("class.name",  "class",    "class"),
        ("object.name", "object",   "class"),
        ("fn.name",     "function", "fn"),
        ("import",      "import",   "import"),
    ],
};
