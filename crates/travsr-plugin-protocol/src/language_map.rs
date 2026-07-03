use travsr_core::Language;

/// Map a canonical proto language string to a Language variant.
/// Case-sensitive. Returns None for unrecognised values.
pub fn language_from_proto_str(s: &str) -> Option<Language> {
    match s {
        "typescript" | "javascript" => Some(Language::TypeScript),
        "rust" => Some(Language::Rust),
        "python" => Some(Language::Python),
        "go" => Some(Language::Go),
        "java" => Some(Language::Java),
        "kotlin" => Some(Language::Kotlin),
        "ruby" => Some(Language::Ruby),
        "csharp" => Some(Language::CSharp),
        "php" => Some(Language::Php),
        "scala" => Some(Language::Scala),
        "cpp" => Some(Language::Cpp),
        "c" => Some(Language::C),
        "swift" => Some(Language::Swift),
        "dart" => Some(Language::Dart),
        "objectivec" | "objc" => Some(Language::ObjectiveC),
        "json" => Some(Language::Json),
        "yaml" => Some(Language::Yaml),
        "toml" => Some(Language::Toml),
        "xml" => Some(Language::Xml),
        _ => None,
    }
}

pub fn language_to_proto_str(lang: Language) -> &'static str {
    lang.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn known_languages_round_trip() {
        for (s, expected) in [
            ("typescript", Language::TypeScript),
            ("javascript", Language::TypeScript),
            ("rust", Language::Rust),
            ("python", Language::Python),
            ("go", Language::Go),
            ("java", Language::Java),
            ("kotlin", Language::Kotlin),
            ("scala", Language::Scala),
            ("cpp", Language::Cpp),
            ("c", Language::C),
            ("swift", Language::Swift),
            ("dart", Language::Dart),
            ("objectivec", Language::ObjectiveC),
            ("objc", Language::ObjectiveC),
            ("json", Language::Json),
            ("yaml", Language::Yaml),
            ("toml", Language::Toml),
            ("xml", Language::Xml),
        ] {
            assert_eq!(language_from_proto_str(s), Some(expected), "failed for {s}");
        }
    }
    #[test]
    fn unknown_language_returns_none() {
        assert_eq!(language_from_proto_str("TypeScript"), None); // case-sensitive
        assert_eq!(language_from_proto_str("Kotlin"), None); // case-sensitive
        assert_eq!(language_from_proto_str(""), None);
    }
}
