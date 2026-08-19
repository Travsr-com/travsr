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

use travsr_rerank::{RerankManifest, Reranker, TractReranker};

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

/// The canonical per-user install location for the reranker model, mirroring
/// the embed backends' `~/.travsr/models/<id>` layout (see `embed.rs`). P5 ships
/// the model here via `travsr rerank install` / the daemon's background
/// auto-fetch. `None` only when the home directory can't be resolved.
pub(crate) fn default_rerank_model_dir() -> Option<PathBuf> {
    Some(
        dirs::home_dir()?
            .join(".travsr")
            .join("models")
            .join("rerank"),
    )
}

/// Directory containing `model_fp16.onnx` + `tokenizer.json`.
///
/// Resolution order:
/// 1. `TRAVSR_RERANK_MODEL_DIR` — explicit override (dev / CI / offline / a
///    custom model). Returned verbatim; existence is `TractReranker::load`'s job.
/// 2. The default install dir `~/.travsr/models/rerank`, but **only when the
///    model file is actually present** — so a fresh machine that has not fetched
///    the model yet stays `None` (fail-open / ships-dark) rather than pointing
///    the loader at an empty directory.
fn rerank_model_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("TRAVSR_RERANK_MODEL_DIR") {
        return Some(PathBuf::from(dir));
    }
    let default = default_rerank_model_dir()?;
    default
        .join(travsr_rerank::MODEL_FILE)
        .exists()
        .then_some(default)
}

/// `None` once load has been attempted (missing config, missing files, or a
/// load-time error) — cached permanently so a broken install doesn't retry an
/// expensive failed load on every query.
///
/// Test-process caveat (RFC-021 F6): first initialization wins for the whole
/// test binary — whichever test calls `reranker()` first (today, always with
/// no model dir configured) permanently pins this to `None` for every test in
/// this module, run in any order. A model-backed wrapper test would flake
/// under the default parallel test harness; the right shape for that is
/// extracting a `fn load_from(dir: Option<PathBuf>) -> Option<TractReranker>`
/// out of the `get_or_init` closure and testing it directly, leaving this
/// static untouched.
static RERANKER: OnceLock<Option<TractReranker>> = OnceLock::new();
static WARM_STARTED: std::sync::Once = std::sync::Once::new();

/// The per-model floor manifest (`model.toml`) next to the model, cached like
/// [`RERANKER`]. `None` when no model dir is configured or the manifest is
/// absent/malformed — the floor read (`seed.rs`) then falls back to the
/// compiled-in `travsr_rerank::DEFAULT_*` floors. Same test-process caveat as
/// `RERANKER`: first init wins for the whole test binary.
static MANIFEST: OnceLock<Option<RerankManifest>> = OnceLock::new();

/// Load (once) the reranker floor manifest for the configured model dir. Runs
/// the self-heal migration (writes `model.toml` for a manual/dev install that
/// predates F8) and, once, verifies the recorded sha256 against the on-disk
/// model so stale floors surface as a warning. Independent of `rerank_disabled`
/// / model *load* success: floors are cheap to resolve and harmless to read
/// even when inference itself is off.
fn manifest() -> Option<&'static RerankManifest> {
    MANIFEST
        .get_or_init(|| {
            let dir = rerank_model_dir()?;
            travsr_rerank::ensure_manifest(&dir);
            let manifest = RerankManifest::read(&dir)?;
            travsr_rerank::warn_on_sha_mismatch(&manifest, &dir);
            Some(manifest)
        })
        .as_ref()
}

/// STRONG floor from the model manifest, if one is present. `seed.rs` prefers
/// an explicit env override, then this, then the compiled-in default.
pub(crate) fn manifest_strong_floor() -> Option<f32> {
    manifest().map(|m| m.strong_floor)
}

/// WEAK floor from the model manifest, if one is present. See
/// [`manifest_strong_floor`].
pub(crate) fn manifest_weak_floor() -> Option<f32> {
    manifest().map(|m| m.weak_floor)
}

/// Human-readable reranker state for `travsr status`. Honest in both contexts:
/// answered by the warm daemon it reflects the loaded model; on the cold CLI
/// fast-path (no daemon) it reflects on-disk presence, since that process never
/// warms the model itself.
///
/// - `off` — `TRAVSR_NO_RERANK` set.
/// - `not installed` — no model configured and none at the default location.
/// - `installed` — model present on disk but not loaded in *this* process.
/// - `ready` — model loaded and serving.
/// - `load failed` — a load was attempted and failed (fail-open to lexical).
pub(crate) fn rerank_status() -> &'static str {
    if rerank_disabled() {
        return "off";
    }
    if rerank_model_dir().is_none() {
        return "not installed";
    }
    match RERANKER.get() {
        Some(Some(_)) => "ready",
        Some(None) => "load failed",
        None => "installed",
    }
}

// ── Model distribution (RFC-021 P5) ──────────────────────────────────────────
//
// The cross-encoder model is byte-identical across every travsr version, so it
// does NOT ride the per-version binary tarball (which also has a 25 MB gate the
// 45.9 MB model would blow). It lives on its own immutable `rerank-model-v1`
// GitHub release and is fetched into `~/.travsr/models/rerank` on demand.
// Integrity is pinned by a committed sha256 per file (tamper-evident,
// independent of the mutable release listing). When the model changes (#462),
// cut a new tag and bump the tag + both hashes together.

/// Immutable, version-independent model release tag.
const MODEL_TAG: &str = "rerank-model-v1";
/// sha256 of `model_fp16.onnx` (ms-marco-MiniLM-L-6-v2, fp16, ~45.9 MB).
const MODEL_SHA256: &str = "a59cbeb92524ab794e3bf3c7a93f5de73bd59b74241b65e81de46dc5bbf4c3ab";
/// sha256 of `tokenizer.json` (HF fast-tokenizer, ~0.7 MB).
const TOKENIZER_SHA256: &str = "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66";
/// Reject a response an order of magnitude past the real sizes before hashing.
const MODEL_SIZE_LIMIT: u64 = 120 * 1024 * 1024;
/// Base URL for the release-asset download. Overridable for tests/mirrors.
const RELEASES_BASE_ENV: &str = "TRAVSR_RERANK_RELEASES_BASE";
/// Shared hermetic hook (also used by `travsr lang install`): skip the network
/// and just report the destination directory.
const SKIP_DOWNLOAD_ENV: &str = "TRAVSR_SKIP_DOWNLOAD";
/// Opt out of the daemon's background auto-fetch without disabling the reranker
/// entirely (an already-installed model still loads).
const NO_AUTOFETCH_ENV: &str = "TRAVSR_NO_RERANK_AUTOFETCH";
const DEFAULT_RELEASES_BASE: &str = "https://github.com/Travsr-com/travsr/releases/download";

/// `true` when both model files are present at the default install location.
pub fn model_installed() -> bool {
    default_rerank_model_dir().is_some_and(|dir| {
        dir.join(travsr_rerank::MODEL_FILE).is_file()
            && dir.join(travsr_rerank::TOKENIZER_FILE).is_file()
    })
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = <sha2::Sha256 as sha2::Digest>::digest(bytes);
    let mut s = String::with_capacity(digest.len() * 2);
    for b in digest {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0x0f) as u32, 16).unwrap());
    }
    s
}

async fn download_verified(
    base: &str,
    file: &str,
    expected_sha: &str,
    client: &reqwest::Client,
) -> anyhow::Result<Vec<u8>> {
    use anyhow::bail;
    let url = format!("{base}/{MODEL_TAG}/{file}");
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("requesting {url}: {e}"))?;
    if !resp.status().is_success() {
        bail!("download failed ({}): {url}", resp.status());
    }
    if let Some(len) = resp.content_length() {
        if len > MODEL_SIZE_LIMIT {
            bail!("{file} exceeds size limit ({len} bytes)");
        }
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| anyhow::anyhow!("reading {file} body: {e}"))?;
    if bytes.len() as u64 > MODEL_SIZE_LIMIT {
        bail!(
            "{file} exceeds size limit after download ({} bytes)",
            bytes.len()
        );
    }
    let actual = hex_sha256(&bytes);
    if actual != expected_sha {
        bail!("sha256 mismatch for {file}: expected {expected_sha}, got {actual}");
    }
    Ok(bytes.to_vec())
}

fn write_atomic(dir: &std::path::Path, name: &str, bytes: &[u8]) -> anyhow::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    // Per-call sequence so two threads writing the same asset in one process
    // (pid alone is not unique across threads) target distinct temp files and
    // never interleave writes before the rename.
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let dest = dir.join(name);
    let tmp = dir.join(format!(
        "{name}.tmp.{}.{}",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&tmp, bytes).map_err(|e| anyhow::anyhow!("writing {}: {e}", tmp.display()))?;
    if let Err(e) = std::fs::rename(&tmp, &dest) {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow::anyhow!("moving into place {}: {e}", dest.display()));
    }
    Ok(())
}

/// Fetch + verify + install `model_fp16.onnx` + `tokenizer.json` into
/// `~/.travsr/models/rerank`, then write `model.toml` (F8 manifest). Idempotent
/// (re-downloads/overwrites, so it doubles as a repair). `TRAVSR_SKIP_DOWNLOAD`
/// short-circuits the network and returns the target dir (CI/offline).
pub async fn install_model() -> anyhow::Result<PathBuf> {
    let dir = default_rerank_model_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| anyhow::anyhow!("creating {}: {e}", dir.display()))?;
    if std::env::var_os(SKIP_DOWNLOAD_ENV).is_some() {
        return Ok(dir);
    }

    let base =
        std::env::var(RELEASES_BASE_ENV).unwrap_or_else(|_| DEFAULT_RELEASES_BASE.to_string());
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .user_agent(concat!("travsr-mcp/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| anyhow::anyhow!("building HTTP client: {e}"))?;

    let (model, tokenizer) = tokio::try_join!(
        download_verified(&base, travsr_rerank::MODEL_FILE, MODEL_SHA256, &client),
        download_verified(
            &base,
            travsr_rerank::TOKENIZER_FILE,
            TOKENIZER_SHA256,
            &client
        ),
    )?;

    write_atomic(&dir, travsr_rerank::MODEL_FILE, &model)?;
    write_atomic(&dir, travsr_rerank::TOKENIZER_FILE, &tokenizer)?;
    travsr_rerank::ensure_manifest(&dir);
    Ok(dir)
}

/// Blocking [`install_model`], safe to call from a plain `std::thread` OR from
/// inside a running tokio runtime: it drives the future on a fresh runtime in a
/// scoped worker thread, so it never panics with "runtime within a runtime"
/// (the `travsr rerank install` CLI path runs inside `#[tokio::main]`).
pub fn install_model_blocking() -> anyhow::Result<PathBuf> {
    std::thread::scope(|s| {
        s.spawn(|| {
            tokio::runtime::Runtime::new()
                .map_err(|e| anyhow::anyhow!("creating tokio runtime: {e}"))?
                .block_on(install_model())
        })
        .join()
        .map_err(|_| anyhow::anyhow!("model-download thread panicked"))?
    })
}

/// P5 auto-fetch: on daemon start, if the reranker is enabled and no model is
/// present at the default location, download it (blocking, in the warm thread)
/// BEFORE `reranker()` pins the `OnceLock`, so the model activates this session.
/// Every exit is a silent no-op → ships-dark fallback; never fatal.
fn maybe_autofetch_model() {
    if rerank_disabled() || std::env::var_os(NO_AUTOFETCH_ENV).is_some() {
        return;
    }
    // An explicit model dir is the operator's responsibility — never auto-write
    // into a location they pointed us at on purpose.
    if std::env::var_os("TRAVSR_RERANK_MODEL_DIR").is_some() {
        return;
    }
    if model_installed() {
        return;
    }
    tracing::info!("reranker model absent, fetching in the background");
    match install_model_blocking() {
        Ok(dir) => tracing::info!(dir = %dir.display(), "reranker model ready"),
        Err(error) => tracing::warn!(
            %error,
            "reranker model download failed, continuing without it"
        ),
    }
}

fn reranker() -> Option<&'static TractReranker> {
    if rerank_disabled() {
        return None;
    }
    RERANKER
        .get_or_init(|| {
            let dir = rerank_model_dir()?;
            // catch_unwind around the load itself: `tract` has internal panics
            // reachable via a malformed/truncated ONNX (a bad
            // TRAVSR_RERANK_MODEL_DIR override, or a partially-written install).
            // `OnceLock` does NOT cache a panicking initializer, so an escaping
            // panic would both propagate through the current query and re-run the
            // failed load on every subsequent one. Map a load panic to `None` so
            // the fail-open decision is cached once and the daemon degrades to the
            // lexical gate, matching the `Err` arm (RFC-021 G5/G6 fail-open).
            match catch_unwind(AssertUnwindSafe(|| TractReranker::load(&dir))) {
                Ok(Ok(r)) => {
                    tracing::info!(model_dir = %dir.display(), "reranker loaded");
                    Some(r)
                }
                Ok(Err(error)) => {
                    tracing::warn!(
                        %error,
                        model_dir = %dir.display(),
                        "reranker failed to load, continuing without it"
                    );
                    None
                }
                Err(_) => {
                    tracing::warn!(
                        model_dir = %dir.display(),
                        "reranker crashed while loading, continuing without it"
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
                // P5: fetch the model first (no-op when present/disabled) so the
                // load below sees it and the reranker activates this session —
                // `reranker()` pins its OnceLock, so a later download would not
                // be picked up until the next daemon start.
                maybe_autofetch_model();
                reranker();
                // Resolve the manifest here too (self-heal write + one-time
                // sha256 verification) so the first query never pays the model
                // hash on its hot path.
                manifest();
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
            tracing::warn!(%error, "rerank inference failed, falling back to lexical gate");
            return None;
        }
        Err(_) => {
            tracing::warn!("rerank inference panicked, falling back to lexical gate");
            return None;
        }
    };

    let budget = rerank_budget_ms();
    if elapsed_ms > budget {
        tracing::warn!(
            elapsed_ms,
            threshold_ms = budget,
            "rerank exceeded circuit-breaker threshold, discarding scores, falling back to lexical gate"
        );
        return None;
    }
    Some(scores)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that mutate process-global env vars; the default test
    /// harness runs test fns on parallel threads and `set_var`/`remove_var`
    /// are process-wide (RFC-021 F6).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::remove_var("TRAVSR_NO_RERANK");
        // Point discovery at a guaranteed-absent dir rather than *removing* the
        // override: with RFC-021 P5 the model is default-installed at
        // ~/.travsr/models/rerank on any machine that has run the daemon, so
        // clearing the override would let reranker() find that real model and
        // load Some — the assertion must stay hermetic regardless of local
        // installs. A nonexistent dir exercises the identical "no usable model
        // -> None" degrade path (loader fails -> cached None), which is the
        // ships-dark contract this test guards.
        std::env::set_var(
            "TRAVSR_RERANK_MODEL_DIR",
            "/nonexistent/travsr-rerank-none-test",
        );
        assert!(reranker().is_none());
        assert_eq!(rerank("anything", &["a", "b"]), None);
        std::env::remove_var("TRAVSR_RERANK_MODEL_DIR");
    }

    #[test]
    fn disabled_flag_short_circuits_even_with_model_dir() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::set_var("TRAVSR_NO_RERANK", "1");
        std::env::set_var("TRAVSR_RERANK_MODEL_DIR", "/nonexistent/path/for/test");
        let result = rerank("q", &["a"]);
        std::env::remove_var("TRAVSR_NO_RERANK");
        std::env::remove_var("TRAVSR_RERANK_MODEL_DIR");
        assert_eq!(result, None);
    }

    #[test]
    fn rerank_model_dir_prefers_env_override_verbatim() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::set_var("TRAVSR_RERANK_MODEL_DIR", "/custom/model/dir");
        let dir = rerank_model_dir();
        std::env::remove_var("TRAVSR_RERANK_MODEL_DIR");
        // Override wins and is returned as-is (existence is the loader's job).
        assert_eq!(dir, Some(PathBuf::from("/custom/model/dir")));
    }

    #[test]
    fn default_rerank_model_dir_is_under_travsr_models_rerank() {
        // P5 canonical location, mirroring embed's ~/.travsr/models/<id>.
        let dir = default_rerank_model_dir().expect("home dir resolvable in test env");
        assert!(dir.ends_with("rerank"), "got {dir:?}");
        assert!(dir.parent().unwrap().ends_with("models"), "got {dir:?}");
        assert!(dir.to_string_lossy().contains(".travsr"), "got {dir:?}");
    }

    #[test]
    fn rerank_status_off_when_disabled() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::set_var("TRAVSR_NO_RERANK", "1");
        let s = rerank_status();
        std::env::remove_var("TRAVSR_NO_RERANK");
        assert_eq!(s, "off");
    }

    #[test]
    fn hex_sha256_matches_known_vector() {
        // sha256("") — canonical empty-input digest.
        assert_eq!(
            hex_sha256(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn pinned_model_hashes_are_lowercase_hex_64() {
        for h in [MODEL_SHA256, TOKENIZER_SHA256] {
            assert_eq!(h.len(), 64, "sha256 hex is 64 chars: {h}");
            assert!(
                h.bytes()
                    .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase()),
                "pinned hash must be lowercase hex: {h}"
            );
        }
    }
}
