//! BM25 lexical reranking of symbol candidates.
//!
//! Stage 2 of the embedding-assisted retrieval pipeline:
//! KNN returns semantically matched file nodes; BM25 ranks the ~1000 contained
//! symbols lexically against the query so the best entry points surface first.
//!
//! Tokenisation delegates to `travsr_store::fts_tokenize::tokenize_identifier`
//! so that camelCase splitting, snake_case delimiters, and the ≥3-char minimum
//! are identical to what the FTS index uses for consistency.

use std::collections::HashMap;
use travsr_core::NodeId;
use travsr_store::fts_tokenize::tokenize_identifier;

/// Okapi BM25 parameters.
const K1: f32 = 1.2;
const B: f32 = 0.75;

/// Rank `symbols` by BM25 relevance to `query`, returning the top `k` node IDs
/// in descending score order.
///
/// `symbols` — `(node_id, text)` pairs; text is typically `"kind: signature"`.
/// `query`   — free-text query; tokenised with the same rules as documents.
/// `k`       — max results to return; 0 returns all ranked results.
///
/// Returns an empty vec when `symbols` is empty.
/// Returns `symbols` in original order when the query yields no tokens (all
/// input characters below the 3-char minimum).
pub fn bm25_rank_symbols(symbols: &[(NodeId, &str)], query: &str, k: usize) -> Vec<NodeId> {
    if symbols.is_empty() {
        return vec![];
    }

    let query_token_str = tokenize_identifier(query);
    let query_tokens: Vec<&str> = query_token_str.split_whitespace().collect();
    if query_tokens.is_empty() {
        // No usable query signal — return all symbols unchanged.
        return symbols.iter().map(|(id, _)| *id).collect();
    }

    // Tokenize every symbol text; owned Strings so there are no lifetime tangles.
    let doc_tokens: Vec<Vec<String>> = symbols
        .iter()
        .map(|(_, text)| {
            tokenize_identifier(text)
                .split_whitespace()
                .map(str::to_string)
                .collect()
        })
        .collect();

    let n = doc_tokens.len() as f32;
    let total_len: usize = doc_tokens.iter().map(|t| t.len()).sum();
    // Guard against all-empty docs (shouldn't happen in practice, avoids NaN).
    let avgdl = if total_len == 0 {
        1.0_f32
    } else {
        total_len as f32 / n
    };

    // Document frequency for each unique query term — computed once, reused
    // across all docs.  Using entry() so duplicate query terms pay one scan.
    let mut df: HashMap<&str, usize> = HashMap::new();
    for &qt in &query_tokens {
        df.entry(qt).or_insert_with(|| {
            doc_tokens
                .iter()
                .filter(|tokens| tokens.iter().any(|t| t == qt))
                .count()
        });
    }

    let mut scored: Vec<(NodeId, f32)> = symbols
        .iter()
        .enumerate()
        .map(|(i, (id, _))| {
            let tokens = &doc_tokens[i];
            let dl = tokens.len() as f32;

            let score: f32 = query_tokens
                .iter()
                .map(|&qt| {
                    let df_t = *df.get(qt).unwrap_or(&0);
                    if df_t == 0 {
                        return 0.0;
                    }
                    // Robertson-Sparck Jones IDF with smoothing (+1) so IDF is
                    // always positive even when every doc contains the term.
                    let idf = ((n - df_t as f32 + 0.5) / (df_t as f32 + 0.5) + 1.0).ln();
                    let tf = tokens.iter().filter(|t| t.as_str() == qt).count() as f32;
                    let denom = tf + K1 * (1.0 - B + B * dl / avgdl);
                    idf * tf * (K1 + 1.0) / denom
                })
                .sum();

            (*id, score)
        })
        .collect();

    // Stable sort so that ties between zero-score docs preserve input order.
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let take = if k == 0 {
        scored.len()
    } else {
        k.min(scored.len())
    };
    scored[..take].iter().map(|(id, _)| *id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use travsr_core::VName;

    fn id(sig: &str) -> NodeId {
        VName::new("", "", "test.rs", "rust", sig).id()
    }

    fn sym<'a>(sig: &'a str, text: &'a str) -> (NodeId, &'a str) {
        (id(sig), text)
    }

    #[test]
    fn empty_symbols_returns_empty() {
        assert!(bm25_rank_symbols(&[], "foo", 10).is_empty());
    }

    #[test]
    fn empty_query_returns_all_in_original_order() {
        let syms = [sym("a", "fn: foo"), sym("b", "fn: bar")];
        let result = bm25_rank_symbols(&syms, "* ::", 10);
        // "*" and "::" are below the 3-char min — all symbols, original order.
        assert_eq!(result, vec![id("a"), id("b")]);
    }

    #[test]
    fn exact_match_ranks_first() {
        let syms = [
            sym("a", "fn: unrelated_helper"),
            sym("b", "fn: PaymentService"),
            sym("c", "struct: PaymentService"),
        ];
        let result = bm25_rank_symbols(&syms, "PaymentService", 10);
        // b and c must both outrank a.
        let a_pos = result.iter().position(|&x| x == id("a")).unwrap();
        let b_pos = result.iter().position(|&x| x == id("b")).unwrap();
        let c_pos = result.iter().position(|&x| x == id("c")).unwrap();
        assert!(b_pos < a_pos, "exact match must outrank unrelated");
        assert!(c_pos < a_pos, "exact match must outrank unrelated");
    }

    #[test]
    fn multi_term_query_prefers_more_overlap() {
        let syms = [
            sym("a", "fn: get_payment_status"), // matches "payment" + "status" + "get"
            sym("b", "fn: list_users"),         // no query overlap
        ];
        let result = bm25_rank_symbols(&syms, "get payment status", 10);
        assert_eq!(result[0], id("a"), "more overlap must rank higher");
    }

    #[test]
    fn camel_case_query_and_doc_are_split_consistently() {
        let syms = [sym("a", "fn: processPayment"), sym("b", "fn: validateUser")];
        // "process" and "payment" are sub-tokens of processPayment.
        let result = bm25_rank_symbols(&syms, "processPayment", 10);
        assert_eq!(result[0], id("a"));
    }

    #[test]
    fn k_limits_output_length() {
        let syms = [
            sym("a", "fn: process"),
            sym("b", "fn: process_payment"),
            sym("c", "fn: process_refund"),
        ];
        let result = bm25_rank_symbols(&syms, "process", 2);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn k_zero_returns_all() {
        let syms = [
            sym("a", "fn: foo"),
            sym("b", "fn: bar"),
            sym("c", "fn: baz"),
        ];
        let result = bm25_rank_symbols(&syms, "foo", 0);
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn zero_matching_terms_preserves_stable_order() {
        // "zzz" is not in any doc — all scores are 0; stable sort keeps input order.
        let syms = [
            sym("a", "fn: foo"),
            sym("b", "fn: bar"),
            sym("c", "fn: baz"),
        ];
        let result = bm25_rank_symbols(&syms, "zzz", 0);
        assert_eq!(result, vec![id("a"), id("b"), id("c")]);
    }

    #[test]
    fn idf_down_weights_common_terms() {
        // "get" appears in both; "payment" only in b.
        // b should score higher because "payment" has higher IDF.
        let syms = [sym("a", "fn: get_user"), sym("b", "fn: get_payment")];
        let result = bm25_rank_symbols(&syms, "get payment", 10);
        assert_eq!(result[0], id("b"), "rare term 'payment' lifts b above a");
    }
}
