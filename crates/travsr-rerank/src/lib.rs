//! RFC-021: in-process cross-encoder relevance arbiter.
//!
//! Pure inference — no panic guard, no warm-up. Those are call-site
//! (travsr-mcp) concerns; this crate stays trivially unit-testable and
//! reusable by a future sidecar variant behind the same [`Reranker`] trait.
//! It owns two things beyond raw inference: splitting a batch across
//! rayon's thread pool, since `tract`'s `SimplePlan` does not itself
//! parallelize across the batch dimension (measured — see [`TractReranker::rerank_batch`]);
//! and capping each candidate's text to [`MAX_CANDIDATE_CHARS`] so per-call
//! cost stays bounded and repo-agnostic regardless of what a caller passes
//! in — deliberately enforced here rather than at each call site, so a
//! future caller (e.g. Phase 4's `travsr ask` parity) can't reintroduce the
//! same unbounded-input cost blowup.

use std::path::Path;

use anyhow::{Context, Result};
use rayon::prelude::*;
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams, TruncationStrategy};
use tract_onnx::prelude::*;

/// Candidate text is truncated to this many tokens (signature-first at the
/// call site; `OnlySecond` truncation here drops from the candidate side,
/// never the query).
const MAX_SEQ_LEN: usize = 256;
const MODEL_FILE: &str = "model_fp16.onnx";
const TOKENIZER_FILE: &str = "tokenizer.json";

/// Hard char-level cap on a candidate's contribution to the tokenizer input,
/// applied before pairing with the query. Deliberately NOT env-tunable: an
/// operator raising it to "improve recall" would silently reintroduce the
/// exact repo-size-dependent cost blowup it exists to prevent.
///
/// Measured (RFC-021 Phase 3 E2E, 2026-07-18): the same K=30 rerank took
/// ~353ms on travsr's own compact Rust functions but 2.5-9s on
/// kubernetes/kubernetes for an identical call. Root cause: candidate text
/// is `signature + snippet_for_node(..)` (up to 40 raw source lines,
/// travsr-analysis's `snippet_line_cap`), and 40 lines of dense Go is far
/// more tokens than 40 lines of idiomatic Rust — routinely exceeding
/// `MAX_SEQ_LEN` on k8s, rarely on travsr. Because tokenization uses
/// `PaddingStrategy::BatchLongest`, one over-length candidate in a batch
/// pads every OTHER candidate in that batch up to the same length, so a
/// single verbose function inflates the whole chunk's cost — not an
/// occasional outlier, a systemic effect once any candidate in a chunk
/// crosses the truncation ceiling.
///
/// The fix bounds the input instead of tolerating the output cost: this
/// crate's model (`ms-marco-MiniLM-L-6-v2`) is a general passage-relevance
/// model trained on ~50-80 word (MS MARCO) passages, never meant to consume
/// raw function bodies — and RFC-021's job is a coarse "is this even
/// topically related" triage, not code comprehension, so the signature plus
/// a handful of body lines carries the signal; more text mostly adds
/// boilerplate that dilutes a non-code-aware model. 480 chars ≈ 120 tokens
/// at ~4 chars/token, leaving headroom under `MAX_SEQ_LEN` for the query +
/// special tokens regardless of the source repo's language or function
/// size — the tokenizer's own `OnlySecond`/`max_length` truncation remains
/// the exact backstop for dense-token edge cases (CJK, heavily-escaped
/// strings) this char-level cut doesn't catch.
const MAX_CANDIDATE_CHARS: usize = 480;

/// Truncates `text` to at most `max_chars` chars on a UTF-8-safe boundary.
/// Pure and total — never panics on multi-byte input, unlike a naive byte
/// slice.
fn truncate_candidate(text: &str, max_chars: usize) -> &str {
    match text.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => &text[..byte_idx],
        None => text,
    }
}

/// Scores `candidates` against `query`. Implementations must be
/// deterministic (same input -> identical output) so "same query, same
/// result" holds. The only v1 implementation is [`TractReranker`]; the
/// trait exists so a future sidecar/GPU variant (travsr-embed#8) can be
/// swapped in without touching call sites.
pub trait Reranker: Send + Sync {
    /// Returns one relevance score in `[0, 1]` per candidate, aligned by
    /// index to `candidates`. Empty input returns an empty vec.
    fn rerank(&self, query: &str, candidates: &[&str]) -> Result<Vec<f32>>;
}

type Plan = TypedRunnableModel<TypedModel>;

/// In-process CPU cross-encoder (`tract`), fp16 weights. Deterministic:
/// fixed accumulation order on CPU means identical input always produces
/// identical output bytes.
pub struct TractReranker {
    plan: Plan,
    tokenizer: Tokenizer,
}

impl TractReranker {
    /// Loads `model_fp16.onnx` + `tokenizer.json` from `model_dir`. Both
    /// files are release-time assets (see RFC-021 §Phase 5) and are not
    /// vendored in this repo.
    pub fn load(model_dir: impl AsRef<Path>) -> Result<Self> {
        let dir = model_dir.as_ref();

        let model_path = dir.join(MODEL_FILE);
        let plan = tract_onnx::onnx()
            .model_for_path(&model_path)
            .with_context(|| format!("loading ONNX graph from {}", model_path.display()))?
            .into_optimized()
            .context("optimizing reranker graph")?
            .into_runnable()
            .context("compiling reranker graph into a runnable plan")?;

        let tokenizer_path = dir.join(TOKENIZER_FILE);
        let mut tokenizer = Tokenizer::from_file(&tokenizer_path).map_err(|e| {
            anyhow::anyhow!("loading tokenizer from {}: {e}", tokenizer_path.display())
        })?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: MAX_SEQ_LEN,
                strategy: TruncationStrategy::OnlySecond,
                ..Default::default()
            }))
            .map_err(|e| anyhow::anyhow!("configuring truncation: {e}"))?;
        tokenizer.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            pad_id: 0,
            pad_type_id: 0,
            pad_token: "[PAD]".to_string(),
            ..Default::default()
        }));

        Ok(Self { plan, tokenizer })
    }
}

impl TractReranker {
    /// One forward pass over `candidates` as a single batch. Measured: `tract`'s
    /// `SimplePlan` does not itself parallelize across the batch dimension —
    /// K=1 took ~34ms and K=30 took ~983ms, perfectly linear — so [`Reranker::rerank`]
    /// splits larger batches across threads and calls this per-chunk.
    fn rerank_batch(&self, query: &str, candidates: &[&str]) -> Result<Vec<f32>> {
        let pairs: Vec<(String, String)> = candidates
            .iter()
            .map(|candidate| {
                (
                    query.to_string(),
                    truncate_candidate(candidate, MAX_CANDIDATE_CHARS).to_string(),
                )
            })
            .collect();
        let encodings = self
            .tokenizer
            .encode_batch(pairs, true)
            .map_err(|e| anyhow::anyhow!("tokenizing candidate batch: {e}"))?;

        let batch = encodings.len();
        let seq_len = encodings
            .iter()
            .map(|e| e.get_ids().len())
            .max()
            .unwrap_or(0);

        let mut ids_flat = vec![0i64; batch * seq_len];
        let mut mask_flat = vec![0i64; batch * seq_len];
        let mut type_flat = vec![0i64; batch * seq_len];
        for (row, encoding) in encodings.iter().enumerate() {
            let base = row * seq_len;
            for (col, &v) in encoding.get_ids().iter().enumerate() {
                ids_flat[base + col] = i64::from(v);
            }
            for (col, &v) in encoding.get_attention_mask().iter().enumerate() {
                mask_flat[base + col] = i64::from(v);
            }
            for (col, &v) in encoding.get_type_ids().iter().enumerate() {
                type_flat[base + col] = i64::from(v);
            }
        }

        let shape = [batch, seq_len];
        let input_ids = Tensor::from_shape(&shape, &ids_flat)?;
        let attention_mask = Tensor::from_shape(&shape, &mask_flat)?;
        let token_type_ids = Tensor::from_shape(&shape, &type_flat)?;

        let outputs = self
            .plan
            .run(tvec![
                input_ids.into(),
                attention_mask.into(),
                token_type_ids.into()
            ])
            .context("running reranker forward pass")?;

        // The fp16 graph's classifier head casts its output back to F16 even
        // under keep_io_types (an onnxconverter_common quirk for op-blocked
        // nodes); read whichever dtype the graph actually produced rather
        // than assuming F32.
        let logits: Vec<f32> = match outputs[0].datum_type() {
            DatumType::F16 => outputs[0]
                .to_array_view::<f16>()
                .context("reading reranker logits (f16)")?
                .iter()
                .map(|v| v.to_f32())
                .collect(),
            _ => outputs[0]
                .to_array_view::<f32>()
                .context("reading reranker logits (f32)")?
                .iter()
                .copied()
                .collect(),
        };

        Ok(logits.into_iter().map(sigmoid).collect())
    }
}

impl Reranker for TractReranker {
    fn rerank(&self, query: &str, candidates: &[&str]) -> Result<Vec<f32>> {
        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        // Chunking overhead isn't worth it for a handful of candidates, or
        // when rayon's global pool has nothing to parallelize onto.
        let n_threads = rayon::current_num_threads().max(1);
        if candidates.len() <= 4 || n_threads <= 1 {
            return self.rerank_batch(query, candidates);
        }

        let chunk_size = candidates.len().div_ceil(n_threads).max(1);
        let scored: Result<Vec<Vec<f32>>> = candidates
            .chunks(chunk_size)
            .collect::<Vec<_>>()
            .into_par_iter()
            .map(|chunk| self.rerank_batch(query, chunk))
            .collect();
        Ok(scored?.into_iter().flatten().collect())
    }
}

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn model_dir_from_env() -> Option<std::path::PathBuf> {
        let dir = env::var_os("TRAVSR_RERANK_TEST_MODEL_DIR")?;
        let dir = std::path::PathBuf::from(dir);
        if dir.join(MODEL_FILE).is_file() && dir.join(TOKENIZER_FILE).is_file() {
            Some(dir)
        } else {
            None
        }
    }

    /// Pure logic, no model required: proves the repo-agnostic cost bound
    /// holds for an arbitrarily large candidate — e.g. a huge generated Go
    /// switch statement or minified JS bundle, not just the k8s snippets
    /// that surfaced the original blowup.
    #[test]
    fn truncate_candidate_bounds_arbitrarily_large_input() {
        let huge = "x".repeat(1_000_000);
        let truncated = truncate_candidate(&huge, MAX_CANDIDATE_CHARS);
        assert_eq!(truncated.chars().count(), MAX_CANDIDATE_CHARS);
    }

    #[test]
    fn truncate_candidate_leaves_short_text_unchanged() {
        let short = "fn:foo does a thing";
        assert_eq!(truncate_candidate(short, MAX_CANDIDATE_CHARS), short);
    }

    #[test]
    fn truncate_candidate_is_utf8_safe_on_multibyte_boundary() {
        // Each 'あ' is 3 bytes; a naive byte-index slice at a char-count
        // boundary would land mid-codepoint and panic. Multi-byte source
        // comments/identifiers are common enough (Japanese, emoji in doc
        // comments) that this must never panic regardless of cap value.
        let text = "あ".repeat(1000);
        let truncated = truncate_candidate(&text, 7);
        assert_eq!(truncated.chars().count(), 7);
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    /// Model + tokenizer assets are release-time artifacts (RFC-021 Phase
    /// 5), not vendored in git. These tests only run when a developer/CI
    /// job points `TRAVSR_RERANK_TEST_MODEL_DIR` at a directory containing
    /// them; otherwise they no-op rather than failing the build.
    macro_rules! require_model {
        () => {
            match model_dir_from_env() {
                Some(dir) => dir,
                None => {
                    eprintln!(
                        "skipping: TRAVSR_RERANK_TEST_MODEL_DIR not set or missing {MODEL_FILE}/{TOKENIZER_FILE}"
                    );
                    return;
                }
            }
        };
    }

    #[test]
    fn empty_candidates_returns_empty_vec_without_a_model() {
        struct AlwaysPanics;
        impl Reranker for AlwaysPanics {
            fn rerank(&self, _query: &str, candidates: &[&str]) -> Result<Vec<f32>> {
                assert!(candidates.is_empty());
                Ok(Vec::new())
            }
        }
        let scores = AlwaysPanics.rerank("anything", &[]).unwrap();
        assert!(scores.is_empty());
    }

    #[test]
    fn determinism_identical_bytes_across_two_runs() {
        let dir = require_model!();
        let reranker = TractReranker::load(&dir).expect("load reranker");
        let query = "how is the knapsack budget optimizer implemented";
        let candidates = [
            "fn:knapsack_budget_optimizer computes a 0-1 knapsack over token budget",
            "fn:Guard.drop implements Rust Drop for a lock guard",
        ];

        let first = reranker.rerank(query, &candidates).unwrap();
        let second = reranker.rerank(query, &candidates).unwrap();

        assert_eq!(
            first, second,
            "identical input must produce bit-identical output"
        );
    }

    #[test]
    fn relevant_pair_scores_above_irrelevant_pair() {
        let dir = require_model!();
        let reranker = TractReranker::load(&dir).expect("load reranker");
        let query = "delete all user accounts and drop the database";
        let candidates = [
            "fn:SqliteStore.delete_file removes a single indexed file's nodes and edges from the graph store",
            "fn:knapsack_budget_optimizer computes a 0-1 knapsack over token budget for context assembly",
        ];

        let scores = reranker.rerank(query, &candidates).unwrap();
        assert!(
            scores[0] > scores[1],
            "expected candidate 0 (destructive-op-adjacent) to score above an unrelated optimizer function, got {scores:?}"
        );
    }

    #[test]
    fn batch_order_is_preserved() {
        let dir = require_model!();
        let reranker = TractReranker::load(&dir).expect("load reranker");
        let query = "graph traversal";
        let candidates = [
            "fn:bfs_context walks the graph breadth-first",
            "fn:unrelated_noop does nothing",
        ];

        let batched = reranker.rerank(query, &candidates).unwrap();
        let solo_a = reranker.rerank(query, &candidates[..1]).unwrap();
        let solo_b = reranker.rerank(query, &candidates[1..]).unwrap();

        assert_eq!(batched.len(), 2);
        assert!(
            (batched[0] - solo_a[0]).abs() < 1e-4,
            "batching must not change candidate 0's score: {} vs {}",
            batched[0],
            solo_a[0]
        );
        assert!(
            (batched[1] - solo_b[0]).abs() < 1e-4,
            "batching must not change candidate 1's score: {} vs {}",
            batched[1],
            solo_b[0]
        );
    }
}
