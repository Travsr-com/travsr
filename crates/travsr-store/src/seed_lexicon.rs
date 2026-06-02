//! WHY: deterministic, in-daemon query normalisation for RFC-012 A1 §4 T0 floor.
//! Stopword stripping and bounded synonym expansion are query-time only; the FTS
//! index is never rewritten.  The exact-substring step in `search_nodes_fuzzy`
//! is deliberately untouched — this module only enriches the FTS seed set.
//!
//! **PA C1 invariant:** `expand_tokens` is additive. Raw content tokens (i.e.
//! raw tokens minus stopwords) are ALWAYS retained in the output union; synonyms
//! only add candidates, never suppress the originals.  If stopword stripping
//! would empty the token list the raw tokens are used as the fallback so the
//! union is never empty for non-empty input.

/// NL intent words stripped from the FTS seed set only (never from Step 1).
///
/// Conservative by design: only words that are unlikely to appear as code
/// identifiers.  Words like `get`, `set`, `run`, `add`, `all`, `list` are
/// intentionally ABSENT because they appear in real identifiers.
/// 2-char tokens (`is`, `do`, `of`, `we`) are already dropped by
/// `tokenize_identifier`'s <3-char rule, so only 3+-char words are listed here.
pub(crate) static STOPWORDS: &[&str] = &[
    "about", "and", "any", "are", "can", "did", "does", "find", "for", "from", "give", "how",
    "into", "its", "our", "please", "show", "tell", "that", "the", "then", "this", "using", "was",
    "were", "what", "when", "where", "which", "who", "whose", "why", "with",
];

/// Bounded, curated synonym/alias table.  Each entry is `(term, &[aliases])`.
/// Aliases are ADDED to the union alongside the term; they never replace it.
/// All aliases are ≥3 chars (the trigram FTS5 minimum).
///
/// DEBT(travsr-#258): lexicon curation / ownership — review + versioning process
/// before Phase 5.  Keep ≤40 entries; values are reviewed, not learned.
pub(crate) static SYNONYMS: &[(&str, &[&str])] = &[
    ("auth", &["rbac", "session", "login", "token"]),
    ("config", &["settings", "options", "cfg"]),
    ("database", &["sqlite", "store", "storage"]),
    ("delete", &["remove", "retract", "drop"]),
    ("error", &["err", "failure", "panic"]),
    ("remove", &["delete", "retract"]),
    ("store", &["database", "sqlite", "storage"]),
    ("traversal", &["bfs", "ppr", "pagerank", "walk"]),
];

/// Returns `true` if `tok` is a stopword that should be excluded from synonym
/// expansion (but see PA C1 — if stripping produces an empty set the raw
/// tokens are kept as fallback).
pub(crate) fn is_stopword(tok: &str) -> bool {
    STOPWORDS.binary_search(&tok).is_ok()
}

/// Returns the synonym aliases for `term`, or an empty slice.
pub(crate) fn synonyms_for(term: &str) -> &'static [&'static str] {
    SYNONYMS
        .iter()
        .find(|(k, _)| *k == term)
        .map_or(&[], |(_, v)| v)
}

/// Build the additive token union for FTS seed selection (RFC-012 A1 T0 floor).
///
/// `raw` is the token list produced by `tokenize_identifier`.
///
/// Algorithm:
/// 1. Compute `content = raw − stopwords`.
/// 2. Seed the union with every token in `content` (PA C1: literal always in).
/// 3. For each content token, append its synonym aliases (additive only).
/// 4. Dedup while preserving first-seen order (N is bounded small, O(N²) fine).
/// 5. If `content` is empty (all raw tokens were stopwords) fall back to `raw`
///    so the union is never empty for non-empty input.
///
/// Returns an empty `Vec` only when `raw` is empty.
pub(crate) fn expand_tokens(raw: &[String]) -> Vec<String> {
    let content: Vec<&String> = raw.iter().filter(|t| !is_stopword(t.as_str())).collect();

    let mut out: Vec<String> = Vec::new();
    // O(1) dedup helper for bounded N.
    let push = |v: &mut Vec<String>, s: &str| {
        if s.len() >= 3 && !v.iter().any(|e| e == s) {
            v.push(s.to_string());
        }
    };

    for t in &content {
        push(&mut out, t.as_str()); // PA C1: literal always enters first
    }
    for t in &content {
        for alias in synonyms_for(t.as_str()) {
            push(&mut out, alias);
        }
    }

    // PA C1 safety floor: if stopwords ate everything, fall back to raw tokens.
    if out.is_empty() {
        for t in raw {
            push(&mut out, t.as_str());
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn stopword_dropped_from_content() {
        let raw = toks(&["where", "dispatch", "tool"]);
        let union = expand_tokens(&raw);
        assert!(
            !union.iter().any(|t| t == "where"),
            "stopword 'where' must not appear in union (content set only)"
        );
        assert!(union.iter().any(|t| t == "dispatch"));
        assert!(union.iter().any(|t| t == "tool"));
    }

    #[test]
    fn synonym_is_additive() {
        // "auth" → union must contain "auth" AND its aliases
        let raw = toks(&["auth"]);
        let union = expand_tokens(&raw);
        assert!(union.iter().any(|t| t == "auth"), "raw token must be kept");
        assert!(
            union.iter().any(|t| t == "rbac"),
            "synonym 'rbac' must be added"
        );
        assert!(
            union.iter().any(|t| t == "session"),
            "synonym 'session' must be added"
        );
    }

    #[test]
    fn c1_raw_literal_never_suppressed() {
        // "delete" has synonyms; it must still appear itself.
        let raw = toks(&["delete"]);
        let union = expand_tokens(&raw);
        assert!(
            union.iter().any(|t| t == "delete"),
            "PA C1: raw token 'delete' must always be in union"
        );
        assert!(union.iter().any(|t| t == "remove"));
        assert!(union.iter().any(|t| t == "retract"));
    }

    #[test]
    fn all_stopword_fallback_is_nonempty() {
        // All tokens are stopwords — fallback must return raw tokens.
        let raw = toks(&["where", "what", "the"]);
        let union = expand_tokens(&raw);
        assert!(
            !union.is_empty(),
            "fallback must return something for non-empty raw"
        );
    }

    #[test]
    fn empty_raw_returns_empty() {
        assert!(expand_tokens(&[]).is_empty());
    }

    #[test]
    fn no_duplicates_in_output() {
        // "store" and "database" are synonyms of each other — union must not dupe.
        let raw = toks(&["store", "database"]);
        let union = expand_tokens(&raw);
        let set: std::collections::HashSet<&String> = union.iter().collect();
        assert_eq!(union.len(), set.len(), "no duplicates in union");
    }
}
