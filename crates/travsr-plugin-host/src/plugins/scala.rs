use super::generic::LanguageConfig;
use travsr_core::Language;

pub const CONFIG: LanguageConfig = LanguageConfig {
    language: Language::Scala,
    extensions: &["scala", "sc"],
    queries: r#"
(class_definition name: (_) @class.name)
(object_definition name: (_) @object.name)
(trait_definition name: (_) @trait.name)
(function_definition name: (_) @fn.name)
(import_declaration) @import
"#,
    capture_kinds: &[
        ("class.name", "class", "class"),
        ("object.name", "object", "class"),
        ("trait.name", "trait", "class"),
        ("fn.name", "function", "fn"),
        ("import", "import", "import"),
    ],
};
