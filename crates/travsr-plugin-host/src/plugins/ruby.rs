use super::generic::LanguageConfig;
use travsr_core::Language;

pub const CONFIG: LanguageConfig = LanguageConfig {
    language: Language::Ruby,
    extensions: &["rb", "rake", "gemspec"],
    queries: r#"
(class name: (constant) @class.name)
(module name: (constant) @module.name)
(method name: (identifier) @fn.name)
(singleton_method name: (identifier) @fn.name)
"#,
    capture_kinds: &[
        ("class.name", "class", "class"),
        ("module.name", "class", "class"),
        ("fn.name", "function", "fn"),
    ],
};
