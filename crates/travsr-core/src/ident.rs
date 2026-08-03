//! Identifier segmentation — the single source of truth for "what words is
//! this identifier made of". Used by the FTS tokenizer (index population, via
//! `travsr_store::fts_tokenize::tokenize_identifier`) and by the retrieval
//! boundary predicate (anchor admission, lexical boundary evidence, via
//! `contains_token`). Before this module existed, these two questions were
//! answered by two different segmenters that disagreed (#478 RFC-023
//! Evidence C/D) — camelCase-aware `tokenize_identifier` built the index, while
//! a punctuation-only boundary check gated anchoring, rejecting genuine
//! anchors like `"sqlite"` in `fn:SqliteStore.exec_ddl`. Keeping both on one
//! segmenter is the point.

/// Split `s` into lowercased word segments.
///
/// Delimiters: every non-ASCII-alphanumeric character. Also splits on
/// lower→upper transitions (`getCallers` → `get`, `callers`) and on
/// letter↔digit transitions (`utf8Decode` → `utf`, `8`, `decode`). Adjacent
/// duplicate segments are suppressed as they are produced (matches the
/// pre-extraction `tokenize_identifier` behaviour exactly).
///
/// KNOWN LIMITATION (deliberate, see RFC-023 §13): consecutive-uppercase
/// acronym runs are not split — `HTTPServer` → `["httpserver"]`. Changing
/// this changes `nodes_fts`/`nodes_fts_words` content and forces a reindex.
///
/// O(n) in the length of `s`, single pass.
pub fn segments(s: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();

    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();

    for i in 0..len {
        let c = chars[i];

        if !c.is_ascii_alphanumeric() {
            flush(&mut current, &mut tokens);
            continue;
        }

        if c.is_ascii_uppercase() {
            if i > 0 && (chars[i - 1].is_ascii_lowercase() || chars[i - 1].is_ascii_digit()) {
                flush(&mut current, &mut tokens);
            }
            current.push(c.to_ascii_lowercase());
        } else if c.is_ascii_digit() {
            if i > 0 && chars[i - 1].is_ascii_alphabetic() {
                flush(&mut current, &mut tokens);
            }
            current.push(c);
        } else {
            if i > 0 && chars[i - 1].is_ascii_digit() {
                flush(&mut current, &mut tokens);
            }
            current.push(c);
        }
    }
    flush(&mut current, &mut tokens);

    tokens
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

/// True when `token` appears in `text` as a whole word segment, or as a
/// contiguous run of segments joined (`"sqlitestore"` matches
/// `["sqlite","store"]`).
///
/// `token` is normalised the same way as `text` before comparison — every
/// non-ASCII-alphanumeric byte is stripped and the result lowercased — so a
/// token that carries its own delimiters (a raw content token like
/// `"seed_target"`, not yet segmented by the caller) still matches a
/// contiguous run of the text's segments (`"seedtarget"` == `"seed"+"target"`).
/// This is the replacement for `travsr_mcp::seed::token_is_sig_component`,
/// which required non-alphanumeric boundaries and so rejected camelCase/
/// PascalCase anchors.
pub fn contains_token(token: &str, text: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    let token_norm: String = token
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if token_norm.is_empty() {
        return false;
    }
    let segs = segments(text);

    for start in 0..segs.len() {
        let mut acc = String::new();
        for seg in &segs[start..] {
            acc.push_str(seg);
            if acc == token_norm {
                return true;
            }
            if acc.len() > token_norm.len() {
                break;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segs(s: &str) -> Vec<String> {
        segments(s)
    }

    #[test]
    fn snake_case() {
        assert_eq!(segs("dispatch_tool_call"), vec!["dispatch", "tool", "call"]);
    }

    #[test]
    fn camel_case() {
        assert_eq!(segs("getCallersGlobal"), vec!["get", "callers", "global"]);
    }

    #[test]
    fn pascal_case() {
        assert_eq!(segs("SqliteStore"), vec!["sqlite", "store"]);
    }

    #[test]
    fn screaming_snake() {
        assert_eq!(segs("MAX_CONTEXT_BUDGET"), vec!["max", "context", "budget"]);
    }

    #[test]
    fn utf8_decode_letter_digit_boundary() {
        assert_eq!(segs("utf8Decode"), vec!["utf", "8", "decode"]);
    }

    #[test]
    fn parse2_url_digit_boundary() {
        assert_eq!(segs("parse2Url"), vec!["parse", "2", "url"]);
    }

    #[test]
    fn dotted_signature() {
        assert_eq!(
            segs("fn:SqliteStore.exec_ddl"),
            vec!["fn", "sqlite", "store", "exec", "ddl"]
        );
    }

    #[test]
    fn path_with_extension() {
        assert_eq!(
            segs("packages/travsr-lsif-ts/src/walker.ts"),
            vec!["packages", "travsr", "lsif", "ts", "src", "walker"]
        );
    }

    #[test]
    fn empty_string() {
        assert_eq!(segs(""), Vec::<String>::new());
    }

    #[test]
    fn single_char() {
        assert_eq!(segs("x"), vec!["x"]);
    }

    #[test]
    fn all_punctuation() {
        assert_eq!(segs("::--__"), Vec::<String>::new());
    }

    #[test]
    fn token_longer_than_text() {
        assert!(!contains_token("verylongtoken", "fn:walk"));
    }

    #[test]
    fn non_ascii_treated_as_delimiter() {
        // Non-ASCII alphanumeric (e.g. accented letters) are not
        // ascii_alphanumeric, so they act as delimiters like punctuation.
        assert_eq!(segs("café_bar"), vec!["caf", "bar"]);
    }

    // ── contains_token: the #478 behaviour-change table ──────────────────────

    #[test]
    fn works_does_not_match_workspace() {
        assert!(!contains_token("works", "provideWorkspaceChatContext"));
    }

    #[test]
    fn auth_does_not_match_authentication() {
        assert!(!contains_token("auth", "fn:authentication"));
    }

    #[test]
    fn wal_does_not_match_walk() {
        assert!(!contains_token("wal", "fn:walk"));
    }

    #[test]
    fn ppr_matches_ppr_inner() {
        assert!(contains_token("ppr", "fn:ppr_inner"));
    }

    #[test]
    fn sqlite_matches_sqlite_store_method() {
        assert!(contains_token("sqlite", "fn:SqliteStore.exec_ddl"));
    }

    #[test]
    fn store_matches_sqlite_store_method() {
        assert!(contains_token("store", "fn:SqliteStore.exec_ddl"));
    }

    #[test]
    fn sqlitestore_matches_via_contiguous_run() {
        assert!(contains_token("sqlitestore", "fn:SqliteStore.exec_ddl"));
    }

    #[test]
    fn token_with_its_own_delimiters_matches_contiguous_run() {
        // A raw content token that still carries its own separator (not yet
        // segmented by the caller) must match the same way a fused token
        // does — the token's own delimiters are stripped before comparing.
        assert!(contains_token("seed_target", "fn:seed_target"));
        assert!(contains_token("Seed_Target", "fn:seed_target"));
    }

    #[test]
    fn http_does_not_match_httpserver_acronym_limitation() {
        // Deliberate known limitation (RFC-023 §13): acronym runs are not
        // split, so "http" is not a segment of "httpserver". Pinned here so
        // the acronym-segmentation follow-up has a failing-by-design anchor
        // to flip.
        assert!(!contains_token("http", "HTTPServer"));
    }
}
