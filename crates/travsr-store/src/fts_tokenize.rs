/// Split an identifier or path into searchable FTS5 tokens.
///
/// Used for both **indexing** (building the `tokens` column) and
/// **query-escaping** (neutralising FTS5 syntax chars before a MATCH call).
///
/// Rules:
/// - Every non-alphanumeric ASCII character is a delimiter.
/// - CamelCase boundaries (lower→upper ASCII) are split.
/// - Digit runs are treated as their own token.
/// - Each piece is lowercased before emit.
/// - The entire lowercased input is also emitted as one extra token so that
///   exact-substring queries continue to resolve correctly via the FTS path.
/// - Tokens shorter than 3 characters are dropped (trigram FTS5 cannot index
///   them anyway — the exact-substring step in `search_nodes_fuzzy` covers the
///   short-identifier case before the FTS path is reached).
/// - Duplicates are removed; total token count is capped at 64 to bound index
///   size on pathological signatures (e.g. generated code with many generics).
///
/// The output is a space-separated string ready for direct insert into the
/// `nodes_fts.tokens` column or for use as a MATCH expression component.
///
/// # Examples
/// ```
/// # use travsr_store::fts_tokenize::tokenize_identifier;
/// assert!(tokenize_identifier("dispatch_tool_call").contains("dispatch"));
/// assert!(tokenize_identifier("dispatch_tool_call").contains("tool"));
/// assert!(tokenize_identifier("getCallersGlobal").contains("callers"));
/// assert!(tokenize_identifier("src/payment.ts").contains("payment"));
/// ```
pub fn tokenize_identifier(s: &str) -> String {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();

    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();

    for i in 0..len {
        let c = chars[i];

        if !c.is_ascii_alphanumeric() {
            // Non-alphanumeric: flush current token, treat as delimiter.
            flush(&mut current, &mut tokens);
            continue;
        }

        if c.is_ascii_uppercase() {
            // CamelCase split: flush if previous char was lowercase ASCII or a digit.
            if i > 0 && (chars[i - 1].is_ascii_lowercase() || chars[i - 1].is_ascii_digit()) {
                flush(&mut current, &mut tokens);
            }
            current.push(c.to_ascii_lowercase());
        } else if c.is_ascii_digit() {
            // Digit run: flush if the previous char was a letter.
            if i > 0 && chars[i - 1].is_ascii_alphabetic() {
                flush(&mut current, &mut tokens);
            }
            current.push(c);
        } else {
            // Lowercase letter: flush if previous char was a digit.
            if i > 0 && chars[i - 1].is_ascii_digit() {
                flush(&mut current, &mut tokens);
            }
            current.push(c);
        }
    }
    flush(&mut current, &mut tokens);

    // Emit the original lowercased input as an extra token so exact-name
    // queries can also hit via the FTS path when the LIKE step is skipped.
    // Only emit for whitespace-free inputs — multi-word NL queries produce a
    // useless original-lower token that may contain FTS5 operator characters.
    let original_lower = s.to_ascii_lowercase();
    if original_lower.len() >= 3
        && !original_lower.contains(char::is_whitespace)
        && !tokens.contains(&original_lower)
    {
        tokens.push(original_lower);
    }

    // Drop tokens < 3 chars (trigram cannot index them) and dedup.
    tokens.retain(|t| t.len() >= 3);
    tokens.dedup();

    // Cap at 64 tokens to bound index size on pathological signatures.
    tokens.truncate(64);

    tokens.join(" ")
}

/// Build the MATCH expression for `search_nodes_fuzzy` Step 2 (T0 heuristic floor).
///
/// Applies RFC-012 A1 normalisation before building the MATCH expression:
/// - Tokenises via `tokenize_identifier` (TL3: never touches the raw literal
///   used by Step 1).
/// - Strips NL intent stopwords and expands bounded synonym aliases (additive
///   only — PA C1: raw content tokens are always in the union).
/// - Passes every token through the same double-quote escaper as `build_match_expr`
///   (TL4: no raw FTS5 operators reach SQLite).
///
/// Returns `None` when the resulting union is empty (pure-punctuation input).
pub fn build_fuzzy_match_expr(query: &str) -> Option<String> {
    let raw_str = tokenize_identifier(query);
    if raw_str.is_empty() {
        return None;
    }
    let raw: Vec<String> = raw_str.split_whitespace().map(str::to_string).collect();

    let union = crate::seed_lexicon::expand_tokens(&raw);
    if union.is_empty() {
        return None;
    }

    let expr = union
        .iter()
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(" OR ");

    if expr.is_empty() {
        None
    } else {
        Some(expr)
    }
}

/// DB-backed variant of `build_fuzzy_match_expr` for the Step 2 hot path (RFC-012 A2 F1).
///
/// Identical contract to `build_fuzzy_match_expr` but calls `expand_tokens_db`
/// (fts_synonyms table) instead of the compile-time static `expand_tokens`.
/// Returns `None` when the token union is empty.
pub fn build_fuzzy_match_expr_db(
    query: &str,
    conn: &rusqlite::Connection,
) -> anyhow::Result<Option<String>> {
    let raw_str = tokenize_identifier(query);
    if raw_str.is_empty() {
        return Ok(None);
    }
    let raw: Vec<String> = raw_str.split_whitespace().map(str::to_string).collect();
    let union = crate::seed_lexicon::expand_tokens_db(conn, &raw)?;
    if union.is_empty() {
        return Ok(None);
    }
    let expr = union
        .iter()
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(" OR ");
    Ok(if expr.is_empty() { None } else { Some(expr) })
}

/// Normalize a natural-language query before embedding.
///
/// Strips only curated sentence-ending punctuation (`?!.,;:`) from the trailing
/// edge of each whitespace-token, and collapses internal whitespace so that
/// `"how daemon work?"` and `"how daemon work ?"` produce identical text and
/// therefore identical embedding vectors.
///
/// CO-C2: previous version stripped ALL non-alphanumeric chars from both ends,
/// which corrupted language names and symbols:
///   `"C++"` → `"C"`, `"c#"` → `"c"`, `"_private"` → `"private"`, `"->ptr"` → `"ptr"`.
/// Now only trailing `?!.,;:` are stripped; leading characters (including `_`, `->`)
/// are preserved so language-filter inference and symbol lookup remain correct.
///
/// Rules:
/// - Split on whitespace.
/// - For each token, strip trailing occurrences of `?`, `!`, `.`, `,`, `;`, `:`.
/// - Drop tokens that become empty after stripping (e.g. a bare `?`).
/// - Rejoin with single spaces.
///
/// # Examples
/// ```
/// # use travsr_store::fts_tokenize::normalize_nl_query;
/// assert_eq!(normalize_nl_query("how daemon work?"),  "how daemon work");
/// assert_eq!(normalize_nl_query("how daemon work ?"), "how daemon work");
/// assert_eq!(normalize_nl_query("  what is PaymentService?  "), "what is PaymentService");
/// assert_eq!(normalize_nl_query("C++"), "C++");
/// assert_eq!(normalize_nl_query("_private"), "_private");
/// ```
pub fn normalize_nl_query(query: &str) -> String {
    const TRAILING_PUNCT: &[char] = &['?', '!', '.', ',', ';', ':'];
    query
        .split_whitespace()
        .filter_map(|word| {
            let stripped = word.trim_end_matches(TRAILING_PUNCT);
            if stripped.is_empty() {
                None
            } else {
                Some(stripped)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Build the MATCH expression for `search_nodes_fuzzy`.
///
/// Tokenises `query`, double-quotes each token (neutralises all FTS5 operators),
/// and joins with " OR " for maximum recall.  Returns `None` when the tokenised
/// result is empty (empty MATCH expressions are a syntax error in SQLite FTS5).
pub fn build_match_expr(query: &str) -> Option<String> {
    let tokens = tokenize_identifier(query);
    if tokens.is_empty() {
        return None;
    }
    // Double-quote each token: `"foo"` is an FTS5 phrase literal, immune to
    // operator injection (`*`, `:`, `^`, `AND`, `OR`, `NOT`, `NEAR`, …).
    let expr = tokens
        .split_whitespace()
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(" OR ");
    if expr.is_empty() {
        None
    } else {
        Some(expr)
    }
}

#[inline]
fn flush(current: &mut String, tokens: &mut Vec<String>) {
    if !current.is_empty() {
        let tok = std::mem::take(current);
        if !tokens.contains(&tok) {
            tokens.push(tok);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(s: &str) -> Vec<String> {
        tokenize_identifier(s)
            .split_whitespace()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn snake_case() {
        let t = tokens("dispatch_tool_call");
        assert!(t.contains(&"dispatch".to_string()));
        assert!(t.contains(&"tool".to_string()));
        assert!(t.contains(&"call".to_string()));
    }

    #[test]
    fn camel_case() {
        let t = tokens("getCallersGlobal");
        assert!(t.contains(&"get".to_string()));
        assert!(t.contains(&"callers".to_string()));
        assert!(t.contains(&"global".to_string()));
    }

    #[test]
    fn screaming_snake() {
        let t = tokens("MAX_CONTEXT_BUDGET");
        assert!(t.contains(&"max".to_string()));
        assert!(t.contains(&"context".to_string()));
        assert!(t.contains(&"budget".to_string()));
    }

    #[test]
    fn kebab_and_colons() {
        let t = tokens("travsr-mcp::sse");
        assert!(t.contains(&"travsr".to_string()));
        assert!(t.contains(&"mcp".to_string()));
        assert!(t.contains(&"sse".to_string()));
    }

    #[test]
    fn path_with_extension() {
        let t = tokens("src/payment.ts");
        assert!(t.contains(&"src".to_string()));
        assert!(t.contains(&"payment".to_string()));
        // "ts" is < 3 chars and should be dropped
        assert!(!t.contains(&"ts".to_string()));
    }

    #[test]
    fn generics_stripped() {
        let t = tokens("Vec<Node>");
        assert!(t.contains(&"vec".to_string()));
        assert!(t.contains(&"node".to_string()));
    }

    #[test]
    fn digit_split() {
        let t = tokens("phase2Indexer");
        assert!(t.contains(&"phase".to_string()));
        assert!(t.contains(&"indexer".to_string()));
    }

    #[test]
    fn original_lowercased_included() {
        let t = tokens("SearchNodesGlobal");
        assert!(t.contains(&"searchnodesglobal".to_string()));
    }

    #[test]
    fn short_tokens_dropped() {
        // "ts", "id", "db" are < 3 chars — should not appear.
        let t = tokens("ts_id_db");
        assert!(!t.contains(&"ts".to_string()));
        assert!(!t.contains(&"id".to_string()));
        assert!(!t.contains(&"db".to_string()));
    }

    #[test]
    fn build_match_expr_quotes_tokens() {
        let expr = build_match_expr("mcp dispatch tool call").unwrap();
        // All tokens double-quoted
        assert!(expr.contains("\"mcp\""));
        assert!(expr.contains("\"dispatch\""));
        assert!(expr.contains(" OR "));
    }

    #[test]
    fn build_match_expr_strips_fts5_operators() {
        // Should not panic or return unquoted operators
        let expr = build_match_expr("foo* AND bar:baz NOT qux^2 NEAR(a)").unwrap();
        assert!(!expr.contains("AND"));
        assert!(!expr.contains("NOT"));
        assert!(!expr.contains("NEAR"));
        assert!(!expr.contains("*"));
        assert!(!expr.contains("^"));
    }

    #[test]
    fn build_match_expr_empty_on_all_punct() {
        // All punctuation → all tokens < 3 chars → None
        assert!(build_match_expr("* :: --").is_none());
    }

    #[test]
    fn dedup_and_cap() {
        // Repeated separators should not produce duplicate tokens.
        let t = tokens("foo__bar__foo");
        let foo_count = t.iter().filter(|x| x.as_str() == "foo").count();
        assert_eq!(
            foo_count, 1,
            "duplicate token 'foo' should appear only once"
        );
    }

    // ── normalize_nl_query ────────────────────────────────────────────────────

    #[test]
    fn normalize_trailing_question_mark_attached() {
        assert_eq!(normalize_nl_query("how daemon work?"), "how daemon work");
    }

    #[test]
    fn normalize_trailing_question_mark_detached() {
        assert_eq!(normalize_nl_query("how daemon work ?"), "how daemon work");
    }

    #[test]
    fn normalize_both_forms_are_identical() {
        assert_eq!(
            normalize_nl_query("how daemon work?"),
            normalize_nl_query("how daemon work ?"),
        );
    }

    #[test]
    fn normalize_leading_and_trailing_whitespace() {
        assert_eq!(
            normalize_nl_query("  what is PaymentService?  "),
            "what is PaymentService"
        );
    }

    #[test]
    fn normalize_bare_punctuation_token_dropped() {
        // A lone `?` surrounded by spaces disappears.
        assert_eq!(normalize_nl_query("hello ? world"), "hello world");
    }

    #[test]
    fn normalize_multiple_punctuation_types() {
        // Only trailing punctuation is stripped per word; "what!?" → "what" (! then ? stripped).
        assert_eq!(normalize_nl_query("what!? is this."), "what is this");
    }

    #[test]
    fn normalize_internal_words_untouched() {
        // Punctuation in the middle of a word (e.g. camelCase) is not stripped.
        assert_eq!(
            normalize_nl_query("find PaymentService.charge"),
            "find PaymentService.charge"
        );
    }

    // ── CO-C2 regression: preserve language/symbol chars ─────────────────────

    #[test]
    fn normalize_cpp_preserved() {
        assert_eq!(normalize_nl_query("C++"), "C++");
        assert_eq!(normalize_nl_query("C++ generics"), "C++ generics");
    }

    #[test]
    fn normalize_csharp_preserved() {
        assert_eq!(normalize_nl_query("c#"), "c#");
    }

    #[test]
    fn normalize_leading_underscore_preserved() {
        assert_eq!(normalize_nl_query("_private"), "_private");
    }

    #[test]
    fn normalize_arrow_preserved() {
        assert_eq!(normalize_nl_query("->ptr"), "->ptr");
    }

    #[test]
    fn normalize_trailing_question_only() {
        // Only trailing sentence-ending chars are removed.
        assert_eq!(normalize_nl_query("hello?"), "hello");
        assert_eq!(normalize_nl_query("hello!"), "hello");
        assert_eq!(normalize_nl_query("hello."), "hello");
    }

    #[test]
    fn normalize_empty_and_all_punct() {
        assert_eq!(normalize_nl_query(""), "");
        assert_eq!(normalize_nl_query("? ! ."), "");
    }
}
