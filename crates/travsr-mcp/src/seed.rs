/// Tier-0 seed quality: model-free seed selection, coverage, confidence, and abstention.
///
/// Pipeline (FTS-only path — zero embeddings required):
///   tokenize_query → per-token anchor resolution (IDF-weighted) → whole-query lexical FTS
///   → RRF fusion → coverage + confidence classification → optional abstention
///
/// KNN (Tier 1) slots into the same rrf_fuse call when the embed sidecar is present.
use std::collections::HashMap;

use travsr_core::{Node as CoreNode, NodeId};
use travsr_retrieval::EdgeFilter;
use travsr_store::SqliteStore;

use crate::tools::{is_noise_seed, kind_boost};

// ── Tunable constants (all env-overridable) ──────────────────────────────────

/// Maximum symbol-frequency for an anchor to count as "rare/trusted" (near-IDF weight 1.0).
fn rare_anchor_max() -> usize {
    std::env::var("TRAVSR_RARE_ANCHOR_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3)
}

/// Coverage threshold for Exact / Strong confidence.
fn coverage_strong() -> f32 {
    std::env::var("TRAVSR_COVERAGE_STRONG")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.6)
}

/// Coverage threshold for Weak confidence.
fn coverage_weak() -> f32 {
    std::env::var("TRAVSR_COVERAGE_WEAK")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.25)
}

/// Raw (positive) BM25 score floor for Strong confidence.
/// SQLite FTS5 `-bm25()` on this corpus: ~0.1 (weak) to ~3+ (strong). Default 0.5.
fn bm25_strong_floor() -> f32 {
    std::env::var("TRAVSR_BM25_STRONG_FLOOR")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&x: &f32| x > 0.0)
        .unwrap_or(0.5)
}

/// Maximum guesses to include when confidence == None (abstention path).
pub(crate) fn abstain_max_guesses() -> usize {
    std::env::var("TRAVSR_ABSTAIN_GUESSES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3)
}

/// Minimum IDF weight for a resolved token to count toward coverage and the anchor escape hatch.
///
/// Tokens below this threshold (e.g. "map", "get", "list", "run") match too many
/// nodes to be meaningful query signals, so they don't count as "covered" even when
/// they resolve.  Default 0.55 ≈ symbol appears in ≤ ~52 nodes out of 7 k (~0.75%).
/// For a corpus of n nodes, freq_max ≈ n / e^(threshold * ln(n)).
fn idf_coverage_min() -> f32 {
    std::env::var("TRAVSR_IDF_COVERAGE_MIN")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&x: &f32| x > 0.0 && x <= 1.0)
        .unwrap_or(0.55)
}

/// RRF k constant — controls how sharply the top ranks dominate.
fn rrf_k() -> f32 {
    std::env::var("TRAVSR_RRF_K")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&x: &f32| x > 0.0)
        .unwrap_or(60.0)
}

// ── Core types ────────────────────────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueryIntent {
    Lookup,
    Callers,
    Deps,
    Explain,
    Relation,
    /// Query with no named symbol — all tokens are NL words.
    Conceptual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SeedSource {
    Exact,
    Lexical,
    Knn,
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedTerm {
    pub token: String,
    /// True if ≥1 exact anchor hit was found.
    pub resolved: bool,
    /// Number of graph symbols whose signature contains this token.
    pub symbol_freq: usize,
    /// IDF weight of this token: high = rare/specific, low = generic ("map", "get").
    pub idf_w: f32,
    /// The top-ranked node ID for this token (if resolved). Reserved for Tier-0.5 reranking.
    #[allow(dead_code)]
    pub top_node: Option<NodeId>,
}

#[derive(Debug, Clone)]
pub(crate) struct Seed {
    pub node: NodeId,
    /// PPR personalisation weight derived from IDF × kind_boost or BM25 × kind_boost.
    pub weight: f32,
    /// Which retrieval source contributed this seed. Reserved for response annotation.
    #[allow(dead_code)]
    pub source: SeedSource,
    /// Raw score before normalisation. Reserved for response annotation.
    #[allow(dead_code)]
    pub score: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Confidence {
    Exact,
    Strong,
    Weak,
    None,
}

impl Confidence {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Strong => "strong",
            Self::Weak => "weak",
            Self::None => "none",
        }
    }
}

#[derive(Debug)]
pub(crate) struct SeedSet {
    /// Classified intent — reserved for future intent-routing.
    #[allow(dead_code)]
    pub intent: QueryIntent,
    pub seeds: Vec<Seed>,
    pub terms: Vec<ResolvedTerm>,
    /// Fraction of content tokens that resolved to at least one anchor: [0.0, 1.0].
    /// Reserved for future response annotation.
    #[allow(dead_code)]
    pub coverage: f32,
    pub confidence: Confidence,
    /// Top raw BM25 score (positive) from the lexical FTS path; 0.0 if no FTS results.
    /// Reserved for future response annotation.
    #[allow(dead_code)]
    pub top_bm25: f32,
}

impl SeedSet {
    /// Extract `(NodeId, weight)` pairs for `ppr_weighted`.
    pub(crate) fn ppr_seeds(&self) -> Vec<(NodeId, f32)> {
        self.seeds.iter().map(|s| (s.node, s.weight)).collect()
    }

    #[allow(dead_code)]
    pub(crate) fn is_empty(&self) -> bool {
        self.seeds.is_empty()
    }
}

// ── Stop-word list (code-aware: omit "get", "set", "run", "use") ─────────────

const STOP_WORDS: &[&str] = &[
    "a", "an", "the", "of", "in", "to", "is", "for", "and", "or", "with", "that", "this", "how",
    "what", "where", "when", "which", "does", "are", "by", "my", "from", "at", "it", "its", "be",
    "as", "on", "so", "all", "me", "i", "you", "we", "they", "he", "she", "was", "were", "had",
    "has", "have", "been", "do", "did", "would", "could", "should",
];

// ── Tokenizer ─────────────────────────────────────────────────────────────────

/// Extract content tokens from a query for per-token anchor resolution.
///
/// - Strips leading/trailing ASCII punctuation (preserving `_` and `.`)
/// - Lowercases each word
/// - Removes stop-words and tokens shorter than 2 characters
pub(crate) fn tokenize_query(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .filter_map(|w| {
            let stripped =
                w.trim_matches(|c: char| c.is_ascii_punctuation() && c != '_' && c != '.');
            if stripped.is_empty() {
                return None;
            }
            let lower = stripped.to_ascii_lowercase();
            if lower.len() < 2 || STOP_WORDS.contains(&lower.as_str()) {
                return None;
            }
            Some(lower)
        })
        .collect()
}

// ── Intent classifier ─────────────────────────────────────────────────────────

/// Deterministic keyword-based intent classification. No LLM involved.
pub(crate) fn classify_intent(query: &str) -> QueryIntent {
    let lower = query.to_ascii_lowercase();
    if lower.contains("calls ")
        || lower.contains("callers")
        || lower.contains("who calls")
        || lower.contains("called by")
    {
        return QueryIntent::Callers;
    }
    if lower.contains("import")
        || lower.contains("depends")
        || lower.contains("dependency")
        || lower.contains("dependencies")
    {
        return QueryIntent::Deps;
    }
    let first_word = lower.split_whitespace().next().unwrap_or("");
    if first_word == "how" || lower.contains("explain") || lower.contains("describe") {
        return QueryIntent::Explain;
    }
    if lower.contains("between") || lower.contains("relationship") || lower.contains("relation") {
        return QueryIntent::Relation;
    }
    QueryIntent::Lookup
}

// ── IDF weight ────────────────────────────────────────────────────────────────

/// IDF-inspired weight for a token that matches `freq` out of `n_total` symbols.
///
/// Returns values in [0.05, 1.0]:
/// - freq=1,   n=10000 → ~0.98  (very specific anchor, near-max weight)
/// - freq=100, n=10000 → ~0.79
/// - freq=1000,n=10000 → ~0.50
/// - freq≥n   (stop-symbol) → floor 0.05
///
/// Formula: clamp( ln((N+1)/(f+1)) / ln(N+1), 0.05, 1.0 )
pub(crate) fn idf_weight(freq: usize, n_total: usize) -> f32 {
    if n_total == 0 {
        return 0.05;
    }
    let num = (n_total + 1) as f32;
    let den = (freq + 1) as f32;
    ((num / den).ln() / num.ln()).clamp(0.05, 1.0)
}

// ── Signature component check ─────────────────────────────────────────────────

/// Returns `true` if `token` (lowercase) appears as a **standalone word-boundary
/// component** in `signature`, not merely as a substring of a longer word.
///
/// Needed to prevent NL verbs like "works" from matching identifier substrings like
/// "workspace" (which would add VS Code nodes as anchors for a Rust algorithm query).
///
/// Boundary characters: anything that is NOT ASCII alphanumeric.
///
/// Examples:
///   "ppr" in "fn:ppr_inner"           → true  (bounded by ':' and '_')
///   "works" in "provideWorkspaceChatContext" → false ("works" is inside "workspace")
///   "auth" in "fn:auth_middleware"    → true  (bounded by ':' and '_')
///   "auth" in "fn:authentication"     → false ("auth" is a prefix of a longer word)
fn token_is_sig_component(token: &str, signature: &str) -> bool {
    let sig = signature.as_bytes();
    let tok = token.as_bytes();
    let tlen = tok.len();
    if tlen == 0 || tlen > sig.len() {
        return false;
    }
    let sig_lower: Vec<u8> = sig.iter().map(|b| b.to_ascii_lowercase()).collect();
    let tok_lower: Vec<u8> = tok.iter().map(|b| b.to_ascii_lowercase()).collect();
    let slen = sig_lower.len();

    let mut i = 0;
    while i + tlen <= slen {
        if sig_lower[i..i + tlen] == tok_lower[..] {
            let left_ok = i == 0 || !sig_lower[i - 1].is_ascii_alphanumeric();
            let right_ok = i + tlen >= slen || !sig_lower[i + tlen].is_ascii_alphanumeric();
            if left_ok && right_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

// ── RRF fusion ────────────────────────────────────────────────────────────────

/// Reciprocal Rank Fusion — combines multiple ranked lists into a single score.
///
/// `score(node) = Σ_source  1 / (k + rank_in_source(node))`
///
/// - Scale-free: works regardless of score magnitude per source.
/// - Handles missing sources gracefully (no term = no contribution, not a penalty).
/// - Deterministic: ties broken by NodeId ascending.
///
/// `sources` must each be sorted descending by score (position 0 = best).
pub(crate) fn rrf_fuse(sources: &[&[(NodeId, f32)]], k: f32) -> Vec<(NodeId, f32)> {
    let mut scores: HashMap<NodeId, f32> = HashMap::new();

    for source in sources {
        for (rank, &(node_id, _score)) in source.iter().enumerate() {
            // rank 0 = best → contribution 1/(k+1); rank 1 → 1/(k+2); etc.
            *scores.entry(node_id).or_insert(0.0) += 1.0 / (k + rank as f32 + 1.0);
        }
    }

    let mut result: Vec<(NodeId, f32)> = scores.into_iter().collect();
    // Deterministic: sort desc by RRF score, break ties by NodeId ascending.
    result.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    result
}

// ── Confidence classifier ─────────────────────────────────────────────────────

fn classify_confidence(
    terms: &[ResolvedTerm],
    coverage: f32,
    top_bm25: f32,
    has_any_seeds: bool,
    #[allow(unused_variables)] has_knn_seeds: bool,
) -> Confidence {
    let rare_max = rare_anchor_max();
    let cov_strong = coverage_strong();
    let cov_weak = coverage_weak();
    let bm25_floor = bm25_strong_floor();

    // Only count anchors whose IDF weight is high enough to be meaningful signals.
    // Generic tokens like "map", "get", "list" (high frequency → low IDF) match
    // hundreds of unrelated nodes and must not fake coverage or the anchor escape.
    let has_rare_anchor = terms
        .iter()
        .any(|t| t.resolved && t.symbol_freq <= rare_max);
    let has_specific_anchor = terms
        .iter()
        .any(|t| t.resolved && t.idf_w >= idf_coverage_min());

    if has_rare_anchor && coverage >= cov_strong {
        Confidence::Exact
    } else if top_bm25 >= bm25_floor && coverage >= cov_strong {
        Confidence::Strong
    } else if has_any_seeds && (coverage >= cov_weak || has_specific_anchor) {
        // KNN seeds enhance grounded queries but never substitute for lexical evidence.
        // has_knn_seeds alone (zero coverage, no anchor) → abstain; KNN always returns K
        // neighbours regardless of semantic distance, so it cannot signal "no match".
        Confidence::Weak
    } else {
        Confidence::None
    }
}

// ── Main entry point ──────────────────────────────────────────────────────────

/// First two slash-delimited path segments, e.g. "crates/travsr-retrieval" from
/// "crates/travsr-retrieval/src/ppr.rs".  Returns `None` for flat or single-segment paths.
fn package_root(path: &str) -> Option<&str> {
    let mut slashes = 0u8;
    for (i, c) in path.char_indices() {
        if c == '/' {
            slashes += 1;
            if slashes == 2 {
                return Some(&path[..i]);
            }
        }
    }
    None
}

/// Build a `SeedSet` for `query` using per-token anchor resolution + whole-query
/// lexical FTS, fused via RRF.
///
/// When `knn_pairs` is non-empty (Tier 1 — embed sidecar contributed), those pairs
/// are included as a third source in `rrf_fuse` — but only after structural proximity
/// filtering: a KNN node is admitted only if its package root (first two path segments)
/// matches at least one anchor or lexical seed's root.  This blocks cross-domain
/// contamination (e.g. VS Code extension nodes seeded for a Rust algorithm query)
/// while still letting KNN reinforce nodes in the same crate.
///
/// Filters applied before RRF:
/// - `is_noise_seed` — excludes crates, test/bench fixtures, build artefacts
/// - RBAC via `filter`
/// - `MAX_SEEDS_PER_PATH` dedup (2 per file) to prevent all seeds clustering in one file
pub(crate) fn build_seed_set(
    store: &SqliteStore,
    query: &str,
    filter: &dyn EdgeFilter,
    knn_pairs: Vec<(CoreNode, f32)>,
) -> SeedSet {
    const MAX_SEEDS_PER_PATH: usize = 2;

    let intent = classify_intent(query);
    let content_tokens = tokenize_query(query);
    let n_content = content_tokens.len();

    // Corpus size for IDF.  Clamp to ≥ 1 000 so the formula remains meaningful in
    // small repos / in-memory test stores: idf_weight(1, 1) collapses to the 0.05 floor,
    // making every token look "generic" and causing spurious abstention in tiny corpora.
    // For real production corpora (≥ 1 000 nodes) the value is unchanged.
    let n_total = store.total_node_count().unwrap_or(10_000).max(1_000);

    // ── Per-token anchor resolution ───────────────────────────────────────────
    let mut terms: Vec<ResolvedTerm> = Vec::with_capacity(n_content);
    // anchor_pairs: (NodeId, weight) where weight = idf_weight * kind_boost
    let mut anchor_raw: Vec<(NodeId, f32)> = Vec::new();
    // Track per-path counts across anchor resolution too.
    let mut anchor_path_counts: HashMap<String, usize> = HashMap::new();

    for token in &content_tokens {
        let exact_nodes = store.search_nodes_by_name(token).unwrap_or_default();
        let freq = store.symbol_frequency(token).unwrap_or(n_total);
        let resolved = !exact_nodes.is_empty();
        let top_node = exact_nodes.first().map(|n| n.id);

        let idf_w = idf_weight(freq, n_total);

        terms.push(ResolvedTerm {
            token: token.clone(),
            resolved,
            symbol_freq: freq,
            idf_w,
            top_node,
        });

        // Only emit high-IDF tokens as anchors (suppresses generic "queue", "run" etc.)
        if idf_w < 0.15 {
            continue;
        }
        // Emit top-3 matches per token to represent the anchor fully.
        for node in exact_nodes.iter().take(3) {
            if is_noise_seed(node) {
                continue;
            }
            if !filter.allow(node.id, node.id, Some(node.vname.corpus.as_str())) {
                continue;
            }
            // Require the token to appear as a standalone identifier component in the
            // signature, not just as a substring of a longer word.  This prevents NL
            // verbs like "works" from anchoring to "provideWorkspaceChatContext" via
            // the "works" substring of "workspace".
            if !token_is_sig_component(token, &node.vname.signature) {
                continue;
            }
            let path_count = anchor_path_counts
                .entry(node.vname.path.clone())
                .or_insert(0);
            if *path_count >= MAX_SEEDS_PER_PATH {
                continue;
            }
            *path_count += 1;
            let w = idf_w * kind_boost(&node.kind, &node.vname.language);
            anchor_raw.push((node.id, w));
        }
    }

    // Count only tokens that resolve to specific-enough anchors (IDF ≥ threshold).
    // Generic tokens like "map" or "get" match hundreds of unrelated nodes and must
    // not inflate coverage — they are not evidence the query is grounded in this repo.
    let idf_min = idf_coverage_min();
    let n_resolved = terms
        .iter()
        .filter(|t| t.resolved && t.idf_w >= idf_min)
        .count();
    // When there are no content tokens (query is all stop-words or very short chars),
    // use a neutral coverage of 0.5 — confidence falls back to BM25 score and seed count.
    let coverage = if n_content == 0 {
        0.5
    } else {
        n_resolved as f32 / n_content as f32
    };

    // ── Structural scope from anchor seeds ───────────────────────────────────
    // Anchor seeds (direct symbol resolution) are the most trusted source.
    // Their package roots (first two path segments) define the structural scope:
    // any FTS or KNN result from outside this scope is cross-domain noise and is dropped.
    //
    // Example: "how ppr works" → anchors in crates/travsr-retrieval → scope = that crate.
    //          FTS trigram-matches "works" → "workspace" in packages/travsr-vscode
    //          → filtered out even though FTS returned it with a high BM25 score.
    //
    // When no anchor seeds exist (no token resolved to a specific symbol), scope is
    // unrestricted and all FTS / KNN results are admitted as the only available signal.
    let anchor_roots: std::collections::HashSet<&str> = anchor_path_counts
        .keys()
        .filter_map(|p| package_root(p))
        .collect();

    // ── Whole-query lexical FTS (scored BM25) ────────────────────────────────
    let lexical_scored = store.search_nodes_fuzzy_scored(query).unwrap_or_default();

    let top_bm25 = lexical_scored.first().map(|p| p.1).unwrap_or(0.0);
    // Normalise BM25 scores per-batch for PPR weight (max = 1.0); floor at 0.05.
    let max_bm25 = top_bm25.max(0.001);

    let mut lex_path_counts: HashMap<String, usize> = HashMap::new();
    let mut lexical_raw: Vec<(NodeId, f32)> = Vec::new();
    for (node, bm25) in &lexical_scored {
        if is_noise_seed(node) {
            continue;
        }
        if !filter.allow(node.id, node.id, Some(node.vname.corpus.as_str())) {
            continue;
        }
        // Structural scope gate: if anchors established a crate scope, restrict FTS
        // results to that scope.  Nodes with no parseable root are allowed through.
        if !anchor_roots.is_empty() {
            if let Some(root) = package_root(&node.vname.path) {
                if !anchor_roots.contains(root) {
                    continue;
                }
            }
        }
        let path_count = lex_path_counts.entry(node.vname.path.clone()).or_insert(0);
        if *path_count >= MAX_SEEDS_PER_PATH {
            continue;
        }
        *path_count += 1;
        let norm = (bm25 / max_bm25).clamp(0.05, 1.0);
        let w = norm * kind_boost(&node.kind, &node.vname.language);
        lexical_raw.push((node.id, w));
    }

    // ── KNN seeds — score-gated, not structure-gated ─────────────────────────
    // KNN's purpose is to surface semantically relevant nodes that lexical
    // search misses — including nodes in different crates or packages from the
    // FTS/anchor scope.  Applying an anchor-root scope gate here defeats that:
    // it degrades KNN to a reinforcement-only signal and blocks it from
    // correcting FTS mis-scoping (e.g. surfacing retrieval code when FTS
    // anchored to security test functions, or surfacing VS Code extension code
    // when FTS anchored to Rust crates for a "typescript extension" query).
    //
    // Quality is enforced upstream by the cosine-score threshold (≥ 0.75) in
    // embed_path_seeds — only high-confidence neighbours reach this point.
    // Nodes that appear in both KNN and anchor/FTS naturally rank higher via
    // RRF multi-source fusion, so semantically-correct nodes beat noise even
    // when noise-adjacent neighbours slip through.
    let knn_raw: Vec<(NodeId, f32)> = knn_pairs.iter().map(|(n, s)| (n.id, *s)).collect();

    // ── RRF fusion ────────────────────────────────────────────────────────────
    let k = rrf_k();
    let rrf_result = rrf_fuse(&[&anchor_raw, &lexical_raw, &knn_raw], k);

    // Convert RRF result back to Seed structs, resolving weights from the
    // anchor/lexical maps (RRF score used as final ordering, original weights for PPR).
    // Build a weight lookup: node → max(anchor_weight, lexical_weight, knn_weight).
    let anchor_map: HashMap<NodeId, f32> = anchor_raw.iter().cloned().collect();
    let lexical_map: HashMap<NodeId, f32> = lexical_raw.iter().cloned().collect();
    let knn_map: HashMap<NodeId, f32> = knn_raw.iter().cloned().collect();

    // Source classification for metadata: prefer Exact > Lexical > Knn
    let seeds: Vec<Seed> = rrf_result
        .into_iter()
        .map(|(node_id, rrf_score)| {
            let anchor_w = anchor_map.get(&node_id).copied();
            let lex_w = lexical_map.get(&node_id).copied();
            let knn_w = knn_map.get(&node_id).copied();
            // Weight fed to PPR: take the max across sources so any strong signal wins.
            let weight = anchor_w
                .unwrap_or(0.0)
                .max(lex_w.unwrap_or(0.0))
                .max(knn_w.unwrap_or(0.0))
                .max(rrf_score * 0.5); // fallback: 50% of RRF score when weight unknown

            let source = if anchor_w.is_some() {
                SeedSource::Exact
            } else if knn_w.is_some() {
                SeedSource::Knn
            } else {
                SeedSource::Lexical
            };

            Seed {
                node: node_id,
                weight,
                source,
                score: rrf_score,
            }
        })
        .collect();

    let has_knn = !knn_raw.is_empty();
    let confidence = classify_confidence(&terms, coverage, top_bm25, !seeds.is_empty(), has_knn);

    SeedSet {
        intent,
        seeds,
        terms,
        coverage,
        confidence,
        top_bm25,
    }
}

// ── Abstention message builder ────────────────────────────────────────────────

/// Build the abstention response for `Confidence::None` queries.
///
/// Returns a clear signal to the AI: no grounded match found, here's what we tried,
/// here are closest guesses (if any).
pub(crate) fn abstain_message(seed_set: &SeedSet, query: &str) -> String {
    let max_guesses = abstain_max_guesses();

    let mut resolved_lines: Vec<String> = Vec::new();
    let mut unresolved: Vec<&str> = Vec::new();
    for term in &seed_set.terms {
        if term.resolved {
            resolved_lines.push(format!(
                "  \"{}\" → {} candidates",
                term.token, term.symbol_freq
            ));
        } else {
            unresolved.push(&term.token);
        }
    }

    let mut msg = format!(
        "[note: no grounded match for this query in this repo]\n\
         query: \"{query}\"\n"
    );

    if !resolved_lines.is_empty() {
        msg.push_str("resolved:\n");
        for line in &resolved_lines {
            msg.push_str(line);
            msg.push('\n');
        }
    }
    if !unresolved.is_empty() {
        msg.push_str(&format!("not found: {}\n", unresolved.join(", ")));
    }

    if max_guesses > 0 && !seed_set.seeds.is_empty() {
        msg.push_str(&format!(
            "\n[The core of your query isn't in this repo. \
             Showing closest {} guess(es) only — treat these as speculative.]\n",
            seed_set.seeds.len().min(max_guesses)
        ));
    }

    msg
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_strips_stop_words() {
        let toks = tokenize_query("what calls the payment handler");
        assert!(!toks.contains(&"what".to_string()));
        assert!(!toks.contains(&"the".to_string()));
        assert!(toks.contains(&"calls".to_string()));
        assert!(toks.contains(&"payment".to_string()));
        assert!(toks.contains(&"handler".to_string()));
    }

    #[test]
    fn tokenize_strips_punctuation_preserves_underscores() {
        let toks = tokenize_query("search_nodes_fuzzy?");
        assert_eq!(toks, vec!["search_nodes_fuzzy"]);
    }

    #[test]
    fn tokenize_filters_short_tokens() {
        let toks = tokenize_query("a b do it");
        // "a", "b", "do" filtered; "it" filtered (stop-word)
        assert!(toks.is_empty(), "got: {toks:?}");
    }

    #[test]
    fn classify_intent_callers() {
        assert_eq!(
            classify_intent("what calls search_nodes_fuzzy"),
            QueryIntent::Callers
        );
        assert_eq!(
            classify_intent("who calls fts_seeds_weighted"),
            QueryIntent::Callers
        );
    }

    #[test]
    fn classify_intent_deps() {
        assert_eq!(
            classify_intent("what does SqliteStore import"),
            QueryIntent::Deps
        );
    }

    #[test]
    fn classify_intent_explain() {
        assert_eq!(
            classify_intent("how does PPR traversal work"),
            QueryIntent::Explain
        );
    }

    #[test]
    fn classify_intent_lookup_default() {
        assert_eq!(classify_intent("get_context"), QueryIntent::Lookup);
    }

    #[test]
    fn idf_weight_specific_token() {
        // Frequency 1 out of 10000 → near 1.0
        let w = idf_weight(1, 10_000);
        assert!(w > 0.9, "specific token should score near 1.0, got {w}");
    }

    #[test]
    fn idf_weight_common_token() {
        // Frequency 5000 out of 10000 (half of corpus) → low weight
        let w = idf_weight(5_000, 10_000);
        assert!(w < 0.3, "common token should score below 0.3, got {w}");
    }

    #[test]
    fn idf_weight_clamped_to_floor() {
        // Frequency >= n_total → clamped to 0.05
        let w = idf_weight(10_000, 10_000);
        assert!((w - 0.05).abs() < 0.01, "should be at floor, got {w}");
    }

    #[test]
    fn rrf_fuse_deterministic() {
        let a: Vec<(NodeId, f32)> = vec![(NodeId(1), 1.0), (NodeId(2), 0.8)];
        let b: Vec<(NodeId, f32)> = vec![(NodeId(2), 1.0), (NodeId(3), 0.5)];
        let result1 = rrf_fuse(&[&a, &b], 60.0);
        let result2 = rrf_fuse(&[&a, &b], 60.0);
        assert_eq!(result1, result2, "RRF must be deterministic");
    }

    #[test]
    fn rrf_fuse_tie_break_by_node_id() {
        // Two single-source lists with disjoint nodes all at rank 0 → same RRF score;
        // tie must break by NodeId ascending.
        let a: Vec<(NodeId, f32)> = vec![(NodeId(5), 1.0)];
        let b: Vec<(NodeId, f32)> = vec![(NodeId(3), 1.0)];
        let result = rrf_fuse(&[&a, &b], 60.0);
        assert_eq!(
            result[0].0,
            NodeId(3),
            "lower NodeId should come first on equal RRF score"
        );
        assert_eq!(result[1].0, NodeId(5));
    }

    #[test]
    fn rrf_fuse_shared_node_scores_higher() {
        // Node 1 appears in both sources; node 2 and 3 appear in one each.
        let a: Vec<(NodeId, f32)> = vec![(NodeId(1), 1.0), (NodeId(2), 0.8)];
        let b: Vec<(NodeId, f32)> = vec![(NodeId(1), 1.0), (NodeId(3), 0.8)];
        let result = rrf_fuse(&[&a, &b], 60.0);
        assert_eq!(
            result[0].0,
            NodeId(1),
            "node present in both sources should rank first"
        );
    }

    #[test]
    fn rrf_fuse_empty_source_ignored() {
        let a: Vec<(NodeId, f32)> = vec![(NodeId(1), 1.0)];
        let empty: Vec<(NodeId, f32)> = vec![];
        let result = rrf_fuse(&[&a, &empty], 60.0);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, NodeId(1));
    }

    #[test]
    fn confidence_none_on_empty() {
        let terms: Vec<ResolvedTerm> = vec![];
        let c = classify_confidence(&terms, 0.0, 0.0, false, false);
        assert_eq!(c, Confidence::None);
    }

    #[test]
    fn confidence_exact_on_rare_anchor_high_coverage() {
        let terms = vec![ResolvedTerm {
            token: "get_context".into(),
            resolved: true,
            symbol_freq: 1,
            idf_w: 0.98, // very rare → specific anchor
            top_node: Some(NodeId(42)),
        }];
        let c = classify_confidence(&terms, 1.0, 0.0, true, false);
        assert_eq!(c, Confidence::Exact);
    }

    #[test]
    fn confidence_strong_on_good_bm25_high_coverage() {
        let terms = vec![ResolvedTerm {
            token: "ppr".into(),
            resolved: false,
            symbol_freq: 50,
            idf_w: 0.0, // not resolved — idf_w irrelevant
            top_node: None,
        }];
        let c = classify_confidence(&terms, 0.8, 2.0, true, false);
        assert_eq!(c, Confidence::Strong);
    }

    #[test]
    fn confidence_weak_on_coverage_above_floor() {
        // Generic resolved token (low idf) + coverage ≥ 0.25 → Weak via coverage path.
        let terms = vec![ResolvedTerm {
            token: "auth".into(),
            resolved: true,
            symbol_freq: 200,
            idf_w: 0.40, // too generic to count as specific anchor
            top_node: Some(NodeId(7)),
        }];
        let c = classify_confidence(&terms, 0.3, 0.1, true, false);
        assert_eq!(c, Confidence::Weak);
    }

    #[test]
    fn confidence_weak_on_specific_anchor_low_coverage() {
        // Specific resolved token (high idf) + coverage below floor → Weak via anchor escape.
        // Validates: "google map" abstains, but "PaymentService foo bar baz" doesn't.
        let terms = vec![ResolvedTerm {
            token: "PaymentService".into(),
            resolved: true,
            symbol_freq: 2,
            idf_w: 0.93, // rare → counts as specific anchor
            top_node: Some(NodeId(9)),
        }];
        let c = classify_confidence(&terms, 0.1, 0.0, true, false);
        assert_eq!(
            c,
            Confidence::Weak,
            "specific rare anchor must trigger Weak even when coverage is below floor"
        );
    }

    #[test]
    fn confidence_none_on_generic_anchor_low_coverage() {
        // Generic resolved token (low idf) + coverage below floor → None → abstain.
        // This is the "what is google map" case: "map" resolves (freq=65, idf≈0.526)
        // but falls below idf_coverage_min (0.55), so it doesn't count as coverage.
        let terms = vec![ResolvedTerm {
            token: "map".into(),
            resolved: true,
            symbol_freq: 65,
            idf_w: 0.526, // real idf for "map" in a 6940-node corpus
            top_node: Some(NodeId(5)),
        }];
        let c = classify_confidence(&terms, 0.0, 0.1, true, false);
        assert_eq!(
            c,
            Confidence::None,
            "generic resolved token with zero coverage must abstain"
        );
    }

    #[test]
    fn confidence_none_on_knn_only_no_fts() {
        // KNN always returns K neighbours regardless of semantic distance — it cannot
        // signal "no match". Zero coverage + no anchor → abstain even when KNN seeds exist.
        let terms = vec![ResolvedTerm {
            token: "xqz_no_match".into(),
            resolved: false,
            symbol_freq: 0,
            idf_w: 0.0,
            top_node: None,
        }];
        let c = classify_confidence(&terms, 0.0, 0.0, true, true);
        assert_eq!(
            c,
            Confidence::None,
            "KNN-only (zero coverage, no anchor) must abstain"
        );
    }
}
