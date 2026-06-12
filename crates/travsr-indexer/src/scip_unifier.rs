//! G1 — SCIP symbol name/kind extraction helpers for the unification pass.
//!
//! This module is **pure** (no I/O, no DB).  The actual DB writes live in
//! `travsr-daemon::scip_unifier` which bridges indexer output with the store.
//!
//! RFC-014 §G1. The descriptor suffix grammar is defined by the SCIP spec and
//! shared by every SCIP indexer (scip-go, scip-typescript, scip-python,
//! rust-analyzer, scip-java, scip-clang, ...), so one parser covers all
//! languages:
//!   `(<disambiguator>).` → method/function    (kind `"function"`)
//!   `#`                  → type/class/trait   (kind `"class"`)
//!   `.`                  → field/variable     (kind `"variable"`)

/// Parsed identity of a SCIP symbol's leaf descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScipName<'a> {
    /// Enclosing type for methods/fields (`Server#Serve().` → `Server`).
    pub container: Option<&'a str>,
    /// Bare identifier (`Serve`).
    pub name: &'a str,
    /// RFC-014 kind class: `"function"`, `"class"`, or `"variable"`.
    pub kind: &'a str,
}

/// Extract `(container, name, kind)` from any SCIP symbol string.
///
/// SCIP symbol format: `<scheme> <mgr> <pkg> <version> <descriptor-chain>`
/// where the last `/`-separated descriptor encodes the identifier + a suffix.
/// Method descriptors may carry an overload disambiguator between the parens
/// (`add(+1).`, scip-clang hash disambiguators) — accepted and stripped.
///
/// Returns `None` for non-SCIP signatures (no descriptor suffix), package
/// descriptors, parameter descriptors, and macro/meta descriptors — callers
/// can feed every Phase B node through without pre-filtering by language.
pub fn scip_name_kind(symbol: &str) -> Option<ScipName<'_>> {
    let descriptor_chain = symbol.split_whitespace().last()?;
    let leaf = descriptor_chain
        .rsplit('/')
        .next()
        .unwrap_or(descriptor_chain);

    if leaf.ends_with(").") {
        // Method/function: `Name().`, `Type#Name().`, `Name(+1).`
        let open = leaf.rfind('(')?;
        let (container, name) = split_container(&leaf[..open]);
        if name.is_empty() {
            return None;
        }
        Some(ScipName {
            container,
            name,
            kind: "function",
        })
    } else if let Some(stripped) = leaf.strip_suffix('#') {
        let (container, name) = split_container(stripped);
        if name.is_empty() {
            return None;
        }
        Some(ScipName {
            container,
            name,
            kind: "class",
        })
    } else if let Some(stripped) = leaf.strip_suffix('.') {
        // Single `.` suffix — field/var. The `().` case is handled above.
        let (container, name) = split_container(stripped);
        if name.is_empty() || name.contains('/') || name.contains(':') {
            return None;
        }
        Some(ScipName {
            container,
            name,
            kind: "variable",
        })
    } else {
        None
    }
}

/// Split `Type#name` into `(Some("Type"), "name")`; bare names pass through.
/// Nested types (`Outer#Inner#name`) keep only the innermost container, which
/// matches the single-level `Container.name` qualification Phase A emits.
fn split_container(s: &str) -> (Option<&str>, &str) {
    match s.rsplit_once('#') {
        Some((pre, name)) => {
            let container = pre.rsplit('#').next().unwrap_or(pre);
            ((!container.is_empty()).then_some(container), name)
        }
        None => (None, s),
    }
}

/// Phase A signature candidates for a parsed SCIP name, ordered most→least
/// specific.  Covers every Phase A parser convention in one list:
///   functions: `method:C.n` (Go/Python/TS), `fn:C.n` (Rust), `fn:n` (all,
///              and the only form for Java/Kotlin/Scala/C/C++/C#/Ruby/PHP/
///              Swift/Dart, whose parsers emit unqualified method names)
///   types:     `class:` `struct:` `interface:` `trait:` `enum:` `type:`
///              (Kotlin objects, Scala traits, Dart mixins, Swift protocols
///              all emit sig prefix `class:`, so no extra prefixes needed)
///   terms:     `var:` `const:` `static:`
pub fn candidate_signatures(parsed: &ScipName<'_>) -> Vec<String> {
    let name = parsed.name;
    match parsed.kind {
        "function" => {
            let mut sigs = Vec::with_capacity(3);
            if let Some(c) = parsed.container {
                sigs.push(format!("method:{c}.{name}"));
                sigs.push(format!("fn:{c}.{name}"));
            }
            sigs.push(format!("fn:{name}"));
            sigs
        }
        "class" => ["class", "struct", "interface", "trait", "enum", "type"]
            .iter()
            .map(|p| format!("{p}:{name}"))
            .collect(),
        "variable" => ["var", "const", "static"]
            .iter()
            .map(|p| format!("{p}:{name}"))
            .collect(),
        _ => Vec::new(),
    }
}

/// Extract the raw SCIP symbol string from a node's VName signature.
///
/// scip-reader packs signatures as `"scip:{rel_path}:{symbol}"`.
/// `ingest_scip_g2` stores just the raw symbol.  Both cases are handled.
pub fn scip_symbol_from_sig(sig: &str) -> &str {
    if let Some(rest) = sig.strip_prefix("scip:") {
        if let Some(pos) = rest.find(':') {
            return &rest[pos + 1..];
        }
    }
    sig
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed<'a>(container: Option<&'a str>, name: &'a str, kind: &'a str) -> ScipName<'a> {
        ScipName {
            container,
            name,
            kind,
        }
    }

    #[test]
    fn go_function() {
        assert_eq!(
            scip_name_kind("scip-go go github.com/org/repo v1.0.0 pkg/HandleRequest()."),
            Some(parsed(None, "HandleRequest", "function"))
        );
    }

    #[test]
    fn go_method_has_container() {
        assert_eq!(
            scip_name_kind("scip-go go github.com/org/repo v1.0.0 pkg/Server#Serve()."),
            Some(parsed(Some("Server"), "Serve", "function"))
        );
    }

    #[test]
    fn go_type() {
        assert_eq!(
            scip_name_kind("scip-go go github.com/org/repo v1.0.0 pkg/Server#"),
            Some(parsed(None, "Server", "class"))
        );
    }

    #[test]
    fn go_variable() {
        assert_eq!(
            scip_name_kind("scip-go go github.com/org/repo v1.0.0 pkg/ErrNotFound."),
            Some(parsed(None, "ErrNotFound", "variable"))
        );
    }

    #[test]
    fn java_overload_disambiguator() {
        assert_eq!(
            scip_name_kind("semanticdb maven jdk 11 java/util/List#add(+1)."),
            Some(parsed(Some("List"), "add", "function"))
        );
    }

    #[test]
    fn rust_analyzer_method() {
        assert_eq!(
            scip_name_kind("rust-analyzer cargo serde 1.0.0 ser/Serializer#serialize()."),
            Some(parsed(Some("Serializer"), "serialize", "function"))
        );
    }

    #[test]
    fn python_class_method() {
        assert_eq!(
            scip_name_kind("scip-python python pkg 1.0 mod/Animal#speak()."),
            Some(parsed(Some("Animal"), "speak", "function"))
        );
    }

    #[test]
    fn nested_type_keeps_innermost_container() {
        assert_eq!(
            scip_name_kind("semanticdb maven . . pkg/Outer#Inner#run()."),
            Some(parsed(Some("Inner"), "run", "function"))
        );
    }

    #[test]
    fn class_field_has_container() {
        assert_eq!(
            scip_name_kind("scip-python python pkg 1.0 mod/Animal#name."),
            Some(parsed(Some("Animal"), "name", "variable"))
        );
    }

    #[test]
    fn no_suffix_is_none() {
        assert_eq!(scip_name_kind("scip-go go pkg v1.0.0 something"), None);
    }

    #[test]
    fn native_phase_a_sig_is_none() {
        // Non-SCIP signatures from builtin Phase B plugins must fall through.
        assert_eq!(scip_name_kind("fn:Type.method"), None);
        assert_eq!(scip_name_kind("class:Foo"), None);
    }

    #[test]
    fn candidates_function_with_container() {
        let sigs = candidate_signatures(&parsed(Some("Server"), "Serve", "function"));
        assert_eq!(sigs, vec!["method:Server.Serve", "fn:Server.Serve", "fn:Serve"]);
    }

    #[test]
    fn candidates_function_bare() {
        let sigs = candidate_signatures(&parsed(None, "HandleRequest", "function"));
        assert_eq!(sigs, vec!["fn:HandleRequest"]);
    }

    #[test]
    fn candidates_class_covers_all_type_prefixes() {
        let sigs = candidate_signatures(&parsed(None, "Server", "class"));
        assert_eq!(
            sigs,
            vec![
                "class:Server",
                "struct:Server",
                "interface:Server",
                "trait:Server",
                "enum:Server",
                "type:Server"
            ]
        );
    }

    #[test]
    fn candidates_variable() {
        let sigs = candidate_signatures(&parsed(None, "MAX_LEN", "variable"));
        assert_eq!(sigs, vec!["var:MAX_LEN", "const:MAX_LEN", "static:MAX_LEN"]);
    }

    #[test]
    fn scip_symbol_from_scip_reader_sig() {
        let sig = "scip:pkg/foo.go:scip-go go example.com v1.0.0 pkg/Foo#";
        assert_eq!(
            scip_symbol_from_sig(sig),
            "scip-go go example.com v1.0.0 pkg/Foo#"
        );
    }

    #[test]
    fn scip_symbol_from_raw_sig() {
        let sig = "scip-go go example.com v1.0.0 pkg/Bar().";
        assert_eq!(scip_symbol_from_sig(sig), sig);
    }
}
