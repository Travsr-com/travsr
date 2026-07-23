/// Tier-0 seed quality: model-free seed selection, coverage, confidence, and abstention.
///
/// Pipeline (FTS-only path — zero embeddings required):
///   tokenize_query → per-token anchor resolution (IDF-weighted) → whole-query lexical FTS
///   → RRF fusion → coverage + confidence classification → optional abstention
///
/// KNN (Tier 1) slots into the same rrf_fuse call when the embed sidecar is present.
use std::collections::HashMap;

use travsr_core::{EdgeKind, Node as CoreNode, NodeId};
use travsr_retrieval::EdgeFilter;
use travsr_store::{SqliteStore, Store};

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

/// Oracle top-cosine at/above which a confident embedding cluster *alone* (no
/// lexical anchor) is a Strong match. Sits in the measured gap between
/// answerable (oracle_top ≥ 0.77) and nonsense (≤ 0.64) queries on bge-small.
/// Override via `TRAVSR_SEMANTIC_PROMOTE_STRONG`.
fn semantic_promote_strong() -> f32 {
    std::env::var("TRAVSR_SEMANTIC_PROMOTE_STRONG")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&x: &f32| x > 0.0 && x <= 1.0)
        .unwrap_or(0.72)
}

/// Minimum over-fetched candidates within the cosine floor band required for
/// semantic promotion — guards against a single fluke neighbour grounding a
/// query. Override via `TRAVSR_SEMANTIC_PROMOTE_MIN_NEAR`.
fn semantic_promote_min_near() -> usize {
    std::env::var("TRAVSR_SEMANTIC_PROMOTE_MIN_NEAR")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3)
}

/// Minimum anchor cosine for the low-coverage `embed_confirmed_specific` rescue
/// (RFC-019). A specific anchor confirms a diluted-coverage query to Strong only
/// when it sits in the measured *answerable* band, not merely above the weak
/// `SEMANTIC_ABS_FLOOR` (0.55). Separates a genuine low-coverage query ("Load
/// mutationg manifests" → "manifests" ≈0.71) from a salad whose lone resolved word
/// is also literally in the query ("find code by semantic meaning" → "semantic"
/// ≈0.615). Override via `TRAVSR_CONFIRM_ANCHOR_FLOOR`.
fn confirm_anchor_floor() -> f32 {
    std::env::var("TRAVSR_CONFIRM_ANCHOR_FLOOR")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&x: &f32| (0.0..=1.0).contains(&x))
        .unwrap_or(0.66)
}

/// Oracle top-cosine below which the embedding is confident *nothing* in the
/// corpus is near the query. A Weak resting only on coincidental lexical
/// coverage is then vetoed to None (unless a rare exact anchor was named).
/// Default 0.55 sits above the measured nonsense leak (0.49) and far below the
/// answerable floor (0.77). Override via `TRAVSR_SEMANTIC_VETO_FLOOR`.
fn semantic_veto_floor() -> f32 {
    std::env::var("TRAVSR_SEMANTIC_VETO_FLOOR")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&x: &f32| (0.0..=1.0).contains(&x))
        .unwrap_or(0.55)
}

/// Per-(model, corpus) semantic-floor calibration.
///
/// Every absolute cosine floor in this module (`SEMANTIC_ORACLE_MIN`,
/// `SEMANTIC_ABS_FLOOR`, `semantic_veto_floor`, `confirm_anchor_floor`,
/// `semantic_promote_strong`, …) was tuned to **bge-small**'s query↔passage cosine
/// scale — its answerable queries sit ≳0.77 and its nonsense leak ≲0.64. A model
/// with a different scale (arctic-embed-256's Matryoshka-truncated vectors and
/// asymmetric query prefix land materially lower) would over- or under-abstain
/// against those fixed numbers: the k8s bench showed literal symbol queries whose
/// correct node was retrieved but demoted to "speculative guesses".
///
/// `Calibration` carries the active model's two measured anchors and maps each
/// reference-model floor into the model's own cosine space, preserving the floor's
/// *position within the answerable band*. It is derived automatically, label-free,
/// at reindex (see `travsr_plugin_host::calibrate_semantic_floors`) and stored in
/// graph.db meta. When absent/degenerate the mapping is the identity, so bge-small
/// and every un-calibrated repo behave byte-for-byte as before.
///
/// Generic by construction: any model — bundled, future-release, or a user's own
/// `embed_catalog.toml` entry — self-calibrates on its first reindex with no manual
/// numbers. The floors are authored once (below), in the reference scale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Calibration {
    /// "Confident but unrelated" cosine scale (nonsense-leak p95).
    pub lo: f32,
    /// "Query matches its answer" cosine scale (self-match p50).
    pub hi: f32,
}

impl Calibration {
    /// bge-small reference anchors the floors in this module were tuned to, in the
    /// same measurement space the auto-calibration probe uses:
    ///   • `REF_LO` = bge's "confident but unrelated" (nonsense) cosine scale — the
    ///     level the veto / abstain floors sit just above (`semantic_veto_floor`,
    ///     `SEMANTIC_ABS_FLOOR` = 0.55).
    ///   • `REF_HI` = bge's "a query matches its answer" (answerable) cosine scale —
    ///     the level the promote floor targets (`semantic_promote_strong` ≈ 0.72,
    ///     answerable band ≈ 0.77).
    /// The probe measures each model's equivalents (nonsense p95, self-match p50); a
    /// floor authored as a bge cosine then maps by the ratio of the two bands. They
    /// MUST bracket the floors so no floor extrapolates below `lo` — that is what made
    /// the veto vanish on a compressed model (the k8s arctic-256 abstain regression).
    const REF_LO: f32 = 0.55;
    const REF_HI: f32 = 0.77;

    /// Identity mapping — the reference model and un-calibrated repos.
    pub(crate) const IDENTITY: Calibration = Calibration {
        lo: Self::REF_LO,
        hi: Self::REF_HI,
    };

    /// Load `{lo, hi}` from graph.db meta (`embed_cos_lo` / `embed_cos_hi`).
    /// Falls back to [`Self::IDENTITY`] when absent, unparseable, non-finite, or
    /// degenerate (`hi <= lo`) — so a missing/corrupt calibration can only ever
    /// reproduce today's behaviour, never break it.
    pub(crate) fn load(store: &SqliteStore) -> Calibration {
        let g = |k: &str| -> Option<f32> {
            store
                .get_meta(k)
                .ok()
                .flatten()
                .and_then(|s| s.trim().parse::<f32>().ok())
                .filter(|x| x.is_finite())
        };
        match (g("embed_cos_lo"), g("embed_cos_hi")) {
            (Some(lo), Some(hi)) if hi > lo => Calibration { lo, hi },
            _ => Self::IDENTITY,
        }
    }

    /// Map a reference-model cosine *threshold* into this model's cosine space,
    /// preserving its fractional position within the `[REF_LO, REF_HI]` band.
    /// Identity when `self == IDENTITY`. Clamped to a valid cosine `[-1, 1]`.
    ///
    /// `O(1)`.
    #[inline]
    pub(crate) fn map(&self, ref_cos: f32) -> f32 {
        let frac = (ref_cos - Self::REF_LO) / (Self::REF_HI - Self::REF_LO);
        (self.lo + frac * (self.hi - self.lo)).clamp(-1.0, 1.0)
    }

    /// Scale a band-relative *width* (e.g. `SEMANTIC_REL_DELTA`) into this model's
    /// band, so a "keep within Δ of the top" rule stays proportional to the model's
    /// answerable-band width rather than an absolute cosine gap. `O(1)`.
    #[inline]
    pub(crate) fn map_delta(&self, ref_delta: f32) -> f32 {
        ref_delta * (self.hi - self.lo) / (Self::REF_HI - Self::REF_LO)
    }
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
    /// The top-ranked node ID for this token (if resolved). Ties G1 rarity to
    /// the specific term that produced the exact anchor (RFC-021 F3).
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
    /// RFC-021 Phase 2: absolute cross-encoder relevance score in `[0, 1]`,
    /// `None` when the seed wasn't in the reranked top-K or the reranker is
    /// absent/disabled/degraded for this query. Does not affect
    /// `classify_confidence` yet (Phase 3) — attach + reorder only.
    pub rerank_score: Option<f32>,
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

/// Today's cosine/coverage lattice, extracted verbatim (RFC-021 Phase 3). Used
/// as the confidence path when the reranker has no opinion for this query
/// (`rerank_score: None` — model absent, disabled, panicked, or degraded) so
/// the reranker-off surface stays byte-for-byte identical to pre-RFC-021
/// behaviour. Also the path for exact-symbol queries (G1): a literal named
/// symbol is judged deterministically here, never by the NL-trained reranker.
#[allow(clippy::too_many_arguments)]
fn classify_confidence_lexical_fallback(
    terms: &[ResolvedTerm],
    coverage: f32,
    top_bm25: f32,
    has_any_seeds: bool,
    #[allow(unused_variables)] has_knn_seeds: bool,
    // Cluster oracle = the KNN nearest-neighbour cosines ONLY. Drives the
    // cluster-level signals (oracle_top, semantic promote/veto) that were
    // calibrated on neighbour density — must NOT be polluted with anchor
    // self-cosines, which would spuriously inflate oracle_top on symbol queries.
    knn_oracle: &HashMap<NodeId, f32>,
    // RFC-019: per-anchor cosines (KNN neighbours ∪ directly-scored anchors). Used
    // only for the per-anchor `embed_agrees` / `embed_confirmed_specific` lookups.
    anchor_oracle: &HashMap<NodeId, f32>,
    scored_ids: &std::collections::HashSet<NodeId>,
    // Model-relative floor calibration. All cosine thresholds below are authored in
    // the bge-small reference scale and mapped into the active model's scale via
    // `cal`; `Calibration::IDENTITY` reproduces the original absolute floors.
    cal: &Calibration,
) -> Confidence {
    let rare_max = rare_anchor_max();
    let cov_strong = coverage_strong();
    let cov_weak = coverage_weak();
    let bm25_floor = bm25_strong_floor();
    let idf_min = idf_coverage_min();

    // Embed oracle as a confidence check: when the embedding is confident, a
    // lexical anchor only counts toward confidence if the model agrees it is near
    // the query. This stops NL query words that are rare-by-coincidence ("literal"
    // → 1 node, "matching" → 3) from claiming Exact/Strong over a semantic salad.
    // When the oracle is absent or weak we have no embedding opinion and trust
    // lexical evidence unchanged — the FTS-only path is identical to before.
    let oracle_top = knn_oracle.values().copied().fold(0.0_f32, f32::max);
    let oracle_confident = oracle_top >= cal.map(SEMANTIC_ORACLE_MIN);
    let floor = (oracle_top - cal.map_delta(SEMANTIC_REL_DELTA)).max(cal.map(SEMANTIC_ABS_FLOOR));
    // RFC-019: absence is no longer disagreement. With the direct-cosine oracle we
    // MEASURE every specific anchor; an anchor's node that is absent from the oracle
    // *after* we tried to score it (`scored_ids`) is genuinely unscoreable
    // (no stored vector / degraded) → "unknown", not "the model rejects it" → we
    // fall back to lexical evidence (agrees). Only an anchor that is present and
    // below floor counts as disagreement. When no score hook ran (`scored_ids`
    // empty — the FTS-only path), the old `unwrap_or(0.0)` semantics are preserved
    // exactly, so behaviour is byte-for-byte identical without embeddings.
    let embed_agrees = |t: &ResolvedTerm| -> bool {
        if !oracle_confident {
            return true;
        }
        match t.top_node {
            Some(n) => match anchor_oracle.get(&n).copied() {
                Some(c) => c >= floor,                   // measured → trust the model
                None if scored_ids.contains(&n) => true, // scored but unscoreable → unknown
                None => false,                           // never scored → old absent⇒disagree
            },
            None => false,
        }
    };

    // Only count anchors whose IDF weight is high enough to be meaningful signals
    // (generic tokens like "map"/"get" match hundreds of nodes), AND — when the
    // oracle is confident — that the embedding agrees are relevant. This drops
    // the rare-by-coincidence NL anchor ("literal" → c1_raw_literal) that drove
    // the false Exact, while keeping a genuinely named symbol ("build_seed_set").
    let has_rare_anchor = terms
        .iter()
        .any(|t| t.resolved && t.symbol_freq <= rare_max && embed_agrees(t));
    let has_specific_anchor = terms
        .iter()
        .any(|t| t.resolved && t.idf_w >= idf_min && embed_agrees(t));

    // Strict embedding confirmation: the oracle is confident AND actively places a
    // *specific* anchor near the query (present in the over-fetch with cos ≥ floor).
    // Unlike `embed_agrees`, this is NOT vacuously true when the oracle is cold/absent
    // — it is positive evidence used to rescue a query whose lexical coverage is
    // diluted by typos or generic filler but whose anchor the model confirms.
    //
    // RFC-019: with the direct-cosine oracle every specific anchor is now MEASURED, so
    // a single query-word that is also a symbol name ("semantic" in "find code by
    // semantic meaning") scores a real, lexical-overlap-inflated cosine (~0.615) that
    // clears the weak `floor` (bottoms out at ABS_FLOOR 0.55) and would falsely confirm
    // a salad. A LOW-coverage query is therefore only confirmed when its anchor sits in
    // the *answerable* band (`confirm_anchor_floor`, ~0.66) — which a genuine
    // low-coverage query clears ("Load mutationg manifests" → "manifests" ≈0.71) but
    // the salad word does not. High-coverage exact queries are unaffected: they pass
    // through `has_specific_anchor` (coverage ≥ cov_strong) regardless.
    let confirm_floor = floor.max(cal.map(confirm_anchor_floor()));
    let embed_confirmed_specific = oracle_confident
        && terms.iter().any(|t| {
            t.resolved
                && t.idf_w >= idf_min
                && t.top_node
                    .and_then(|n| anchor_oracle.get(&n).copied())
                    .is_some_and(|c| c >= confirm_floor)
        });

    // Fix 1+3a: semantic grounding. A confident oracle whose neighbours form a
    // coherent near cluster (≥ N candidates within the floor band) is a Strong
    // match even with NO lexical anchor. This is the conceptual-query path —
    // "how does the kubelet sync pod status", every token too common to be a
    // specific anchor on a 261 k-node corpus — that the lexical gates can't reach.
    // `oracle_top` separates answerable (≥ 0.77) from nonsense (≤ 0.64); the
    // promote threshold sits in that gap, so salads never reach Strong here.
    let oracle_present = !knn_oracle.is_empty();
    let n_oracle_near = knn_oracle.values().filter(|&&c| c >= floor).count();
    let semantic_strong = has_any_seeds
        && oracle_top >= cal.map(semantic_promote_strong())
        && n_oracle_near >= semantic_promote_min_near();

    // Fix 1c: semantic veto. Embeddings ran and are confident nothing in the
    // corpus is near the query (oracle_top below the veto floor). A Weak that
    // rests on coincidental lexical coverage ("the cat sat on the warm
    // windowsill" → 0.49) is then a false positive → abstain. A rare exact anchor
    // is exempt: a precise symbol the user literally typed is honoured even when
    // the model is weak on it.
    let semantic_veto =
        oracle_present && oracle_top < cal.map(semantic_veto_floor()) && !has_rare_anchor;

    if has_rare_anchor && coverage >= cov_strong {
        Confidence::Exact
    } else if has_specific_anchor && coverage >= cov_strong {
        // A specific (high-IDF) anchor the embedding AGREES with — or that has no
        // embedding opinion to contradict it — at high coverage is a Strong match.
        // e.g. "type Tweak" → type:Tweak (idf ~0.81, cosine ~1.0): not rare enough
        // for Exact (the symbol recurs across packages) but unmistakably the right
        // anchor. `embed_agrees` is baked into `has_specific_anchor`, so a confident
        // oracle that DISAGREES (the NL-salad case — "literal"/"semantic" matching a
        // symbol by coincidence) never reaches here and correctly stays Weak below.
        Confidence::Strong
    } else if embed_confirmed_specific && coverage >= cov_weak {
        // Embedding-confirmed specific anchor at low token-coverage. The query's other
        // tokens are typos or generic filler (e.g. "Load mutationg manifests" → coverage
        // 1/3: "mutationg" is a typo, "load" is generic), but the confident oracle places
        // the specific anchor ("manifests", cos 0.71 ≥ floor 0.643) near the query — a
        // real match, Strong not Weak. Requires a CONFIDENT oracle that AGREES, so salad
        // queries whose anchor is absent from the over-fetch (cos < floor) never reach here.
        Confidence::Strong
    } else if semantic_strong {
        // Fix 1+3a: oracle-grounded Strong (no lexical anchor required). See the
        // `semantic_strong` derivation above — gated on oracle_top in the
        // answerable band plus a coherent near cluster, so nonsense (low
        // oracle_top) and flukes (sparse cluster) never reach here.
        Confidence::Strong
    } else if !oracle_confident && top_bm25 >= bm25_floor && coverage >= cov_strong {
        // Pure-lexical Strong is only honest when there is NO embedding opinion.
        // When the oracle IS confident yet no embed-agreed *specific* anchor exists,
        // the high BM25/coverage came from NL words matching symbols by
        // coincidence — the result is embedding-driven, which is Weak-grade.
        Confidence::Strong
    } else if semantic_veto {
        // Fix 1c: embeddings are confident nothing matches — abstain rather than
        // surface coincidental lexical coverage. Reaches here only after the
        // lexical-Strong path above declined, so genuinely strong lexical evidence
        // (high coverage + BM25) still survives a weak oracle.
        Confidence::None
    } else if has_any_seeds && (coverage >= cov_weak || has_specific_anchor) {
        // KNN seeds enhance grounded queries but never substitute for lexical evidence.
        // has_knn_seeds alone (zero coverage, no anchor) → abstain; KNN always returns K
        // neighbours regardless of semantic distance, so it cannot signal "no match".
        Confidence::Weak
    } else {
        Confidence::None
    }
}

/// Absolute floor at/above which the reranker's opinion is a Strong match.
/// Measured 2026-07-17 against `bench/queries.json` (N=27) with
/// `crates/travsr-mcp/src/seed.rs::rerank_floor_sweep` — see RFC-021 §13.2 for
/// the full measurement and its caveats. Override via
/// `TRAVSR_RERANK_STRONG_FLOOR` (re-runnable per model, per the RFC).
fn rerank_strong_floor() -> f32 {
    std::env::var("TRAVSR_RERANK_STRONG_FLOOR")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&x: &f32| (0.0..=1.0).contains(&x))
        .unwrap_or(0.8)
}

/// Absolute floor at/above which the reranker's opinion is at least a Weak
/// match; below it the query abstains (`Confidence::None`) rather than
/// surfacing a confident salad. See [`rerank_strong_floor`] for provenance.
/// Override via `TRAVSR_RERANK_WEAK_FLOOR`.
fn rerank_weak_floor() -> f32 {
    std::env::var("TRAVSR_RERANK_WEAK_FLOOR")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&x: &f32| (0.0..=1.0).contains(&x))
        .unwrap_or(0.5)
}

/// G1 bypass decision (RFC-021 F2/F3): true when the user named a genuinely
/// *rare* symbol — an exact anchor exists **and** the specific term that
/// produced that anchor (`ResolvedTerm.top_node`) is one of `exact_anchor_ids`
/// **and** is rare enough (`symbol_freq <= rare_anchor_max()`) to be a real,
/// specific identifier rather than a coincidence. Tying rarity to the
/// anchor-producing term (not just any resolved term in the query) stops a
/// salad query with one incidental rare resolved term plus an exact anchor on
/// an unrelated common word from also bypassing. Single source of truth: the
/// caller (`build_seed_set`) uses this both to decide whether to run rerank
/// inference at all (unused when bypassing) and to gate
/// [`classify_confidence`].
fn compute_g1_bypass(
    terms: &[ResolvedTerm],
    exact_anchor_ids: &std::collections::HashSet<NodeId>,
) -> bool {
    let rare_max = rare_anchor_max();
    terms.iter().any(|t| {
        t.resolved
            && t.symbol_freq <= rare_max
            && t.top_node.is_some_and(|n| exact_anchor_ids.contains(&n))
    })
}

/// RFC-021 F4: four-band seed ordering after rerank. A rank-31+ seed that was
/// never judged by the reranker must not sink below a reranked seed the model
/// scored near zero (judged garbage) — only below seeds it approved.
///
/// Band 0: exact anchors (structural priority, as before RFC-021).
/// Band 1: reranked, at/above the weak floor (model-approved, best first).
/// Band 2: unreranked tail (never judged): keeps RRF order via sort stability.
/// Band 3: reranked, below the weak floor (model-rejected: judged garbage
///         must not outrank seeds that were never judged).
///
/// `sort_by` is stable, so within band 2 every key is equal and the pre-sort
/// (RRF) relative order is preserved for free.
fn sort_seeds_post_rerank(seeds: &mut [Seed], weak_floor: f32) {
    let band = |s: &Seed| -> u8 {
        if s.source == SeedSource::Exact {
            0
        } else {
            match s.rerank_score {
                Some(r) if r >= weak_floor => 1,
                None => 2,
                Some(_) => 3,
            }
        }
    };
    seeds.sort_by(|a, b| {
        band(a).cmp(&band(b)).then_with(|| {
            b.rerank_score
                .unwrap_or(f32::NEG_INFINITY)
                .partial_cmp(&a.rerank_score.unwrap_or(f32::NEG_INFINITY))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    });
}

/// RFC-021 Phase 3: the reranker's absolute score is the sole seed-selection /
/// abstention signal for NL queries. Two guards preserve the deterministic
/// paths the reranker must never arbitrate:
///
/// - **G1 (exact-symbol bypass):** when the user named a genuinely *rare*
///   symbol — an exact anchor exists **and** the specific term that produced
///   that anchor is rare enough — `symbol_freq <= rare_anchor_max()` — to be
///   a real, specific identifier, not a coincidence — confidence is judged
///   deterministically by [`classify_confidence_lexical_fallback`] regardless
///   of the rerank score. The cross-encoder is NL-trained and would
///   false-abstain on a literal identifier query (e.g. `GetWarningsForPod`
///   scores low against its own name, and is rare: `symbol_freq` 1) —
///   trading salad for missed precise lookups.
///
///   Rarity, not mere coverage, is the load-bearing check: `"delete all user
///   accounts and drop the database"` (the RFC's own reproduction query)
///   resolves an exact anchor too — `drop`/`delete` coincidentally name
///   `SqliteStore.drop`/`delete_file` — and coverage alone measured 3/5 =
///   0.6, clearing the same `coverage_strong()` bar a genuine hit would (a
///   coverage-only gate was tried and still let this exact query bypass to
///   `Confidence::Strong` — Defect A reproduced through the G1 escape
///   hatch). What actually separates the two cases is that `drop` is
///   anything but rare — dozens of types implement `Drop` in this repo, so
///   its `symbol_freq` is large — while `GetWarningsForPod` is unique.
///   Requiring rarity is also embeddings-independent (G2): it reads only
///   `ResolvedTerm.symbol_freq`, a structural fact from the graph, never an
///   oracle/cosine.
///
///   Rarity is tied to the anchor-producing term itself, not just any resolved
///   term in the query, via [`compute_g1_bypass`] — see its doc for why. The
///   caller (`build_seed_set`) computes this once and passes it in as
///   `g1_bypass`, also using it to skip rerank inference entirely on bypassed
///   queries (RFC-021 F2), since the model's opinion is unused either way.
/// - **Reranker unavailable (`rerank_score: None`):** model absent, disabled
///   (`TRAVSR_NO_RERANK`), load failed, panicked, or ran over budget — fall
///   back to the identical pre-RFC-021 gate. No regression, no partial state.
///
/// Otherwise the four-arm absolute-floor gate is embeddings-independent by
/// construction (embeddings remain a candidate *source* into RRF fusion, never
/// a confidence input here) — G2: confidence is identical embed-on/off for
/// the same query.
#[allow(clippy::too_many_arguments)]
fn classify_confidence(
    terms: &[ResolvedTerm],
    coverage: f32,
    top_bm25: f32,
    has_any_seeds: bool,
    has_knn_seeds: bool,
    knn_oracle: &HashMap<NodeId, f32>,
    anchor_oracle: &HashMap<NodeId, f32>,
    scored_ids: &std::collections::HashSet<NodeId>,
    cal: &Calibration,
    g1_bypass: bool,
    rerank_score: Option<f32>,
) -> Confidence {
    if g1_bypass {
        return classify_confidence_lexical_fallback(
            terms,
            coverage,
            top_bm25,
            has_any_seeds,
            has_knn_seeds,
            knn_oracle,
            anchor_oracle,
            scored_ids,
            cal,
        );
    }
    match rerank_score {
        Some(r) if r >= rerank_strong_floor() => Confidence::Strong,
        Some(r) if r >= rerank_weak_floor() => Confidence::Weak,
        Some(_) => Confidence::None,
        None => classify_confidence_lexical_fallback(
            terms,
            coverage,
            top_bm25,
            has_any_seeds,
            has_knn_seeds,
            knn_oracle,
            anchor_oracle,
            scored_ids,
            cal,
        ),
    }
}

// ── Main entry point ──────────────────────────────────────────────────────────

/// First two slash-delimited path segments, e.g. "crates/travsr-retrieval" from
/// "crates/travsr-retrieval/src/ppr.rs".  Returns `None` for flat or single-segment paths.
pub(crate) fn package_root(path: &str) -> Option<&str> {
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

// ── Tier-1 semantic validation thresholds ─────────────────────────────────────

/// Minimum top cosine for the embed oracle to be trusted at all.  Below this the
/// embedding found nothing strongly relevant, so lexical seeds are left untouched
/// (avoids cutting good FTS results on queries the model is weak on).
const SEMANTIC_ORACLE_MIN: f32 = 0.58;
/// Multiplier applied to the strongest non-exact seed weight to set the floor for
/// exact-anchor seeds — guarantees a literal symbol match anchors the PPR walk above
/// any KNN neighbour whose cosine×kind_boost would otherwise dominate.
const EXACT_SEED_PRIORITY: f32 = 1.5;
/// Keep seeds whose cosine is within this band of the top oracle score …
const SEMANTIC_REL_DELTA: f32 = 0.13;
/// … but never below this absolute cosine floor.
const SEMANTIC_ABS_FLOOR: f32 = 0.55;
/// Never cut below this many seeds — a grounded result must not be emptied by
/// validation even when the oracle is harsh.
const SEMANTIC_MIN_KEEP: usize = 5;

/// #393 score-aware scope gate. A lexical seed whose package root is outside the
/// anchor crate scope is admitted when its per-batch normalised score reaches
/// this floor. Rationale: strong lexical evidence (the query term directly names
/// a symbol or a path segment in another crate) is real relevance, not cross-
/// domain drift, so it overrides the structural scope. Weak FTS trigram drift
/// (e.g. "works" → "workspace") scores far below the floor and stays gated,
/// preserving the anti-"confident salad" protection. Generic: match *strength*
/// decides — no per-term string, plural, or stemming heuristics.
///
/// Default 0.3; override via `TRAVSR_SCOPE_STRONG_FLOOR` for bench tuning. Because
/// `norm` is clamped to `<= 1.0`, a value `> 1.0` (e.g. 2.0) restores the pre-#393
/// hard scope gate — every out-of-scope node is dropped, nothing overrides scope.
const SCOPE_STRONG_FLOOR_DEFAULT: f32 = 0.3;

fn scope_strong_floor() -> f32 {
    std::env::var("TRAVSR_SCOPE_STRONG_FLOOR")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&x: &f32| (0.0..=2.0).contains(&x))
        .unwrap_or(SCOPE_STRONG_FLOOR_DEFAULT)
}

/// #393 score-aware scope gate decision (pure, so the admit/gate policy is
/// unit-testable without a store fixture). Returns `true` = DROP the seed.
///
/// An out-of-scope lexical seed is dropped only when anchors established a scope
/// AND the seed's normalised score is below `floor`. In-scope seeds, seeds with
/// no anchor scope, and strong out-of-scope seeds (`norm >= floor`) are kept.
fn scope_gate_drops(has_anchor_scope: bool, in_scope: bool, norm: f32, floor: f32) -> bool {
    has_anchor_scope && !in_scope && norm < floor
}

/// Demote seeds the embed oracle places far from the query, then reshuffle the
/// survivors by cosine.  A seed absent from the oracle (not among the over-fetched
/// nearest neighbours) is treated as semantically distant (cosine 0.0) — this is
/// what cuts a spurious lexical anchor like `c1_raw_literal` (FTS-matched the NL
/// word "literal", cosine far below the genuine semantic neighbours).
///
/// No-op when the oracle is empty (embeddings off/degraded) or not confident
/// (top cosine < `SEMANTIC_ORACLE_MIN`) — lexical evidence then stands unchanged.
fn semantic_validate(
    seeds: Vec<Seed>,
    oracle: &HashMap<NodeId, f32>,
    cal: &Calibration,
) -> Vec<Seed> {
    if oracle.is_empty() || seeds.is_empty() {
        return seeds;
    }
    let top = oracle.values().copied().fold(0.0_f32, f32::max);
    if top < cal.map(SEMANTIC_ORACLE_MIN) {
        return seeds; // oracle not confident — trust lexical evidence
    }
    let floor = (top - cal.map_delta(SEMANTIC_REL_DELTA)).max(cal.map(SEMANTIC_ABS_FLOOR));

    let mut scored: Vec<(Seed, f32)> = seeds
        .into_iter()
        .map(|s| {
            let cos = oracle.get(&s.node).copied().unwrap_or(0.0);
            (s, cos)
        })
        .collect();

    let n_above = scored.iter().filter(|(_, c)| *c >= floor).count();
    if n_above < SEMANTIC_MIN_KEEP {
        // Graceful: too few clear the floor — keep the top-N by cosine so a
        // grounded result is never emptied by an over-harsh oracle.
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(SEMANTIC_MIN_KEEP);
    } else {
        scored.retain(|(_, c)| *c >= floor);
        // Reshuffle: most semantically-relevant first.
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    }

    scored.into_iter().map(|(s, _)| s).collect()
}

/// RFC-021 / Issue D — apply `semantic_validate`'s whole-query cosine filter to
/// the lexical/KNN recall tail **only**. `SeedSource::Exact` anchors (deterministic
/// FTS name matches) bypass it entirely and always survive to the reranker, which
/// RFC-021 §15 ratified as the *sole* relevance arbiter.
///
/// Why: `semantic_validate` scores an oracle-absent seed as cosine `0.0` and cuts
/// it. For a conversational query like `"give me data about daemon"`, the *diluted*
/// whole-query embedding scores the genuine `Daemon` exact anchors below the floor,
/// so they were dropped before the cross-encoder ever judged them → false abstain.
/// Letting exact anchors through fixes that without re-introducing the veto: salad
/// (`"...drop the database"`) still abstains because the reranker scores the
/// surviving exact `drop` seed low downstream, not because this stage dropped it.
/// Lexical/KNN seeds (coincidental token-overlap like `"literal"`/`"semantic"`, and
/// embedding neighbours) remain subject to the cosine cut, as before.
fn semantic_validate_preserving_exact(
    seeds: Vec<Seed>,
    oracle: &HashMap<NodeId, f32>,
    cal: &Calibration,
) -> Vec<Seed> {
    let (exact, tail): (Vec<Seed>, Vec<Seed>) = seeds
        .into_iter()
        .partition(|s| s.source == SeedSource::Exact);
    let mut kept = semantic_validate(tail, oracle, cal);
    kept.extend(exact);
    kept
}

// ── RFC-019 seed re-rank helpers ──────────────────────────────────────────────

/// Cosine at/above which a **structurally disjoint** non-exact seed is rescued from
/// the contamination gate — i.e. treated as a genuine cross-package semantic match
/// the graph missed rather than a token-collision false positive. Set near-duplicate
/// high: measured token-collision seeds (`RemovePod` ≈0.751 for `GetWarningsForPod`)
/// sit below it, so they are dropped, while a true near-duplicate (≈0.9) survives.
/// Override via `TRAVSR_DISJOINT_RESCUE_COS`.
fn disjoint_rescue_cos() -> f32 {
    std::env::var("TRAVSR_DISJOINT_RESCUE_COS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&x: &f32| (0.0..=1.0).contains(&x))
        .unwrap_or(0.85)
}

/// Max node-visits when expanding the exact anchors' structural neighbourhood —
/// guards against a pathological high-degree hub blowing up the BFS.
const ANCHOR_NEIGHBORHOOD_CAP: usize = 2_000;
/// Hop radius for the structural-disjointness test in the contamination gate.
const ANCHOR_NEIGHBORHOOD_HOPS: usize = 2;
/// Max forward callees seeded per query from the exact anchors (ranked by
/// `kind_boost`), so anchor-callee seeding cannot flood the personalisation vector.
const MAX_ANCHOR_CALLEE_SEEDS: usize = 8;
/// Base weight for an anchor-callee seed before `kind_boost`. High enough that the
/// delegated function body ranks as a real dependency, below an exact anchor's
/// floated weight so it never outranks the named symbol itself.
const ANCHOR_CALLEE_WEIGHT_BASE: f32 = 0.8;

/// Bounded ≤`ANCHOR_NEIGHBORHOOD_HOPS`-hop neighbourhood of `anchors` over
/// structural edges in **both** directions. Used only to distinguish a
/// contaminated seed (no path to any exact anchor) from a genuine dependency.
/// O(visited·avg_degree); capped at `ANCHOR_NEIGHBORHOOD_CAP` visits. Edge-read
/// errors degrade gracefully (that frontier simply doesn't expand).
fn anchor_neighborhood(
    store: &SqliteStore,
    anchors: &std::collections::HashSet<NodeId>,
) -> std::collections::HashSet<NodeId> {
    let mut visited: std::collections::HashSet<NodeId> = anchors.iter().copied().collect();
    let mut frontier: Vec<NodeId> = anchors.iter().copied().collect();
    for _ in 0..ANCHOR_NEIGHBORHOOD_HOPS {
        if frontier.is_empty() || visited.len() >= ANCHOR_NEIGHBORHOOD_CAP {
            break;
        }
        let mut next: Vec<NodeId> = Vec::new();
        for node in frontier.drain(..) {
            if let Ok(fwd) = store.iter_edges_from(node) {
                for e in fwd {
                    if visited.insert(e.dst) {
                        next.push(e.dst);
                    }
                }
            }
            if let Ok(rev) = store.iter_edges_to(node) {
                for e in rev {
                    if visited.insert(e.src) {
                        next.push(e.src);
                    }
                }
            }
            if visited.len() >= ANCHOR_NEIGHBORHOOD_CAP {
                break;
            }
        }
        frontier = next;
    }
    visited
}

/// Forward `RefCall`/`Depends` callees of the exact `anchors`, as new PPR seeds.
/// Filters build/test noise and RBAC-denied nodes, skips ids already seeded, and
/// keeps the top `MAX_ANCHOR_CALLEE_SEEDS` by `kind_boost` (deterministic tie-break
/// on NodeId). Weight = `ANCHOR_CALLEE_WEIGHT_BASE × kind_boost`.
fn anchor_callee_seeds(
    store: &SqliteStore,
    filter: &dyn EdgeFilter,
    anchors: &std::collections::HashSet<NodeId>,
    existing: &std::collections::HashSet<NodeId>,
) -> Vec<Seed> {
    let mut callee_ids: std::collections::HashSet<NodeId> = std::collections::HashSet::new();
    for &anchor in anchors {
        if let Ok(fwd) = store.iter_edges_from(anchor) {
            for e in fwd {
                if matches!(e.kind, EdgeKind::RefCall | EdgeKind::Depends)
                    && !existing.contains(&e.dst)
                    && !anchors.contains(&e.dst)
                {
                    callee_ids.insert(e.dst);
                }
            }
        }
    }
    if callee_ids.is_empty() {
        return Vec::new();
    }
    let ids: Vec<NodeId> = callee_ids.into_iter().collect();
    let nodes = match store.get_nodes(&ids) {
        Ok(n) => n,
        Err(_) => return Vec::new(),
    };
    let mut scored: Vec<(NodeId, f32)> = nodes
        .into_iter()
        .filter(|n| !is_noise_seed(n))
        .filter(|n| filter.allow(n.id, n.id, Some(n.vname.corpus.as_str())))
        .map(|n| {
            let w = ANCHOR_CALLEE_WEIGHT_BASE * kind_boost(&n.kind, &n.vname.language);
            (n.id, w)
        })
        .collect();
    // Highest kind_boost first; deterministic tie-break on NodeId ascending.
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    scored.truncate(MAX_ANCHOR_CALLEE_SEEDS);
    scored
        .into_iter()
        .map(|(node, weight)| Seed {
            node,
            weight,
            source: SeedSource::Lexical,
            score: weight,
            rerank_score: None,
        })
        .collect()
}

/// Build a `SeedSet` for `query` using per-token anchor resolution + whole-query
/// lexical FTS, fused via RRF.
///
/// When `knn_pairs` is non-empty (Tier 1 — embed sidecar contributed), those pairs
/// are included as a third source in `rrf_fuse`, weighted `cosine × kind_boost`.
/// KNN is score-gated (in `embed_path_seeds`), not structure-gated, so it can
/// surface semantically relevant nodes in different crates than the FTS/anchor
/// scope.  After fusion, `semantic_validate` consults the `knn_oracle` (query
/// cosine for every over-fetched candidate) to demote lexical anchors the
/// embedding disagrees with — fixing the "confident salad" on NL queries whose
/// words resolve to rare-by-coincidence symbols.
///
/// Filters applied before RRF:
/// - `is_noise_seed` — excludes crates, test/bench fixtures, build artefacts
/// - RBAC via `filter`
/// - `MAX_SEEDS_PER_PATH` dedup (2 per file) to prevent all seeds clustering in one file
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub(crate) fn build_seed_set(
    store: &SqliteStore,
    query: &str,
    filter: &dyn EdgeFilter,
    knn_pairs: Vec<(CoreNode, f32)>,
    knn_oracle: &HashMap<NodeId, f32>,
    score_fn: Option<&dyn Fn(&str, &[NodeId]) -> Vec<(NodeId, f32)>>,
) -> SeedSet {
    const MAX_SEEDS_PER_PATH: usize = 2;

    let intent = classify_intent(query);
    let content_tokens = tokenize_query(query);
    let n_content = content_tokens.len();

    // Model-relative floor calibration for this repo's active embedding model.
    // Identity (no-op) when the repo is un-calibrated or embedded with the bge-small
    // reference model — so the FTS-only and bge-small paths are byte-for-byte unchanged.
    let cal = Calibration::load(store);

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
        // `added_for_this_token` guarantees this token at least one anchor slot even
        // when the shared per-path budget is already exhausted by an EARLIER token —
        // otherwise a common word processed first (e.g. "daemon") can fully starve a
        // later, more specific token's match in the same file (e.g. "watch" ->
        // handle_watch_event in crates/travsr-daemon/src/lib.rs, dropped purely
        // because "daemon" already filled that path's 2-slot cap). Every candidate
        // still has to pass noise/filter/token_is_sig_component first, so the bypass
        // only ever fires for a genuine match — it just can't be starved by token
        // processing order. Beyond its first slot, a token is capped exactly as
        // before.
        let mut added_for_this_token = 0usize;
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
            if *path_count >= MAX_SEEDS_PER_PATH && added_for_this_token > 0 {
                continue;
            }
            *path_count += 1;
            added_for_this_token += 1;
            let w = idf_w * kind_boost(&node.kind, &node.vname.language);
            anchor_raw.push((node.id, w));
        }
    }

    // `rrf_fuse` (below) requires every source sorted descending by score —
    // "position 0 = best" is its documented precondition, since it scores by
    // list position, not by the weight itself. `anchor_raw` is built in
    // token-processing order (all of one token's matches, then the next), not
    // relevance order, so without this sort a token processed later in the
    // query (e.g. "daemon") gets a worse RRF rank than an earlier token's
    // weaker matches (e.g. "data") purely because it was appended later —
    // regardless of which one is actually the better match.
    anchor_raw.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // G1: computed here, before semantic_validate, not after it. `anchor_raw`
    // is exactly what will become the Exact-sourced seeds once RRF fusion
    // runs, so its own node ids are the right (and only available) exact-id
    // set at this point. This matters: `semantic_validate` (below) can drop
    // an Exact seed whose cosine to the whole-query embedding is low even
    // when the seed is a genuine, rare, literal match — e.g. "give me data
    // about daemon" scored struct:Daemon's cosine below several unrelated
    // "data"-flavoured seeds', and semantic_validate silently removed it
    // before G1 (which exists precisely to protect rare exact matches from
    // NL-trained signals) ever got a chance to see it, since the old
    // g1_bypass was computed from the POST-semantic_validate seed list. G1's
    // deterministic path must not depend on the NL embedding oracle agreeing
    // first — that is exactly the failure mode it exists to prevent.
    let early_exact_ids: std::collections::HashSet<NodeId> =
        anchor_raw.iter().map(|&(id, _)| id).collect();
    let g1_bypass = compute_g1_bypass(&terms, &early_exact_ids);

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

    // #393: `search_nodes_fuzzy_scored` now returns RRF-ordered + kind-diversified
    // results, so the strongest lexical match is NO LONGER at position 0. Take the
    // explicit max over the batch — using `.first()` here would understate the
    // normalisation denominator and make the score-aware scope gate below (and the
    // `top_bm25` abstention signal) silently more permissive than intended.
    let top_bm25 = lexical_scored.iter().map(|p| p.1).fold(0.0_f32, f32::max);
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
        let norm = (bm25 / max_bm25).clamp(0.05, 1.0);
        // Score-aware structural scope gate (#393). If anchors established a crate
        // scope, a node outside it is dropped ONLY when its lexical match is weak
        // (normalised score < scope_strong_floor()). A strong lexical match is trusted
        // regardless of crate, so a legitimately multi-domain query (a term that
        // names both a symbol and a directory in different crates) is not collapsed
        // to a single anchor's scope. Weak cross-domain FTS drift stays gated.
        // Nodes with no parseable root are allowed through as before.
        //
        // Safety note: on a warm embed oracle, `semantic_validate` / the RFC-019
        // contamination gate further prune anything admitted here that the query
        // embedding disagrees with. On the FTS-only / cold-oracle path there is no
        // such backstop, so the `scope_strong_floor()` (relative BM25) IS the gate.
        // The floor is deliberately the sole knob (env-tunable), and it depends on
        // `max_bm25` being the true batch max above — hence the explicit fold-max.
        let in_scope = match package_root(&node.vname.path) {
            Some(root) => anchor_roots.contains(root),
            None => true, // no parseable root — allowed through as before
        };
        if scope_gate_drops(
            !anchor_roots.is_empty(),
            in_scope,
            norm,
            scope_strong_floor(),
        ) {
            continue;
        }
        let path_count = lex_path_counts.entry(node.vname.path.clone()).or_insert(0);
        if *path_count >= MAX_SEEDS_PER_PATH {
            continue;
        }
        *path_count += 1;
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
    // Nodes that appear in both KNN and anchor/FTS naturally rank higher via
    // RRF multi-source fusion, so semantically-correct nodes beat noise even
    // when noise-adjacent neighbours slip through.
    //
    // Weight = cosine × kind_boost, mirroring the anchor/lexical weighting so a
    // KNN seed competes fairly in the PPR personalisation vector. Raw cosine
    // (~0.65–0.80) would otherwise lose to a normalised FTS match × kind_boost
    // (up to ~3.0), systematically under-weighting semantic seeds.
    // Filter KNN results through is_noise_seed before weighting: build-cache nodes,
    // test paths, and OS caches must not enter the PPR personalisation vector.
    let knn_raw: Vec<(NodeId, f32)> = knn_pairs
        .iter()
        .filter(|(n, _)| !is_noise_seed(n))
        .map(|(n, s)| {
            (
                n.id,
                *s * crate::tools::kind_boost(&n.kind, &n.vname.language),
            )
        })
        .collect();

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
                rerank_score: None,
            }
        })
        .collect();

    let has_knn = !knn_raw.is_empty();

    // ── RFC-019: direct-cosine oracle augmentation ─────────────────────────────
    // The KNN oracle only holds cosines for nodes the KNN over-fetch happened to
    // surface. MEASURE the specific anchors (and exact-anchor nodes) it missed, so
    // the classifier/ranker use a true query↔candidate cosine instead of inferring
    // "relevance by membership". `scored_ids` records every id we submitted so the
    // classifier can tell "scored but unscoreable → unknown" from "never scored".
    // No-op when `score_fn` is None: `augmented_oracle == *knn_oracle`, `scored_ids`
    // empty → every downstream path is byte-for-byte identical to the FTS-only path.
    let mut augmented_oracle: HashMap<NodeId, f32> = knn_oracle.clone();
    let mut scored_ids: std::collections::HashSet<NodeId> = std::collections::HashSet::new();
    if let Some(score) = score_fn {
        let mut to_score: Vec<NodeId> = Vec::new();
        let mut seen: std::collections::HashSet<NodeId> = std::collections::HashSet::new();
        for t in &terms {
            if let Some(n) = t.top_node {
                if !knn_oracle.contains_key(&n) && seen.insert(n) {
                    to_score.push(n);
                }
            }
        }
        for &(id, _) in &anchor_raw {
            if !knn_oracle.contains_key(&id) && seen.insert(id) {
                to_score.push(id);
            }
        }
        if !to_score.is_empty() {
            scored_ids.extend(to_score.iter().copied());
            for (id, cos) in score(query, &to_score) {
                augmented_oracle.insert(id, cos);
            }
        }
    }

    // ── Tier-1 semantic validation ────────────────────────────────────────────
    // When the embed oracle is confident, use the query cosine to demote LEXICAL
    // anchors the embedding disagrees with. NL query words that are rare-by-
    // coincidence ("literal", "semantic") resolve to lexical anchors that are
    // semantically unrelated to intent; the whole-query embedding detects this.
    // No-op when embeddings are absent/degraded.
    //
    // RFC-021 / Issue D: exact anchors are EXEMPT (see
    // `semantic_validate_preserving_exact`). A deterministic FTS name match must
    // reach the reranker — the ratified relevance arbiter — and never be vetoed
    // by a diluted whole-query embedding ("give me data about daemon" abstained
    // because the `Daemon` exact anchors were cosine-cut before the cross-encoder
    // saw them). Salad still abstains: the reranker judges the surviving exact
    // seed low downstream.
    //
    // Skipped entirely when G1 already bypassed: the user named a genuinely
    // rare, specific symbol, and G1's whole point is that a deterministic
    // exact match must not be second-guessed by an NL-trained embedding signal.
    let mut seeds = if g1_bypass {
        seeds
    } else {
        semantic_validate_preserving_exact(seeds, &augmented_oracle, &cal)
    };

    // ── RFC-019: seed re-rank (contamination gate + anchor-callee seeding) ─────
    // Only when the query named an exact symbol (an exact anchor exists) — never on
    // pure-NL queries, which have no anchor and must be left to the semantic path.
    let exact_anchor_ids: std::collections::HashSet<NodeId> = seeds
        .iter()
        .filter(|s| s.source == SeedSource::Exact)
        .map(|s| s.node)
        .collect();
    if !exact_anchor_ids.is_empty() {
        // Bounded ≤2-hop structural neighbourhood of the exact anchors (both
        // directions). Computed once; a non-exact seed OUTSIDE it is structurally
        // disjoint from the named symbol.
        let neighborhood = anchor_neighborhood(store, &exact_anchor_ids);

        // Contamination gate. When the user names an exact symbol, its relevant
        // context is its STRUCTURAL neighbourhood, and a token-collision embedding
        // false positive is not distinguishable by cosine: measured on the live
        // k8s index, `InterPodAffinity.RemovePod` scores cosine **0.751** to query
        // `GetWarningsForPod` purely on the shared "Pod"/method tokens (BGE is
        // NL-trained and cannot disambiguate code identifiers) — well above any
        // sane floor. The reliable signal is structure: drop a non-exact seed that
        // is structurally DISJOINT from every exact anchor, UNLESS its cosine is
        // near-duplicate-high (a genuine cross-package semantic match the graph
        // missed). Structurally-connected seeds and un-scoreable seeds are always
        // kept, and the gate never fires on pure-NL queries (no exact anchor), so
        // conceptual retrieval is untouched.
        let rescue = cal.map(disjoint_rescue_cos());
        seeds.retain(|s| {
            if s.source == SeedSource::Exact {
                return true;
            }
            let disjoint = !neighborhood.contains(&s.node);
            // Drop only a disjoint seed whose (known) cosine is below the
            // near-duplicate rescue bar. Connected or un-scoreable → keep.
            !disjoint
                || augmented_oracle
                    .get(&s.node)
                    .map(|&c| c >= rescue)
                    .unwrap_or(true)
        });

        // Anchor-callee seeding: add the exact anchors' own 1-hop RefCall/Depends
        // callees as seeds, so the function body it delegates to
        // (`warningsForPodSpecAndMeta` for `GetWarningsForPod` — private/lowercase,
        // never lexically seeded) is scored as the dependency it is instead of
        // being buried below a tangential seed. `enrich_seeds_with_callers` adds
        // reverse (caller) edges; this adds the complementary forward callees.
        let existing: std::collections::HashSet<NodeId> = seeds.iter().map(|s| s.node).collect();
        let callees = anchor_callee_seeds(store, filter, &exact_anchor_ids, &existing);
        seeds.extend(callees);
    }

    // Exact-name priority: a node reached via an exact anchor hit is the symbol the
    // user literally named. It must anchor the PPR walk above any semantically-near
    // KNN neighbour — otherwise a high cosine×kind_boost weight (e.g. method:TestJig.Run
    // for query "type:Tweak") outweighs the exact match in the personalisation vector
    // and PPR ranks the neighbour first. Float every Exact seed's weight just above the
    // strongest non-exact seed so PPR concentrates mass on the named symbol and pulls
    // ITS neighbours in as context. No-op when there is no exact match (pure-NL queries).
    // RFC-019 cosine-scaled teleportation: when the oracle is confident, scale each
    // non-exact seed's PPR weight by its true cosine so teleportation mass tracks
    // semantic proximity. A 0.46-cosine seed can no longer hand itself a PPR restart
    // floor above a strong structural dependency of the 1.00 exact anchor (see the
    // `GetWarningsForPod` evidence). Absent-from-oracle → cosine unknown → weight
    // left unchanged (structural seeds like anchor callees keep their justification).
    // No-op when the oracle is cold/absent.
    let oracle_top = augmented_oracle.values().copied().fold(0.0_f32, f32::max);
    let oracle_confident = oracle_top >= cal.map(SEMANTIC_ORACLE_MIN);
    if oracle_confident {
        for s in seeds.iter_mut() {
            if s.source != SeedSource::Exact {
                if let Some(&cos) = augmented_oracle.get(&s.node) {
                    s.weight *= cos.clamp(0.0, 1.0);
                }
            }
        }
    }

    let max_non_exact = seeds
        .iter()
        .filter(|s| s.source != SeedSource::Exact)
        .map(|s| s.weight)
        .fold(0.0_f32, f32::max);
    if max_non_exact > 0.0 {
        let floor = max_non_exact * EXACT_SEED_PRIORITY;
        // Fix 2: when the oracle is confident, only float an exact anchor the
        // embedding ALSO places near the query. A generic English word that
        // happens to be an exact symbol name (e.g. the kubelet `RECONCILE`
        // constant matched for "HorizontalPodAutoscaler reconcile loop") must not
        // outrank the semantically-correct neighbour (`reconcileAutoscaler`).
        // No-op when the oracle is cold/absent — `agrees` is vacuously true.
        let cos_floor =
            (oracle_top - cal.map_delta(SEMANTIC_REL_DELTA)).max(cal.map(SEMANTIC_ABS_FLOOR));
        for s in seeds.iter_mut() {
            if s.source == SeedSource::Exact {
                let agrees = !oracle_confident
                    || augmented_oracle.get(&s.node).copied().unwrap_or(0.0) >= cos_floor;
                if agrees {
                    s.weight = s.weight.max(floor);
                }
            }
        }
    }

    // G1: confidence will be decided by the deterministic lexical path
    // regardless of what the reranker says, so the (NL-trained) reranker's
    // opinion is unused: skip inference (RFC-021 F2). `g1_bypass` was already
    // computed above, before semantic_validate ran, and is reused here as-is
    // — recomputing it from the post-semantic_validate `exact_anchor_ids`
    // would reintroduce the bug this fix closes (semantic_validate silently
    // removing the seed G1 was supposed to protect, before G1 ever saw it).

    // ── RFC-021: cross-encoder rerank ────────────────────────────────────────
    // Reranks the top-K fused candidates against the query text, attaches
    // Seed.rerank_score, and reorders (exact anchors keep structural priority
    // — they are the literally-named symbol; rerank orders everything else).
    // `max_rerank_score` (None = reranker had no opinion for this query — see
    // classify_confidence's fallback arm) feeds Phase 3's confidence gate. A
    // no-op — every seed keeps `rerank_score: None`, original ordering, and
    // `max_rerank_score` stays `None` — when the reranker is absent, disabled,
    // degraded for this query, or bypassed by G1, so behaviour is byte-for-byte
    // unchanged without it.
    let mut max_rerank_score: Option<f32> = None;
    if !seeds.is_empty() && !g1_bypass {
        let topk = crate::rerank::rerank_topk();
        let repo_root = match store.get_meta("repo_root") {
            Ok(Some(r)) if !r.is_empty() => Some(std::path::PathBuf::from(r)),
            _ => None,
        };
        let take_n = seeds.len().min(topk);
        let candidate_ids: Vec<NodeId> = seeds[..take_n].iter().map(|s| s.node).collect();
        if let Ok(candidate_nodes) = store.get_nodes(&candidate_ids) {
            let node_by_id: HashMap<NodeId, &CoreNode> =
                candidate_nodes.iter().map(|n| (n.id, n)).collect();
            // Signature-first: truncation (travsr-rerank, OnlySecond) drops from
            // the skeleton tail, never the signature. RFC-021 F5: cap the read at
            // ~10 lines instead of the kind-default (up to 40) — travsr-rerank
            // truncates candidate text to MAX_CANDIDATE_CHARS (480, ~10 lines at
            // ~48 chars/line) before tokenization, so lines past that are always
            // discarded; reading fewer lines here saves the per-candidate
            // collection/format work without changing what the model sees.
            const RERANK_SNIPPET_LINES: usize = 10;
            let texts: Vec<String> = candidate_ids
                .iter()
                .map(|id| match node_by_id.get(id) {
                    Some(n) => {
                        let skeleton = repo_root.as_deref().and_then(|root| {
                            travsr_analysis::snippet::snippet_for_node_capped(
                                n,
                                root,
                                RERANK_SNIPPET_LINES,
                            )
                        });
                        match skeleton {
                            Some(body) => format!("{}\n{}", n.vname.signature, body),
                            None => n.vname.signature.clone(),
                        }
                    }
                    None => String::new(),
                })
                .collect();
            let text_refs: Vec<&str> = texts.iter().map(String::as_str).collect();

            if let Some(scores) = crate::rerank::rerank(query, &text_refs) {
                for (seed, score) in seeds[..take_n].iter_mut().zip(scores) {
                    seed.rerank_score = Some(score);
                }
                sort_seeds_post_rerank(&mut seeds, rerank_weak_floor());
                max_rerank_score = Some(
                    seeds
                        .iter()
                        .filter_map(|s| s.rerank_score)
                        .fold(f32::NEG_INFINITY, f32::max),
                );
            }
        }
    }

    let confidence = classify_confidence(
        &terms,
        coverage,
        top_bm25,
        !seeds.is_empty(),
        has_knn,
        knn_oracle,        // cluster signals: KNN neighbours only
        &augmented_oracle, // per-anchor agreement: neighbours ∪ scored anchors
        &scored_ids,
        &cal,
        g1_bypass, // G1: rare exact-symbol queries stay deterministic
        max_rerank_score,
    );

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

    // ── #393 score-aware scope gate ──────────────────────────────────────────

    #[test]
    fn scope_gate_admits_strong_out_of_scope() {
        // Out of the anchor crate scope, but a strong lexical match (norm >= floor)
        // overrides the structural scope — the multi-domain case (#393).
        assert!(!scope_gate_drops(true, false, 0.50, 0.3));
        assert!(!scope_gate_drops(true, false, 0.30, 0.3)); // exactly at floor => kept
    }

    #[test]
    fn scope_gate_drops_weak_out_of_scope() {
        // Weak cross-domain FTS drift stays gated.
        assert!(scope_gate_drops(true, false, 0.10, 0.3));
        assert!(scope_gate_drops(true, false, 0.29, 0.3));
    }

    #[test]
    fn scope_gate_keeps_in_scope_regardless_of_score() {
        assert!(!scope_gate_drops(true, true, 0.01, 0.3));
    }

    #[test]
    fn scope_gate_noop_without_anchor_scope() {
        // No anchors established a scope => nothing is gated (unrestricted).
        assert!(!scope_gate_drops(false, false, 0.01, 0.3));
    }

    #[test]
    fn scope_gate_floor_above_one_restores_hard_gate() {
        // `norm` is clamped to <= 1.0, so a floor > 1.0 (TRAVSR_SCOPE_STRONG_FLOOR=2.0)
        // drops every out-of-scope node — the pre-#393 hard gate.
        for norm in [0.05f32, 0.5, 1.0] {
            assert!(scope_gate_drops(true, false, norm, 2.0));
        }
    }

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
        let c = classify_confidence_lexical_fallback(
            &terms,
            0.0,
            0.0,
            false,
            false,
            &HashMap::new(),
            &HashMap::new(),
            &std::collections::HashSet::new(),
            &Calibration::IDENTITY,
        );
        assert_eq!(c, Confidence::None);
    }

    #[test]
    fn calibration_map_is_identity_at_reference() {
        let id = Calibration::IDENTITY;
        for &x in &[0.55_f32, 0.58, 0.64, 0.66, 0.72, 0.77] {
            assert!((id.map(x) - x).abs() < 1e-6, "map({x}) must be identity");
        }
        assert!((id.map_delta(0.13) - 0.13).abs() < 1e-6);
    }

    #[test]
    fn calibration_map_scales_compressed_model() {
        // A model whose answerable band sits lower/narrower than bge-small's.
        let cal = Calibration { lo: 0.45, hi: 0.62 };
        assert!((cal.map(Calibration::REF_LO) - 0.45).abs() < 1e-6);
        assert!((cal.map(Calibration::REF_HI) - 0.62).abs() < 1e-6);
        // Interior floor stays strictly inside the mapped band, monotonic.
        let m = cal.map(0.66);
        assert!(m > 0.45 && m < 0.62);
        // A band-relative width scales by the band ratio: 0.13 * (0.17 / 0.22).
        let expected = 0.13 * (0.62 - 0.45) / (Calibration::REF_HI - Calibration::REF_LO);
        assert!((cal.map_delta(0.13) - expected).abs() < 1e-5);
    }

    #[test]
    fn calibration_recovers_low_cosine_answerable_query() {
        // Regression for the k8s arctic-256 over-abstention (bench L4/C4): a genuinely
        // answerable conceptual query whose whole cosine scale is lower than bge-small's.
        // Top cluster ~0.62 with a coherent near set, no lexical anchor.
        let mut oracle = HashMap::new();
        oracle.insert(NodeId(1), 0.62);
        oracle.insert(NodeId(2), 0.60);
        oracle.insert(NodeId(3), 0.58);
        let scored = std::collections::HashSet::new();

        // Reference (bge-small) floors demand oracle_top >= 0.72 to promote → abstains
        // even though this is the model's *answerable* band. This is the bug.
        let ref_c = classify_confidence_lexical_fallback(
            &[],
            0.0,
            0.0,
            true,
            true,
            &oracle,
            &oracle,
            &scored,
            &Calibration::IDENTITY,
        );
        assert_eq!(
            ref_c,
            Confidence::None,
            "absolute bge-small floors over-abstain on a lower-scale model"
        );

        // Auto-calibrated to this model's band → the same cosines clear the mapped
        // promote floor and the query is correctly Strong (not demoted to guesses).
        let cal = Calibration { lo: 0.45, hi: 0.62 };
        let got = classify_confidence_lexical_fallback(
            &[],
            0.0,
            0.0,
            true,
            true,
            &oracle,
            &oracle,
            &scored,
            &cal,
        );
        assert_eq!(
            got,
            Confidence::Strong,
            "model-relative floors recover the answerable query"
        );
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
        let c = classify_confidence_lexical_fallback(
            &terms,
            1.0,
            0.0,
            true,
            false,
            &HashMap::new(),
            &HashMap::new(),
            &std::collections::HashSet::new(),
            &Calibration::IDENTITY,
        );
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
        let c = classify_confidence_lexical_fallback(
            &terms,
            0.8,
            2.0,
            true,
            false,
            &HashMap::new(),
            &HashMap::new(),
            &std::collections::HashSet::new(),
            &Calibration::IDENTITY,
        );
        assert_eq!(c, Confidence::Strong);
    }

    #[test]
    fn confidence_strong_on_embed_agreed_specific_anchor() {
        // Regression: "type Tweak" → type:Tweak. The anchor recurs across packages
        // (symbol_freq > rare_max, so NOT Exact) but is high-IDF and the confident
        // embed oracle places it right at the query. A confident oracle must PROMOTE
        // an agreed specific anchor to Strong, not demote it to Weak (the pre-fix bug
        // where the only Strong path was gated on `!oracle_confident`).
        let tweak = NodeId(100);
        let terms = vec![
            ResolvedTerm {
                token: "tweak".into(),
                resolved: true,
                symbol_freq: 8, // > rare_max(3) → not a rare anchor
                idf_w: 0.81,    // >= idf_coverage_min(0.55) → specific anchor
                top_node: Some(tweak),
            },
            ResolvedTerm {
                token: "type".into(),
                resolved: true,
                symbol_freq: 5000,
                idf_w: 0.2,
                top_node: Some(NodeId(200)),
            },
        ];
        // Confident oracle that AGREES the anchor is near the query (cosine 0.95).
        let oracle = HashMap::from([(tweak, 0.95_f32)]);
        let c = classify_confidence_lexical_fallback(
            &terms,
            1.0,
            0.4,
            true,
            true,
            &oracle,
            &oracle,
            &std::collections::HashSet::new(),
            &Calibration::IDENTITY,
        );
        assert_eq!(
            c,
            Confidence::Strong,
            "embed-agreed specific anchor at high coverage must be Strong, not Weak"
        );
    }

    #[test]
    fn confidence_weak_on_embed_disagreed_specific_anchor() {
        // Salad guard: a high-IDF anchor the confident oracle places FAR from the
        // query (cosine 0.0 → absent from oracle) must NOT be promoted to Strong.
        // This is the "find code by semantic meaning" → is_semantic_edge case.
        let anchor = NodeId(100);
        let terms = vec![ResolvedTerm {
            token: "semantic".into(),
            resolved: true,
            symbol_freq: 2,
            idf_w: 0.9,
            top_node: Some(anchor),
        }];
        // Confident oracle whose top hit is a DIFFERENT node — anchor absent (cosine 0).
        let oracle = HashMap::from([(NodeId(999), 0.88_f32)]);
        let c = classify_confidence_lexical_fallback(
            &terms,
            1.0,
            0.4,
            true,
            true,
            &oracle,
            &oracle,
            &std::collections::HashSet::new(),
            &Calibration::IDENTITY,
        );
        assert_eq!(
            c,
            Confidence::Weak,
            "embed-disagreed anchor must stay Weak even at high coverage"
        );
    }

    #[test]
    fn confidence_rfc019_absent_but_scored_anchor_is_strong() {
        // RFC-019 core case: `PodSpec` resolves `class:PodSpec` (specific, high-IDF)
        // but that node lands OUTSIDE the KNN over-fetch → absent from the oracle.
        // The direct-cosine oracle DID score it (it's in `scored_ids`) and found no
        // stored vector / it's just absent — "unknown", NOT disagreement. Absence
        // must no longer read as cosine 0 → weak; a measured specific anchor at high
        // coverage is Strong.
        let anchor = NodeId(100);
        let terms = vec![ResolvedTerm {
            token: "podspec".into(),
            resolved: true,
            symbol_freq: 8, // recurs across packages → not rare (not Exact)
            idf_w: 0.58,    // >= idf_coverage_min → specific anchor
            top_node: Some(anchor),
        }];
        // Confident oracle (top 0.79) whose near cluster does NOT contain the anchor,
        // and only one node clears the floor (so semantic_strong can't fire — this
        // Strong must come from the anchor path).
        let oracle = HashMap::from([(NodeId(999), 0.79_f32)]);
        let scored: std::collections::HashSet<NodeId> = [anchor].into_iter().collect();
        let c = classify_confidence_lexical_fallback(
            &terms,
            1.0,
            0.4,
            true,
            true,
            &oracle,
            &oracle,
            &scored,
            &Calibration::IDENTITY,
        );
        assert_eq!(
            c,
            Confidence::Strong,
            "scored-but-absent specific anchor (unknown) must not be penalised → Strong"
        );
    }

    #[test]
    fn confidence_rfc019_absent_and_unscored_preserves_old_weak() {
        // Load-bearing safety property: with NO score hook (`scored_ids` empty — the
        // FTS-only path), an anchor absent from the confident oracle keeps the OLD
        // absent⇒disagree semantics → Weak, byte-for-byte identical to pre-RFC-019.
        // Identical inputs to the test above, only `scored_ids` differs.
        let anchor = NodeId(100);
        let terms = vec![ResolvedTerm {
            token: "podspec".into(),
            resolved: true,
            symbol_freq: 8,
            idf_w: 0.58,
            top_node: Some(anchor),
        }];
        let oracle = HashMap::from([(NodeId(999), 0.79_f32)]);
        let c = classify_confidence_lexical_fallback(
            &terms,
            1.0,
            0.4,
            true,
            true,
            &oracle,
            &oracle,
            &std::collections::HashSet::new(),
            &Calibration::IDENTITY,
        );
        assert_eq!(
            c,
            Confidence::Weak,
            "without a score hook, absent anchor keeps old absent⇒disagree → Weak"
        );
    }

    #[test]
    fn confidence_rfc019_low_coverage_answerable_band_gates_confirm() {
        // Measured "find code by semantic meaning" shape: the lone resolved word
        // "semantic" is also a symbol name, so the direct-cosine oracle MEASURES it
        // at ≈0.615 — a lexical-overlap-inflated cosine that clears the weak floor
        // (0.55) but sits in the nonsense band, below the answerable confirm floor
        // (0.66). At low coverage (0.25) it must NOT confirm to Strong; the same
        // shape with a genuine answerable-band anchor (0.71, "Load mutationg
        // manifests") MUST. This is the salad-regression guard for the absent⇒
        // unknown change.
        let sem = NodeId(60);
        let terms = vec![ResolvedTerm {
            token: "semantic".into(),
            resolved: true,
            symbol_freq: 100,
            idf_w: 0.63,
            top_node: Some(sem),
        }];
        // Cluster oracle_top 0.659 (confident, but below promote 0.72 → no
        // semantic_strong). Anchor oracle scores "semantic" at 0.615 (present, but
        // below confirm floor 0.66). scored → we measured it.
        let cluster = HashMap::from([(NodeId(999), 0.659_f32)]);
        let anchor_oracle = HashMap::from([(sem, 0.615_f32)]);
        let scored: std::collections::HashSet<NodeId> = [sem].into_iter().collect();
        let c = classify_confidence_lexical_fallback(
            &terms,
            0.25,
            15.0,
            true,
            true,
            &cluster,
            &anchor_oracle,
            &scored,
            &Calibration::IDENTITY,
        );
        assert_eq!(
            c,
            Confidence::Weak,
            "answerable-band gate: a below-0.66 anchor must not confirm a low-coverage salad"
        );

        // Same shape, anchor now in the answerable band (0.71) → Strong.
        let anchor_ok = HashMap::from([(sem, 0.71_f32)]);
        let c2 = classify_confidence_lexical_fallback(
            &terms,
            0.25,
            15.0,
            true,
            true,
            &cluster,
            &anchor_ok,
            &scored,
            &Calibration::IDENTITY,
        );
        assert_eq!(
            c2,
            Confidence::Strong,
            "answerable-band anchor (≥0.66) confirms a genuine low-coverage query"
        );
    }

    #[test]
    fn confidence_strong_on_embed_confirmed_low_coverage() {
        // Measured "Load mutationg manifests" shape: a typo ("mutationg") and a generic
        // token ("load", idf 0.42) drag classifier coverage to 1/3, but the confident
        // oracle places the specific anchor ("manifests", idf 0.66, cos 0.71 ≥ floor) near
        // the query. A real, embedding-confirmed match must be Strong, not Weak.
        let manifests = NodeId(50);
        let terms = vec![
            ResolvedTerm {
                token: "load".into(),
                resolved: true,
                symbol_freq: 1361,
                idf_w: 0.42, // generic — does not count toward coverage/specific anchor
                top_node: Some(NodeId(1)),
            },
            ResolvedTerm {
                token: "mutationg".into(),
                resolved: false, // typo — unresolved
                symbol_freq: 0,
                idf_w: 1.0,
                top_node: None,
            },
            ResolvedTerm {
                token: "manifests".into(),
                resolved: true,
                symbol_freq: 71,
                idf_w: 0.66,
                top_node: Some(manifests),
            },
        ];
        // Confident oracle (top 0.773) that AGREES the anchor is near (cos 0.71 ≥ floor 0.643).
        let oracle = HashMap::from([(manifests, 0.71_f32), (NodeId(99), 0.773_f32)]);
        let c = classify_confidence_lexical_fallback(
            &terms,
            0.333,
            19.0,
            true,
            true,
            &oracle,
            &oracle,
            &std::collections::HashSet::new(),
            &Calibration::IDENTITY,
        );
        assert_eq!(
            c,
            Confidence::Strong,
            "embed-confirmed specific anchor must be Strong even at low token coverage"
        );
    }

    #[test]
    fn confidence_weak_on_embed_unconfirmed_low_coverage_salad() {
        // Measured salad shape ("find code by semantic meaning"): a specific-ish anchor
        // ("semantic", idf 0.63) but the confident oracle does NOT place it near the query
        // (absent from the over-fetch → cos treated as 0). Must stay Weak — the embedding
        // confirmation gate is what separates this from "Load mutationg manifests".
        let semantic = NodeId(60);
        let terms = vec![
            ResolvedTerm {
                token: "semantic".into(),
                resolved: true,
                symbol_freq: 100,
                idf_w: 0.63,
                top_node: Some(semantic), // NOT in the oracle below → unconfirmed
            },
            ResolvedTerm {
                token: "code".into(),
                resolved: true,
                symbol_freq: 2260,
                idf_w: 0.38,
                top_node: Some(NodeId(2)),
            },
        ];
        // Confident oracle (top 0.659) whose neighbours do NOT include the anchor.
        let oracle = HashMap::from([(NodeId(99), 0.659_f32)]);
        let c = classify_confidence_lexical_fallback(
            &terms,
            0.25,
            15.0,
            true,
            true,
            &oracle,
            &oracle,
            &std::collections::HashSet::new(),
            &Calibration::IDENTITY,
        );
        assert_eq!(
            c,
            Confidence::Weak,
            "embed-unconfirmed anchor (salad) must stay Weak at low coverage"
        );
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
        let c = classify_confidence_lexical_fallback(
            &terms,
            0.3,
            0.1,
            true,
            false,
            &HashMap::new(),
            &HashMap::new(),
            &std::collections::HashSet::new(),
            &Calibration::IDENTITY,
        );
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
        let c = classify_confidence_lexical_fallback(
            &terms,
            0.1,
            0.0,
            true,
            false,
            &HashMap::new(),
            &HashMap::new(),
            &std::collections::HashSet::new(),
            &Calibration::IDENTITY,
        );
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
        let c = classify_confidence_lexical_fallback(
            &terms,
            0.0,
            0.1,
            true,
            false,
            &HashMap::new(),
            &HashMap::new(),
            &std::collections::HashSet::new(),
            &Calibration::IDENTITY,
        );
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
        let c = classify_confidence_lexical_fallback(
            &terms,
            0.0,
            0.0,
            true,
            true,
            &HashMap::new(),
            &HashMap::new(),
            &std::collections::HashSet::new(),
            &Calibration::IDENTITY,
        );
        assert_eq!(
            c,
            Confidence::None,
            "KNN-only (zero coverage, no anchor) must abstain"
        );
    }

    // ── Fix 1+3a / 1c: semantic promotion + veto ──────────────────────────────

    /// A confident oracle with a coherent near cluster grounds a conceptual query
    /// to Strong even with zero lexical coverage / no anchor — the "how does the
    /// kubelet sync pod status" case (oracle_top 0.83, every word too common).
    #[test]
    fn confidence_semantic_strong_promotes_conceptual_query() {
        let terms = vec![ResolvedTerm {
            token: "pod".into(),
            resolved: true,
            symbol_freq: 14_000, // generic → not rare, not specific
            idf_w: 0.30,
            top_node: None,
        }];
        // oracle_top 0.83 → floor 0.70; 5 candidates ≥ floor → coherent cluster.
        let oracle: HashMap<NodeId, f32> = [
            (NodeId(1), 0.83),
            (NodeId(2), 0.80),
            (NodeId(3), 0.78),
            (NodeId(4), 0.74),
            (NodeId(5), 0.71),
        ]
        .into_iter()
        .collect();
        let c = classify_confidence_lexical_fallback(
            &terms,
            0.0,
            0.0,
            true,
            true,
            &oracle,
            &oracle,
            &std::collections::HashSet::new(),
            &Calibration::IDENTITY,
        );
        assert_eq!(
            c,
            Confidence::Strong,
            "confident oracle + near cluster must promote to Strong without a lexical anchor"
        );
    }

    /// Nonsense whose oracle_top sits below the promote threshold (≤0.64 measured)
    /// must NOT be promoted — it abstains.
    #[test]
    fn confidence_semantic_strong_not_fired_for_nonsense() {
        let terms = vec![ResolvedTerm {
            token: "quux".into(),
            resolved: false,
            symbol_freq: 0,
            idf_w: 0.0,
            top_node: None,
        }];
        // oracle_top 0.64 < promote(0.72), and > veto(0.55) → neither promote nor veto.
        let oracle: HashMap<NodeId, f32> =
            [(NodeId(1), 0.64), (NodeId(2), 0.62), (NodeId(3), 0.60)]
                .into_iter()
                .collect();
        let c = classify_confidence_lexical_fallback(
            &terms,
            0.0,
            0.0,
            true,
            true,
            &oracle,
            &oracle,
            &std::collections::HashSet::new(),
            &Calibration::IDENTITY,
        );
        assert_eq!(
            c,
            Confidence::None,
            "sub-threshold oracle (nonsense) must not reach Strong"
        );
    }

    /// Embeddings confident NOTHING matches (oracle_top below veto floor) + only
    /// coincidental lexical coverage → abstain, not Weak. The "cat sat on the warm
    /// windowsill" leak (oracle_top 0.49).
    #[test]
    fn confidence_semantic_veto_abstains_on_coincidental_coverage() {
        let anchor = NodeId(7);
        let terms = vec![ResolvedTerm {
            token: "windowsill".into(),
            resolved: true,
            symbol_freq: 40, // specific (high idf) but not rare
            idf_w: 0.62,
            top_node: Some(anchor),
        }];
        // oracle present but far (top 0.49 < veto 0.55); anchor absent from oracle.
        let oracle: HashMap<NodeId, f32> = [(NodeId(99), 0.49)].into_iter().collect();
        let c = classify_confidence_lexical_fallback(
            &terms,
            0.5,
            12.0,
            true,
            true,
            &oracle,
            &oracle,
            &std::collections::HashSet::new(),
            &Calibration::IDENTITY,
        );
        assert_eq!(
            c,
            Confidence::None,
            "confidently-far oracle must veto coincidental lexical coverage"
        );
    }

    /// The veto exempts a rare exact anchor: a precise symbol the user literally
    /// typed is honoured (Weak) even when the model is weak on it.
    #[test]
    fn confidence_semantic_veto_exempts_rare_anchor() {
        let anchor = NodeId(7);
        let terms = vec![ResolvedTerm {
            token: "PaymentService".into(),
            resolved: true,
            symbol_freq: 2, // rare → exact anchor the user named
            idf_w: 0.93,
            top_node: Some(anchor),
        }];
        let oracle: HashMap<NodeId, f32> = [(NodeId(99), 0.49)].into_iter().collect();
        // Low coverage so it doesn't hit the Exact branch; must land on Weak, not None.
        let c = classify_confidence_lexical_fallback(
            &terms,
            0.3,
            0.1,
            true,
            true,
            &oracle,
            &oracle,
            &std::collections::HashSet::new(),
            &Calibration::IDENTITY,
        );
        assert_eq!(
            c,
            Confidence::Weak,
            "rare exact anchor must be exempt from the semantic veto"
        );
    }

    /// Strong lexical evidence (high coverage + BM25) survives a weak oracle —
    /// the veto only suppresses Weak-grade coincidences, never a real lexical hit.
    #[test]
    fn confidence_lexical_strong_survives_weak_oracle() {
        let terms = vec![ResolvedTerm {
            token: "ppr".into(),
            resolved: false, // no anchor — pure whole-query BM25 evidence
            symbol_freq: 50,
            idf_w: 0.0,
            top_node: None,
        }];
        // Oracle present but weak (0.50 < veto 0.55) — would veto if lexical were weak.
        let oracle: HashMap<NodeId, f32> = [(NodeId(99), 0.50)].into_iter().collect();
        let c = classify_confidence_lexical_fallback(
            &terms,
            0.8,
            2.0,
            true,
            true,
            &oracle,
            &oracle,
            &std::collections::HashSet::new(),
            &Calibration::IDENTITY,
        );
        assert_eq!(
            c,
            Confidence::Strong,
            "strong lexical evidence must beat the veto (lexical-Strong path is checked first)"
        );
    }

    // ── semantic_validate (Tier-1 cosine oracle) ──────────────────────────────

    fn seed(id: u64, source: SeedSource) -> Seed {
        Seed {
            node: NodeId(id),
            weight: 1.0,
            source,
            score: 1.0,
            rerank_score: None,
        }
    }

    #[test]
    fn semantic_validate_noop_when_oracle_empty() {
        // Embeddings off/degraded → oracle empty → lexical seeds untouched.
        let seeds = vec![seed(1, SeedSource::Lexical), seed(2, SeedSource::Exact)];
        let out = semantic_validate(seeds.clone(), &HashMap::new(), &Calibration::IDENTITY);
        assert_eq!(out.len(), 2, "empty oracle must be a no-op");
    }

    #[test]
    fn semantic_validate_noop_when_oracle_not_confident() {
        // Top cosine below ORACLE_MIN → embedding found nothing strong → keep all.
        let seeds = vec![seed(1, SeedSource::Lexical), seed(2, SeedSource::Knn)];
        let oracle: HashMap<NodeId, f32> = [(NodeId(2), 0.50)].into_iter().collect();
        let out = semantic_validate(seeds, &oracle, &Calibration::IDENTITY);
        assert_eq!(out.len(), 2, "weak oracle must not cut lexical seeds");
    }

    #[test]
    fn semantic_validate_cuts_salad_keeps_relevant() {
        // The confident-oracle case: NodeId(10..13) are genuine semantic seeds
        // (cosine ~0.68); NodeId(1..6) are spurious FTS anchors absent from the
        // oracle (cosine 0.0). Validation must drop the salad and keep the seeds.
        let mut seeds: Vec<Seed> = (1..=6).map(|i| seed(i, SeedSource::Exact)).collect();
        seeds.extend((10..=15).map(|i| seed(i, SeedSource::Knn)));
        let oracle: HashMap<NodeId, f32> = (10..=15)
            .map(|i| (NodeId(i), 0.68 - (i as f32 - 10.0) * 0.01))
            .collect();
        let out = semantic_validate(seeds, &oracle, &Calibration::IDENTITY);
        let kept: std::collections::HashSet<u64> = out.iter().map(|s| s.node.0).collect();
        assert!(
            kept.iter().all(|&id| id >= 10),
            "spurious FTS anchors (cosine 0) must be cut; got {kept:?}"
        );
        assert_eq!(out.len(), 6, "all 6 genuine semantic seeds must survive");
        // Reshuffled by cosine: highest-cosine seed first.
        assert_eq!(
            out[0].node,
            NodeId(10),
            "survivors reshuffled by cosine desc"
        );
    }

    #[test]
    fn semantic_validate_preserving_exact_keeps_exact_below_floor() {
        // RFC-021 / Issue D repro (hermetic). A moderately-common on-topic exact
        // anchor ("daemon"-style: resolves to real Daemon symbols but is not rare
        // enough for G1) that the DILUTED whole-query embedding leaves out of the
        // oracle (cosine 0.0) must NOT be dropped here — it has to reach the
        // reranker. Contrast `semantic_validate_cuts_salad_keeps_relevant`, which
        // calls the raw `semantic_validate` and drops the exact-sourced seeds.
        let mut seeds: Vec<Seed> = (1..=3).map(|i| seed(i, SeedSource::Exact)).collect();
        // KNN tail the diluted query DOES score (off-topic neighbours it drifted to):
        seeds.extend((10..=15).map(|i| seed(i, SeedSource::Knn)));
        let oracle: HashMap<NodeId, f32> = (10..=15)
            .map(|i| (NodeId(i), 0.68 - (i as f32 - 10.0) * 0.01))
            .collect();

        let out = semantic_validate_preserving_exact(seeds, &oracle, &Calibration::IDENTITY);
        let kept: std::collections::HashSet<u64> = out.iter().map(|s| s.node.0).collect();
        assert!(
            kept.contains(&1) && kept.contains(&2) && kept.contains(&3),
            "exact anchors (cosine 0, absent from oracle) must bypass the veto and \
             reach the reranker; got {kept:?}"
        );
    }

    #[test]
    fn semantic_validate_preserving_exact_still_cuts_lexical_tail() {
        // The exemption is exact-only: a coincidental LEXICAL anchor the confident
        // oracle disagrees with (cosine 0.0, absent) is still cut, so the reranker
        // isn't handed the whole spurious-collision tail.
        let mut seeds: Vec<Seed> = (1..=3).map(|i| seed(i, SeedSource::Lexical)).collect();
        seeds.extend((10..=15).map(|i| seed(i, SeedSource::Knn)));
        let oracle: HashMap<NodeId, f32> = (10..=15)
            .map(|i| (NodeId(i), 0.68 - (i as f32 - 10.0) * 0.01))
            .collect();

        let out = semantic_validate_preserving_exact(seeds, &oracle, &Calibration::IDENTITY);
        let kept: std::collections::HashSet<u64> = out.iter().map(|s| s.node.0).collect();
        assert!(
            kept.iter().all(|&id| id >= 10),
            "lexical anchors the embedding rejects must still be cut; got {kept:?}"
        );
        assert_eq!(out.len(), 6, "only the 6 KNN seeds survive");
    }

    #[test]
    fn semantic_validate_never_empties_grounded_result() {
        // Confident oracle but only 2 seeds clear the floor — MIN_KEEP guarantees
        // we still return SEMANTIC_MIN_KEEP rather than emptying the result.
        let mut seeds: Vec<Seed> = (1..=6).map(|i| seed(i, SeedSource::Lexical)).collect();
        seeds.push(seed(10, SeedSource::Knn));
        seeds.push(seed(11, SeedSource::Knn));
        let oracle: HashMap<NodeId, f32> = [
            (NodeId(10), 0.70),
            (NodeId(11), 0.69),
            (NodeId(1), 0.40),
            (NodeId(2), 0.35),
        ]
        .into_iter()
        .collect();
        let out = semantic_validate(seeds, &oracle, &Calibration::IDENTITY);
        assert_eq!(
            out.len(),
            SEMANTIC_MIN_KEEP,
            "must keep top-{SEMANTIC_MIN_KEEP} by cosine, never empty"
        );
    }

    // ── RFC-021 Phase 3: absolute-floor classify_confidence gate ─────────────

    #[test]
    fn confidence_rerank_above_strong_floor_is_strong() {
        let oracle: HashMap<NodeId, f32> = HashMap::new();
        let scored = std::collections::HashSet::new();
        let c = classify_confidence(
            &[],
            0.0,
            0.0,
            true,
            false,
            &oracle,
            &oracle,
            &scored,
            &Calibration::IDENTITY,
            false, // no exact anchor
            Some(0.9),
        );
        assert_eq!(c, Confidence::Strong);
    }

    #[test]
    fn confidence_rerank_between_floors_is_weak() {
        let oracle: HashMap<NodeId, f32> = HashMap::new();
        let scored = std::collections::HashSet::new();
        let c = classify_confidence(
            &[],
            0.0,
            0.0,
            true,
            false,
            &oracle,
            &oracle,
            &scored,
            &Calibration::IDENTITY,
            false,
            Some(0.65),
        );
        assert_eq!(c, Confidence::Weak);
    }

    /// The RFC-021 reproduction bug, at the unit level: the reranker ran and
    /// scored the best candidate low — must abstain, not surface a confident
    /// salad, regardless of how much lexical coverage the query happened to hit.
    #[test]
    fn confidence_rerank_below_weak_floor_abstains() {
        let oracle: HashMap<NodeId, f32> = HashMap::new();
        let scored = std::collections::HashSet::new();
        let c = classify_confidence(
            &[],
            0.0,
            0.0,
            true,
            false,
            &oracle,
            &oracle,
            &scored,
            &Calibration::IDENTITY,
            false,
            Some(0.1),
        );
        assert_eq!(
            c,
            Confidence::None,
            "a low rerank score must abstain even with seeds present"
        );
    }

    #[test]
    fn confidence_rerank_none_falls_back_to_lexical_gate() {
        // Reranker unavailable for this query (None) — must produce the exact
        // same decision as calling the fallback directly with identical inputs.
        let oracle: HashMap<NodeId, f32> = HashMap::new();
        let scored = std::collections::HashSet::new();
        let terms = [ResolvedTerm {
            token: "build_seed_set".into(),
            resolved: true,
            symbol_freq: 1,
            idf_w: 0.9,
            top_node: Some(NodeId(1)),
        }];
        let via_gate = classify_confidence(
            &terms,
            1.0,
            0.0,
            true,
            false,
            &oracle,
            &oracle,
            &scored,
            &Calibration::IDENTITY,
            false,
            None,
        );
        let via_fallback = classify_confidence_lexical_fallback(
            &terms,
            1.0,
            0.0,
            true,
            false,
            &oracle,
            &oracle,
            &scored,
            &Calibration::IDENTITY,
        );
        assert_eq!(via_gate, via_fallback);
        assert_eq!(via_gate, Confidence::Exact);
    }

    /// G1 (blocker): an exact-symbol query stays on the deterministic path even
    /// when the (NL-trained) reranker scores the exact match's own name low —
    /// the failure mode the RFC names explicitly (`GetWarningsForPod` scoring
    /// low against its own literal query). Trading salad for a false-abstain on
    /// a precise lookup is exactly what G1 forbids.
    #[test]
    fn confidence_g1_exact_anchor_bypasses_low_rerank_score() {
        let oracle: HashMap<NodeId, f32> = HashMap::new();
        let scored = std::collections::HashSet::new();
        let terms = [ResolvedTerm {
            token: "getwarningsforpod".into(),
            resolved: true,
            symbol_freq: 1,
            idf_w: 0.9,
            top_node: Some(NodeId(1)),
        }];
        let exact_anchor_ids: std::collections::HashSet<NodeId> = [NodeId(1)].into_iter().collect();
        let g1_bypass = compute_g1_bypass(&terms, &exact_anchor_ids);
        assert!(
            g1_bypass,
            "rare term tied to its own exact anchor must bypass"
        );
        let c = classify_confidence(
            &terms,
            1.0,
            0.0,
            true,
            false,
            &oracle,
            &oracle,
            &scored,
            &Calibration::IDENTITY,
            g1_bypass,
            Some(0.02), // reranker scored the exact match low — must be ignored
        );
        assert_eq!(
            c,
            Confidence::Exact,
            "exact-symbol queries must not be governed by the rerank floor"
        );
    }

    /// The fix for the RFC-021 reproduction bug at the G1-boundary level: an
    /// exact anchor resolves ("drop" really is `SqliteStore.drop`), and even
    /// measures decent coverage (3/5 in the real repro — dozens of types
    /// implement `Drop`, so "drop" and "delete" both resolve). A
    /// coverage-only gate was tried first and still let this exact query
    /// bypass to `Confidence::Strong` (Defect A reproduced through the G1
    /// escape hatch) — coverage alone can't tell "the user named a symbol"
    /// from "a common word coincidentally IS one." Rarity can: `drop` is
    /// anything but rare (`symbol_freq` far above `rare_anchor_max`), so
    /// `compute_g1_bypass` must return false here; the query falls through
    /// to the rerank floor, where a low score correctly abstains.
    #[test]
    fn confidence_g1_requires_a_rare_anchor_not_just_any_exact_anchor() {
        let oracle: HashMap<NodeId, f32> = HashMap::new();
        let scored = std::collections::HashSet::new();
        let terms = [ResolvedTerm {
            token: "drop".into(),
            resolved: true,
            symbol_freq: 50, // common — dozens of Drop impls, nowhere near rare
            idf_w: 0.6,
            top_node: Some(NodeId(1)),
        }];
        let exact_anchor_ids: std::collections::HashSet<NodeId> = [NodeId(1)].into_iter().collect();
        let g1_bypass = compute_g1_bypass(&terms, &exact_anchor_ids);
        assert!(!g1_bypass, "a common exact anchor must not bypass");
        let c = classify_confidence(
            &terms,
            0.6, // coverage alone clears coverage_strong — must not be enough on its own
            0.0,
            true,
            false,
            &oracle,
            &oracle,
            &scored,
            &Calibration::IDENTITY,
            g1_bypass,
            Some(0.02), // reranker correctly scores the sentence irrelevant
        );
        assert_eq!(
            c,
            Confidence::None,
            "a coincidental exact-anchor hit on a common word must not bypass the rerank floor"
        );
    }

    /// RFC-021 F3: a salad query that resolves an exact anchor on a *common*
    /// term but ALSO happens to resolve an unrelated *rare* term (whose own
    /// top match is not among the exact anchors) must not bypass G1 either —
    /// rarity has to be tied to the term that produced the anchor, not to any
    /// resolved term in the query.
    #[test]
    fn confidence_g1_rarity_must_be_tied_to_the_anchor_term_not_any_resolved_term() {
        let terms = [
            ResolvedTerm {
                token: "drop".into(),
                resolved: true,
                symbol_freq: 50, // common — this is the term that produced the exact anchor
                idf_w: 0.6,
                top_node: Some(NodeId(1)),
            },
            ResolvedTerm {
                token: "incidental".into(),
                resolved: true,
                symbol_freq: 1, // rare, but resolves to an unrelated node — not an exact anchor
                idf_w: 0.9,
                top_node: Some(NodeId(2)),
            },
        ];
        let exact_anchor_ids: std::collections::HashSet<NodeId> = [NodeId(1)].into_iter().collect();
        let g1_bypass = compute_g1_bypass(&terms, &exact_anchor_ids);
        assert!(
            !g1_bypass,
            "rarity on an unrelated resolved term must not launder a common anchor into a bypass"
        );

        let oracle: HashMap<NodeId, f32> = HashMap::new();
        let scored = std::collections::HashSet::new();
        let c = classify_confidence(
            &terms,
            0.6,
            0.0,
            true,
            false,
            &oracle,
            &oracle,
            &scored,
            &Calibration::IDENTITY,
            g1_bypass,
            Some(0.02),
        );
        assert_eq!(
            c,
            Confidence::None,
            "salad with an incidental rare term plus a common anchor must abstain, not bypass"
        );
    }

    /// G2: confidence must be identical whether embeddings ran or not, for the
    /// same query — the reranker's floor gate never looks at knn_oracle /
    /// anchor_oracle at all (only classify_confidence_lexical_fallback does,
    /// and it's reached identically in both cases here since rerank_score is
    /// held constant).
    #[test]
    fn confidence_g2_identical_embed_on_vs_off_for_same_rerank_score() {
        let empty: HashMap<NodeId, f32> = HashMap::new();
        let warm: HashMap<NodeId, f32> = [(NodeId(1), 0.95)].into_iter().collect();
        let scored = std::collections::HashSet::new();
        let with_embed_off = classify_confidence(
            &[],
            0.0,
            0.0,
            true,
            false,
            &empty,
            &empty,
            &scored,
            &Calibration::IDENTITY,
            false,
            Some(0.85),
        );
        let with_embed_on = classify_confidence(
            &[],
            0.0,
            0.0,
            true,
            true,
            &warm,
            &warm,
            &scored,
            &Calibration::IDENTITY,
            false,
            Some(0.85),
        );
        assert_eq!(
            with_embed_off, with_embed_on,
            "confidence must not depend on embedding presence when the reranker has an opinion"
        );
    }

    /// RFC-021 F4: a reranked-but-rejected seed (model scored it near zero —
    /// judged garbage) must not outrank a seed the model never saw (rank 31+,
    /// still carries whatever RRF standing it had). Only model-approved seeds
    /// (>= weak floor) may outrank the unjudged tail.
    #[test]
    fn sort_seeds_post_rerank_unjudged_tail_outranks_judged_garbage() {
        let weak = rerank_weak_floor();
        let mut approved = seed(1, SeedSource::Lexical);
        approved.rerank_score = Some(0.9);
        let mut rejected = seed(2, SeedSource::Lexical);
        rejected.rerank_score = Some(0.001);
        let unjudged = seed(3, SeedSource::Lexical); // rerank_score: None (tail)

        let mut seeds = vec![rejected.clone(), unjudged.clone(), approved.clone()];
        sort_seeds_post_rerank(&mut seeds, weak);

        let order: Vec<u64> = seeds.iter().map(|s| s.node.0).collect();
        assert_eq!(
            order,
            vec![1, 3, 2],
            "approved (band 1) > unjudged tail (band 2) > judged garbage (band 3)"
        );
    }

    /// Regression test for a real repro found live on the travsr repo: "how
    /// does the daemon watch for file changes" abstained because
    /// `handle_watch_event` never made it into the anchor set. Root cause:
    /// `anchor_path_counts` is one counter shared across every token in the
    /// query — the earlier-processed "daemon" token filled
    /// `crates/travsr-daemon/src/lib.rs`'s 2-slot budget before "watch" was
    /// processed, so its perfectly valid match (`handle_watch_event`) was
    /// dropped purely by token order, not relevance. A token that resolves a
    /// genuine, all-checks-passed match must always get at least one anchor
    /// slot, even when an earlier token already exhausted the shared
    /// per-path budget.
    #[test]
    fn build_seed_set_later_token_not_starved_by_earlier_tokens_path_cap() {
        use travsr_core::{Node, VName};
        let mut store = SqliteStore::open_in_memory().unwrap();

        // Two "daemon" matches in the same file exhaust MAX_SEEDS_PER_PATH (2)
        // before the "watch" token is processed.
        let path = "crates/travsr-daemon/src/lib.rs";
        let daemon_new = Node::new(
            VName::new("corpus", "", path, "rust", "fn:daemon_new"),
            "function",
        );
        let daemon_run = Node::new(
            VName::new("corpus", "", path, "rust", "fn:daemon_run"),
            "function",
        );
        let handle_watch = Node::new(
            VName::new("corpus", "", path, "rust", "fn:handle_watch_event"),
            "function",
        );
        store.put_node(&daemon_new).unwrap();
        store.put_node(&daemon_run).unwrap();
        store.put_node(&handle_watch).unwrap();

        let seed_set = build_seed_set(
            &store,
            "daemon watch",
            &travsr_retrieval::OpenFilter,
            vec![],
            &HashMap::new(),
            None,
        );

        let seed_ids: std::collections::HashSet<NodeId> =
            seed_set.seeds.iter().map(|s| s.node).collect();
        assert!(
            seed_ids.contains(&handle_watch.id),
            "later token's genuine match must not be starved by an earlier \
             token filling the shared path budget; seeds present: {:?}",
            seed_set.seeds.iter().map(|s| s.node).collect::<Vec<_>>()
        );
    }

    /// Regression test for a second, related repro found live: "give me data
    /// about daemon" abstained even though "daemon" alone resolves cleanly
    /// to real, well-connected content. Root cause: `anchor_raw` fed
    /// `rrf_fuse` in raw token-processing order, not weight order —
    /// `rrf_fuse`'s own documented precondition is that each source is
    /// sorted descending by score (position 0 = best), since it scores by
    /// list position. "data" (common, many matches) is processed before
    /// "daemon" (rare, one specific match) purely because it appears earlier
    /// in the query, so daemon's higher-weight match landed at a worse RRF
    /// rank than data's lower-weight ones despite being more relevant.
    #[test]
    fn build_seed_set_anchor_raw_sorted_by_weight_before_rrf_fusion() {
        use travsr_core::{Node, VName};
        let mut store = SqliteStore::open_in_memory().unwrap();

        // "data": common, several lower-weight matches, each in its own path.
        for i in 0..3 {
            let n = Node::new(
                VName::new(
                    "corpus",
                    "",
                    format!("crates/x/data_{i}.rs"),
                    "rust",
                    format!("fn:data_helper_{i}"),
                ),
                "function",
            );
            store.put_node(&n).unwrap();
        }
        // "daemon": rare, one specific, higher-weight match (lower symbol_freq
        // means higher idf_w; same kind as the data matches so kind_boost is
        // not a confounding variable).
        let daemon = Node::new(
            VName::new(
                "corpus",
                "",
                "crates/travsr-daemon/src/lib.rs",
                "rust",
                "fn:daemon_run",
            ),
            "function",
        );
        store.put_node(&daemon).unwrap();

        // "data" appears before "daemon" in the query, so it is processed
        // first in the per-token anchor loop.
        let seed_set = build_seed_set(
            &store,
            "data daemon",
            &travsr_retrieval::OpenFilter,
            vec![],
            &HashMap::new(),
            None,
        );

        assert!(!seed_set.seeds.is_empty(), "expected at least one seed");
        assert_eq!(
            seed_set.seeds[0].node,
            daemon.id,
            "the rarer, higher-weight match must rank first regardless of \
             which token resolved it earlier in query order; seeds: {:?}",
            seed_set.seeds.iter().map(|s| s.node).collect::<Vec<_>>()
        );
    }

    /// Regression test for a third repro found live: "give me data about
    /// daemon" abstained even after the RRF-sort fix. Root cause:
    /// `semantic_validate` ran BEFORE `g1_bypass` was computed and dropped
    /// the exact, rare "daemon" match because its cosine to the noisy,
    /// filler-word-heavy whole-query embedding was lower than several
    /// unrelated seeds' — exactly the failure mode G1 exists to prevent, but
    /// G1 was only ever consulted afterward, too late to help. Fixed by
    /// computing `g1_bypass` from `anchor_raw` right after it is built, and
    /// skipping `semantic_validate` entirely when it is true.
    #[test]
    fn build_seed_set_g1_bypass_protects_exact_match_from_semantic_validate() {
        use travsr_core::{Node, VName};
        let mut store = SqliteStore::open_in_memory().unwrap();

        // Five unrelated exact matches, all scored high by the (simulated)
        // embed oracle - enough to make it "confident" and, pre-fix, to push
        // the rare daemon match out of semantic_validate's top-N cosine cut.
        let fillers = ["alpha", "beta", "gamma", "delta", "epsilon"];
        let mut oracle: HashMap<NodeId, f32> = HashMap::new();
        for (i, f) in fillers.iter().enumerate() {
            let n = Node::new(
                VName::new(
                    "corpus",
                    "",
                    format!("crates/x/{f}_{i}.rs"),
                    "rust",
                    format!("fn:{f}_helper"),
                ),
                "function",
            );
            store.put_node(&n).unwrap();
            oracle.insert(n.id, 0.85);
        }
        // "daemon": rare, genuine match, deliberately absent from the oracle
        // (cosine treated as 0.0) - simulates the live repro where the exact
        // match's own cosine to the whole conversational query scored low.
        let daemon = Node::new(
            VName::new(
                "corpus",
                "",
                "crates/travsr-daemon/src/lib.rs",
                "rust",
                "fn:daemon_run",
            ),
            "function",
        );
        store.put_node(&daemon).unwrap();

        let seed_set = build_seed_set(
            &store,
            "alpha beta gamma delta epsilon daemon",
            &travsr_retrieval::OpenFilter,
            vec![],
            &oracle,
            None,
        );

        assert!(
            seed_set.seeds.iter().any(|s| s.node == daemon.id),
            "G1's rare exact match must survive semantic_validate, not be cut \
             by a low cosine to the noisy whole-query embedding; seeds: {:?}",
            seed_set
                .seeds
                .iter()
                .map(|s| (s.node, s.source))
                .collect::<Vec<_>>()
        );
    }

    // ── RFC-021 Phase 0: rerank floor-sweep (dev tool, not a CI test) ─────────
    //
    // Not run in CI: needs both a real cross-encoder model AND a real indexed
    // graph.db. By default it self-dogfoods against travsr's own codebase (the
    // labeled queries compiled in from bench/queries.json). Gated on two env
    // vars, skips gracefully when either is absent.
    //
    // Run (travsr self-corpus):
    //   TRAVSR_RERANK_MODEL_DIR=<dir> TRAVSR_FLOOR_SWEEP_DB=<path/to/.travsr/graph.db> \
    //     cargo test -p travsr-mcp --lib rerank_floor_sweep -- --nocapture --ignored
    //
    // Run (cross-repo, e.g. kubernetes — RFC-021 §9.3 Task 4 floor-generalization
    // gate): additionally point the query set at that repo's labeled file:
    //   TRAVSR_FLOOR_SWEEP_QUERIES=bench/queries-k8s.json \
    //   TRAVSR_FLOOR_SWEEP_DB=/path/to/kubernetes/.travsr/graph.db  (+ model dir)
    #[test]
    #[ignore]
    fn rerank_floor_sweep() {
        let Some(db_path) = std::env::var_os("TRAVSR_FLOOR_SWEEP_DB") else {
            eprintln!("skipping: TRAVSR_FLOOR_SWEEP_DB not set");
            return;
        };
        if std::env::var_os("TRAVSR_RERANK_MODEL_DIR").is_none() {
            eprintln!("skipping: TRAVSR_RERANK_MODEL_DIR not set");
            return;
        }

        #[derive(serde::Deserialize)]
        struct LabeledQuery {
            id: String,
            category: String,
            query: String,
        }
        #[derive(serde::Deserialize)]
        struct QueryFile {
            queries: Vec<LabeledQuery>,
        }
        // Query set: compiled-in travsr self-corpus by default; override with
        // TRAVSR_FLOOR_SWEEP_QUERIES=<path> to calibrate against another repo's
        // labeled set (e.g. bench/queries-k8s.json) — RFC-021 §9.3 Task 3, the
        // cross-repo floor-generalization gate. The file must have the same
        // {queries:[{id,category,query}]} shape and the same positive/negative
        // category vocabulary (literal|conceptual|cross vs nonsense|salad).
        let raw: String = match std::env::var_os("TRAVSR_FLOOR_SWEEP_QUERIES") {
            Some(path) => std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read TRAVSR_FLOOR_SWEEP_QUERIES {path:?}: {e}")),
            None => include_str!("../../../bench/queries.json").to_string(),
        };
        let parsed: QueryFile =
            serde_json::from_str(&raw).expect("floor-sweep query file must parse");

        let store = SqliteStore::open(std::path::Path::new(&db_path)).expect("open graph.db");

        // No KNN, no score_fn: the floor is measured on the reranker's own
        // signal, independent of embeddings — matching G2 (confidence must be
        // embed-on/off identical), so the calibration shouldn't secretly
        // depend on whether embeddings happen to be indexed for this repo.
        let mut scored: Vec<(String, String, f32)> = Vec::new(); // (id, category, max_rerank_score)
        for q in &parsed.queries {
            let seed_set = build_seed_set(
                &store,
                &q.query,
                &travsr_retrieval::OpenFilter,
                vec![],
                &HashMap::new(),
                None,
            );
            let max_score = seed_set
                .seeds
                .iter()
                .filter_map(|s| s.rerank_score)
                .fold(0.0_f32, f32::max);
            scored.push((q.id.clone(), q.category.clone(), max_score));
            eprintln!(
                "{:<4} {:<10} max_rerank_score={:.4}",
                q.id, q.category, max_score
            );
        }

        let is_positive = |cat: &str| matches!(cat, "literal" | "conceptual" | "cross");
        let is_negative = |cat: &str| matches!(cat, "nonsense" | "salad");
        let n_pos = scored.iter().filter(|(_, c, _)| is_positive(c)).count();
        let n_neg = scored.iter().filter(|(_, c, _)| is_negative(c)).count();

        eprintln!(
            "\nfloor sweep (N={}: {n_pos} positive, {n_neg} negative)",
            scored.len()
        );
        eprintln!("floor | negative_abstention | positive_recall");
        let mut best_floor: f32 = 0.0;
        let mut steps = 0;
        while steps <= 20 {
            let floor = steps as f32 * 0.05;
            steps += 1;
            let neg_abstained = scored
                .iter()
                .filter(|(_, c, s)| is_negative(c) && *s < floor)
                .count();
            let pos_retained = scored
                .iter()
                .filter(|(_, c, s)| is_positive(c) && *s >= floor)
                .count();
            let neg_abstention = neg_abstained as f32 / n_neg.max(1) as f32;
            let pos_recall = pos_retained as f32 / n_pos.max(1) as f32;
            eprintln!("{floor:.2}  | {neg_abstention:.3}                | {pos_recall:.3}");
            // Largest floor that still honors the recall gate — negative_abstention
            // is monotonically non-decreasing in floor, so this is the ROC-optimal pick.
            if pos_recall >= 0.90 {
                best_floor = floor;
            }
        }
        eprintln!("\nrecommended WEAK_FLOOR (>=90% positive recall): {best_floor:.2}");

        // STRONG_FLOOR heuristic: median score among positives, NOT hit-verified
        // (a coarser anchor than WEAK_FLOOR — see travsr-qa-engineer note in the
        // RFC-021 Phase 0 discussion: this dataset only labels answerable/nonsense,
        // not graded strong/weak, so this half of the pair is a starting heuristic).
        let mut pos_scores: Vec<f32> = scored
            .iter()
            .filter(|(_, c, _)| is_positive(c))
            .map(|(_, _, s)| *s)
            .collect();
        pos_scores.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = pos_scores.get(pos_scores.len() / 2).copied().unwrap_or(0.0);
        eprintln!("recommended STRONG_FLOOR (heuristic, median of positive scores): {median:.2}");
    }
}
