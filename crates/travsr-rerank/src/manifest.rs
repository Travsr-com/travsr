//! RFC-021 F8: per-model floor manifest (`model.toml`).
//!
//! Pins the reranker's confidence floors to the specific model they were
//! measured against, so **model + floors bump atomically** (§15 G7) instead of
//! the floors living as prose in the RFC / hardcoded defaults in `seed.rs`.
//!
//! Mirrors the embed prior art
//! (`travsr-plugin-host::embed_catalog::{write_model_descriptor, ensure_model_descriptor}`):
//! an atomic write plus a self-heal migration for pre-manifest installs. The
//! file sits next to `model_fp16.onnx`; Phase 5 ships it as a release asset,
//! and until then `ensure_manifest` writes it for the manual/dev install.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::MODEL_FILE;

/// Stable identifier of the v1 reranker model: the fp16 export of
/// `cross-encoder/ms-marco-MiniLM-L-6-v2`. Bumping the model bumps this string
/// (and, with it, the measured floors it ships alongside).
pub const MODEL_ID: &str = "ms-marco-MiniLM-L-6-v2";

/// Confidence floors for [`MODEL_ID`]. Measured 2026-07-17 against
/// `bench/queries.json` (travsr, N=27) and re-validated 2026-07-22 on
/// kubernetes/kubernetes (Go): the same `(0.8, 0.5)` pair holds 100%
/// salad-abstention on both repos (RFC-021 §13.2 / §9.8). These are the single
/// source of truth for the fallback default — `seed.rs` references them rather
/// than re-declaring the literals.
pub const DEFAULT_STRONG_FLOOR: f32 = 0.8;
pub const DEFAULT_WEAK_FLOOR: f32 = 0.5;

/// Written next to `model_fp16.onnx`.
const MANIFEST_FILE: &str = "model.toml";

/// The per-model floor manifest. Co-locating the floors with the model is what
/// makes them bump atomically; `model_id` + `sha256` identify *which* model the
/// floors were measured against, so a model swapped without rewriting this file
/// is detectable ([`warn_on_sha_mismatch`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RerankManifest {
    /// Stable model id (see [`MODEL_ID`]).
    pub model_id: String,
    /// Lowercase hex SHA-256 of `model_fp16.onnx` at manifest-write time.
    /// Provenance for Phase 5's `SHA256SUMS` flow and the stale-floor guard.
    pub sha256: String,
    /// Floors these `model_id`/`sha256` were measured against.
    pub strong_floor: f32,
    pub weak_floor: f32,
}

impl RerankManifest {
    /// Read + parse `model.toml` from `model_dir`. Infallible for the caller:
    /// an absent or malformed manifest returns `None` (the floor read then
    /// falls back to the compiled-in defaults), so a torn/hand-broken file can
    /// never crash a query. A malformed file is warned about once.
    pub fn read(model_dir: &Path) -> Option<Self> {
        let path = model_dir.join(MANIFEST_FILE);
        let content = std::fs::read_to_string(&path).ok()?;
        match toml::from_str(&content) {
            Ok(manifest) => Some(manifest),
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "rerank: malformed model.toml, ignoring, using default floors"
                );
                None
            }
        }
    }

    /// Atomically write this manifest to `model_dir/model.toml`.
    ///
    /// Write-to-temp-then-rename (same filesystem) so a concurrent reader — the
    /// background warm thread, or a parallel self-heal — never observes a torn
    /// file. `NamedTempFile` guarantees a distinct temp name (parallel writers
    /// can't collide) and cleans it up on any error.
    pub fn write(&self, model_dir: &Path) -> Result<()> {
        use std::io::Write as _;
        let content = toml::to_string(self).context("serialize rerank manifest")?;
        let path = model_dir.join(MANIFEST_FILE);
        let mut tmp = tempfile::NamedTempFile::new_in(model_dir).with_context(|| {
            format!(
                "creating temp for rerank manifest in {}",
                model_dir.display()
            )
        })?;
        tmp.write_all(content.as_bytes())
            .context("writing rerank manifest to temp file")?;
        tmp.persist(&path)
            .map_err(|e| anyhow::Error::new(e.error))
            .with_context(|| format!("persisting rerank manifest to {}", path.display()))?;
        Ok(())
    }
}

/// Lowercase hex SHA-256 of `model_dir/model_fp16.onnx`.
fn model_sha256(model_dir: &Path) -> Result<String> {
    let path = model_dir.join(MODEL_FILE);
    let bytes =
        std::fs::read(&path).with_context(|| format!("reading {} for sha256", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let digest = hasher.finalize();

    use std::fmt::Write as _;
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(hex)
}

/// Self-heal a pre-manifest install: write `model.toml` iff the model is
/// present (`model_fp16.onnx` exists) but the manifest is absent. This is what
/// makes the current manual/dev install (hand-fetched model, no manifest)
/// manifest-backed without a re-download, exactly as embed's
/// `ensure_model_descriptor` migrates pre-descriptor installs.
///
/// Never fatal: a hash or write failure is logged and the caller proceeds on
/// the compiled-in default floors. No-op when the manifest already exists or
/// the model is not installed.
pub fn ensure_manifest(model_dir: &Path) {
    let model = model_dir.join(MODEL_FILE);
    let manifest = model_dir.join(MANIFEST_FILE);
    if !model.exists() || manifest.exists() {
        return;
    }
    let sha256 = match model_sha256(model_dir) {
        Ok(sha) => sha,
        Err(error) => {
            tracing::warn!(%error, "rerank: could not hash model for manifest self-heal");
            return;
        }
    };
    let written = RerankManifest {
        model_id: MODEL_ID.to_string(),
        sha256,
        strong_floor: DEFAULT_STRONG_FLOOR,
        weak_floor: DEFAULT_WEAK_FLOOR,
    };
    match written.write(model_dir) {
        Ok(()) => tracing::info!(
            path = %manifest.display(),
            "rerank: wrote missing model.toml (self-heal)"
        ),
        Err(error) => tracing::warn!(
            %error,
            "rerank: could not write model.toml, using default floors"
        ),
    }
}

/// Warn (once, on the caller's thread) if the on-disk `model_fp16.onnx` no
/// longer matches the sha256 the manifest's floors were measured against —
/// i.e. the model was swapped without rewriting `model.toml`, so its floors may
/// be stale. Never fatal: the manifest's floors are still applied (best
/// available); this only surfaces the drift.
pub fn warn_on_sha_mismatch(manifest: &RerankManifest, model_dir: &Path) {
    match model_sha256(model_dir) {
        Ok(actual) if actual != manifest.sha256 => tracing::warn!(
            expected = %manifest.sha256,
            %actual,
            "rerank: model.toml sha256 does not match model_fp16.onnx, floors may be stale (re-run the floor sweep for this model)"
        ),
        Ok(_) => {}
        Err(error) => {
            tracing::debug!(%error, "rerank: could not hash model for manifest verification")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes a throwaway `model_fp16.onnx` so `ensure_manifest`/sha256 have a
    /// file to hash. Content is arbitrary — only the bytes' hash matters.
    fn fake_model(dir: &Path, bytes: &[u8]) {
        std::fs::write(dir.join(MODEL_FILE), bytes).unwrap();
    }

    #[test]
    fn write_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = RerankManifest {
            model_id: MODEL_ID.to_string(),
            sha256: "deadbeef".to_string(),
            strong_floor: 0.8,
            weak_floor: 0.5,
        };
        manifest.write(dir.path()).unwrap();
        let read = RerankManifest::read(dir.path()).unwrap();
        assert_eq!(read, manifest);
    }

    #[test]
    fn read_absent_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(RerankManifest::read(dir.path()).is_none());
    }

    #[test]
    fn read_malformed_is_none_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(MANIFEST_FILE), "this = is = not = toml").unwrap();
        assert!(RerankManifest::read(dir.path()).is_none());
    }

    #[test]
    fn write_is_atomic_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        RerankManifest {
            model_id: MODEL_ID.to_string(),
            sha256: "abc".to_string(),
            strong_floor: 0.8,
            weak_floor: 0.5,
        }
        .write(dir.path())
        .unwrap();
        // Exactly one entry (model.toml); NamedTempFile is renamed into place,
        // never left behind.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from(MANIFEST_FILE)]);
    }

    #[test]
    fn ensure_writes_defaults_when_model_present_and_manifest_absent() {
        let dir = tempfile::tempdir().unwrap();
        fake_model(dir.path(), b"weights");
        ensure_manifest(dir.path());
        let read = RerankManifest::read(dir.path()).expect("manifest self-healed");
        assert_eq!(read.model_id, MODEL_ID);
        assert_eq!(read.strong_floor, DEFAULT_STRONG_FLOOR);
        assert_eq!(read.weak_floor, DEFAULT_WEAK_FLOOR);
        assert!(!read.sha256.is_empty());
    }

    #[test]
    fn ensure_noop_when_no_model_installed() {
        let dir = tempfile::tempdir().unwrap();
        ensure_manifest(dir.path());
        assert!(RerankManifest::read(dir.path()).is_none());
    }

    #[test]
    fn ensure_does_not_overwrite_existing_manifest() {
        let dir = tempfile::tempdir().unwrap();
        fake_model(dir.path(), b"weights");
        // A pre-existing manifest with a *tuned* weak floor must survive.
        let tuned = RerankManifest {
            model_id: MODEL_ID.to_string(),
            sha256: "existing".to_string(),
            strong_floor: 0.85,
            weak_floor: 0.55,
        };
        tuned.write(dir.path()).unwrap();
        ensure_manifest(dir.path());
        assert_eq!(RerankManifest::read(dir.path()).unwrap(), tuned);
    }

    #[test]
    fn sha256_is_stable_and_content_dependent() {
        let dir = tempfile::tempdir().unwrap();
        fake_model(dir.path(), b"weights-a");
        let a1 = model_sha256(dir.path()).unwrap();
        let a2 = model_sha256(dir.path()).unwrap();
        assert_eq!(a1, a2, "same bytes hash identically");
        assert_eq!(a1.len(), 64, "sha256 hex is 64 chars");
        fake_model(dir.path(), b"weights-b");
        assert_ne!(
            a1,
            model_sha256(dir.path()).unwrap(),
            "different bytes differ"
        );
    }
}
