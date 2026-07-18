//! RFC-021 Phase 2: safe call-site wrapper around `travsr-rerank`.
//!
//! Everything here degrades to `None` ("no opinion") rather than propagating
//! an error: a missing model, a load failure, an inference panic, or a
//! slow pass must never crash the daemon or block a query — the caller
//! (`seed::build_seed_set`) treats `None` as "leave ordering/confidence
//! untouched", which is what makes Phase 2 safe to ship dark.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Instant;

use travsr_rerank::{Reranker, TractReranker};

/// Escape hatch: forces the lexical-only fallback regardless of whether a
/// model is configured. Distinct from "no model configured" so operators can
/// disable the reranker on an otherwise-bundled install (minimal installs,
/// Phase 5).
fn rerank_disabled() -> bool {
    std::env::var("TRAVSR_NO_RERANK")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Candidates reranked per query. Bounds the forward-pass cost — the plan's
/// K ≈ 20-40 window; default matches the RFC.
pub(crate) fn rerank_topk() -> usize {
    std::env::var("TRAVSR_RERANK_TOPK")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&x: &usize| x > 0)
        .unwrap_or(30)
}

/// Circuit-breaker budget. Mirrors `knn_budget_ms` (tools.rs): measured
/// *after* the call completes and the result discarded if over budget, rather
/// than a preemptive timeout — a CPU-bound forward pass can't be safely
/// aborted mid-flight without unsafe thread termination, and this is the same
/// tradeoff the existing KNN breaker already makes.
///
/// Default 1200 — this is a backstop, not the primary defense against repo
/// size. The primary defense is `travsr-rerank::MAX_CANDIDATE_CHARS`, which
/// bounds each candidate's tokenizer input so real cost stays roughly
/// repo-agnostic (RFC-021 Phase 3 E2E, 2026-07-18): before that fix, K=30 on
/// kubernetes/kubernetes measured 2.5-9s (vs ~353ms on travsr's own compact
/// Rust fns) because 40-line Go snippets routinely blew past the tokenizer's
/// truncation ceiling, and `PaddingStrategy::BatchLongest` then padded every
/// candidate in a batch up to the longest one. After the input-size fix,
/// the same K=30 on k8s measured 700-950ms — much closer to travsr's own
/// number, but a real repo/hardware can still occasionally exceed 600ms
/// (measured: 7/27 queries), so 1200 keeps headroom over the current
/// post-fix worst case without going back to tolerating unbounded cost.
fn rerank_budget_ms() -> u128 {
    std::env::var("TRAVSR_RERANK_BUDGET_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&x: &u128| x > 0)
        .unwrap_or(1200)
}

/// Directory containing `model_fp16.onnx` + `tokenizer.json`. Phase 5 bundles
/// this next to the platform binary; until then the reranker is opt-in via
/// this env var (unset → `reranker()` stays `None`, fail-open).
fn rerank_model_dir() -> Option<PathBuf> {
    std::env::var_os("TRAVSR_RERANK_MODEL_DIR").map(PathBuf::from)
}

/// `None` once load has been attempted (missing config, missing files, or a
/// load-time error) — cached permanently so a broken install doesn't retry an
/// expensive failed load on every query.
static RERANKER: OnceLock<Option<TractReranker>> = OnceLock::new();
static WARM_STARTED: std::sync::Once = std::sync::Once::new();

fn reranker() -> Option<&'static TractReranker> {
    if rerank_disabled() {
        return None;
    }
    RERANKER
        .get_or_init(|| {
            let dir = rerank_model_dir()?;
            match TractReranker::load(&dir) {
                Ok(r) => {
                    tracing::info!(model_dir = %dir.display(), "RFC-021 reranker loaded");
                    Some(r)
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        model_dir = %dir.display(),
                        "RFC-021 reranker load failed — falling back to lexical gate"
                    );
                    None
                }
            }
        })
        .as_ref()
}

/// Spawn a background thread that loads the reranker eagerly, so the first
/// real query doesn't pay the (model-load) cold-start cost. Idempotent and
/// non-blocking — safe to call from every server entrypoint (stdio,
/// stdio-global, SSE); only the first call actually spawns anything.
pub(crate) fn warm_background() {
    WARM_STARTED.call_once(|| {
        std::thread::Builder::new()
            .name("rerank-warm".into())
            .spawn(|| {
                reranker();
            })
            .ok();
    });
}

/// Score `candidates` against `query`. Returns `None` — "no opinion" — when
/// the reranker is absent, disabled, panicked, errored, or ran over budget;
/// the caller must leave ordering/confidence untouched in that case. Never
/// panics itself. `Some(scores)` is always the same length as `candidates`,
/// aligned by index.
pub(crate) fn rerank(query: &str, candidates: &[&str]) -> Option<Vec<f32>> {
    let reranker = reranker()?;
    if candidates.is_empty() {
        return Some(Vec::new());
    }

    let start = Instant::now();
    let outcome = catch_unwind(AssertUnwindSafe(|| reranker.rerank(query, candidates)));
    let elapsed_ms = start.elapsed().as_millis();

    let scores = match outcome {
        Ok(Ok(scores)) => scores,
        Ok(Err(error)) => {
            tracing::warn!(%error, "rerank inference failed — falling back to lexical gate");
            return None;
        }
        Err(_) => {
            tracing::warn!("rerank inference panicked — falling back to lexical gate");
            return None;
        }
    };

    let budget = rerank_budget_ms();
    if elapsed_ms > budget {
        tracing::warn!(
            elapsed_ms,
            threshold_ms = budget,
            "rerank exceeded circuit-breaker threshold — discarding scores, falling back to lexical gate"
        );
        return None;
    }
    Some(scores)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rerank_topk_default_is_thirty() {
        std::env::remove_var("TRAVSR_RERANK_TOPK");
        assert_eq!(rerank_topk(), 30);
    }

    #[test]
    fn rerank_budget_default_is_1200ms() {
        std::env::remove_var("TRAVSR_RERANK_BUDGET_MS");
        assert_eq!(rerank_budget_ms(), 1200);
    }

    #[test]
    fn no_model_configured_is_none_not_panic() {
        std::env::remove_var("TRAVSR_NO_RERANK");
        std::env::remove_var("TRAVSR_RERANK_MODEL_DIR");
        // Without TRAVSR_RERANK_MODEL_DIR, reranker() must degrade to None —
        // this is the "ships dark" default for every environment that hasn't
        // opted in (including CI).
        assert!(reranker().is_none());
        assert_eq!(rerank("anything", &["a", "b"]), None);
    }

    #[test]
    fn disabled_flag_short_circuits_even_with_model_dir() {
        std::env::set_var("TRAVSR_NO_RERANK", "1");
        std::env::set_var("TRAVSR_RERANK_MODEL_DIR", "/nonexistent/path/for/test");
        let result = rerank("q", &["a"]);
        std::env::remove_var("TRAVSR_NO_RERANK");
        std::env::remove_var("TRAVSR_RERANK_MODEL_DIR");
        assert_eq!(result, None);
    }
}
