use travsr_core::{Language, LanguageId};

/// Validate a wire language string into an open [`LanguageId`]
/// (RFC-013 D-1: validate format, do NOT reject unknowns).
///
/// This is the registration gate: any well-formed id is a valid identity.
/// Mapping to a builtin [`Language`] is a secondary, non-gating step — use
/// [`language_from_proto_str`] (or `LanguageId::as_builtin`) for that.
pub fn language_id_from_proto_str(s: &str) -> Option<LanguageId> {
    // Accept the historical "objc" alias by canonicalising it first.
    let canonical = if s == "objc" { "objectivec" } else { s };
    LanguageId::new(canonical)
}

/// Map a canonical proto language string to a builtin Language variant.
/// Case-sensitive. Returns None for unrecognised values — which, post
/// RFC-013 D-1, is NOT a registration error; it merely means the language
/// has no builtin fast-path.
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
