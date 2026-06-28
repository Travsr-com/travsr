// EmbedSupervisor — daemon-level lifecycle manager for the embed sidecar.
//
// Zero-cost when no embed plugin binary is installed: the supervisor holds
// Option<Arc<Mutex<EmbedSidecar>>> = None and all methods are no-ops.
// The SqliteStore embed_knn_hook is never injected in that case.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use travsr_core::NodeId;
use travsr_error::StoreError;

use crate::embed_sidecar::{EmbedCapabilities, EmbedSidecar};

/// Callback type for Step 4 — matches `travsr_store::EmbedKnnHook`.
type KnnHook = Arc<dyn Fn(&str, u32) -> Result<Vec<(NodeId, f32)>, StoreError> + Send + Sync>;

/// FT-H3: maximum respawn attempts before permanently disabling the supervisor.
const MAX_RESPAWN_ATTEMPTS: u32 = 3;

/// Manages the embed plugin subprocess for one daemon session.
///
/// Created once at daemon startup. If the binary is not installed or the
/// handshake fails, the supervisor is inactive and all methods are no-ops.
/// The daemon checks `is_active()` before injecting the knn hook into the store.
pub struct EmbedSupervisor {
    /// None when no plugin installed or handshake permanently failed.
    inner: Option<Arc<Mutex<EmbedSidecar>>>,
    model_id: Option<String>,
    // FT-H3: respawn state — stored for `maybe_respawn`.
    binary: Option<PathBuf>,
    db_path: Option<PathBuf>,
    respawn_count: u32,
}

impl EmbedSupervisor {
    /// Try to start the embed plugin supervisor.
    ///
    /// `binary`   — absolute path to a `travsr-embed-<backend>` binary.
    /// `db_path`  — absolute path to the repo's `graph.db` file.
    /// `model_id` — backend catalog ID forwarded to the sidecar via `--model-id`.
    ///
    /// Returns a disabled supervisor (is_active() == false) if the binary is
    /// absent or the handshake fails — never panics, never returns an error.
    pub fn try_start(binary: &Path, db_path: &Path, model_id: &str) -> Self {
        if !binary.exists() {
            tracing::debug!(
                binary = %binary.display(),
                "embed plugin binary not found — Step 4 (semantic ANN) disabled. \
                 Run `travsr embed init` to install."
            );
            return Self {
                inner: None,
                model_id: None,
                binary: None,
                db_path: None,
                respawn_count: 0,
            };
        }

        match EmbedSidecar::spawn(binary, db_path, model_id) {
            Ok(sidecar) => {
                let model_id = sidecar.caps.model_id.clone();
                tracing::info!(
                    model_id = %model_id,
                    dim = sidecar.caps.embedding_dim,
                    backend = %sidecar.caps.backend,
                    "embed sidecar started"
                );
                Self {
                    model_id: Some(model_id),
                    inner: Some(Arc::new(Mutex::new(sidecar))),
                    binary: Some(binary.to_path_buf()),
                    db_path: Some(db_path.to_path_buf()),
                    respawn_count: 0,
                }
            }
            Err(e) => {
                tracing::warn!("embed sidecar start failed — Step 4 disabled: {e}");
                Self {
                    inner: None,
                    model_id: None,
                    binary: None,
                    db_path: None,
                    respawn_count: 0,
                }
            }
        }
    }

    /// Returns `true` when the embed plugin subprocess is running and healthy.
    pub fn is_active(&self) -> bool {
        match &self.inner {
            None => false,
            Some(arc) => arc.lock().map(|s| s.is_alive()).unwrap_or(false),
        }
    }

    /// Returns the negotiated capabilities, or `None` when inactive.
    pub fn capabilities(&self) -> Option<EmbedCapabilities> {
        self.inner
            .as_ref()
            .and_then(|arc| arc.lock().ok().map(|s| s.caps.clone()))
    }

    /// Build the knn_hook closure suitable for `SqliteStore::set_embed_knn_hook`.
    ///
    /// Returns `None` when the supervisor is inactive so the caller can skip
    /// injection entirely. The returned closure holds an `Arc` clone — the
    /// supervisor may be dropped after injection; the hook keeps the sidecar alive.
    ///
    /// `model_id` — must match the negotiated `caps.model_id` and the value
    /// stored in `meta.current_embed_model`. Forwarded in every KnnRequest.
    pub fn knn_hook(&self, model_id: String) -> Option<KnnHook> {
        let arc = self.inner.as_ref()?.clone();
        Some(Arc::new(move |query: &str, k: u32| {
            let sidecar = arc
                .lock()
                .map_err(|_| StoreError::Database("embed sidecar mutex poisoned".into()))?;
            if !sidecar.is_alive() {
                return Ok(vec![]);
            }
            match sidecar.knn(query, k, &model_id) {
                Ok(pairs) => {
                    // Kythe VName hashes are signed i64 and can be negative; usearch
                    // stores them as u64 via bit-reinterpretation and returns the same
                    // bit pattern. Both NodeId(positive_as_u64) and NodeId(neg_as_u64)
                    // are valid — SQLite stores the original i64 and the cast roundtrips.
                    let nodes: Vec<(NodeId, f32)> = pairs
                        .into_iter()
                        .map(|(id, score)| (NodeId(id as u64), score))
                        .collect();
                    Ok(nodes)
                }
                Err(e) => {
                    // Non-fatal: log and return empty rather than failing the query.
                    tracing::warn!("embed knn failed (non-fatal): {e}");
                    Ok(vec![])
                }
            }
        }))
    }

    /// The model_id negotiated at handshake, or `None` when inactive.
    pub fn model_id(&self) -> Option<&str> {
        self.model_id.as_deref()
    }

    /// FT-H3: attempt to respawn the sidecar after a crash.
    ///
    /// Bounded by MAX_RESPAWN_ATTEMPTS. Each call uses exponential back-off
    /// (2^n seconds, capped at 30s) before trying to spawn. Returns `true` if
    /// the respawn succeeded and the supervisor is now active again.
    ///
    /// Safe to call even when the supervisor was never active (returns `false`
    /// immediately). Callers should check `is_active()` afterward.
    pub fn maybe_respawn(&mut self) -> bool {
        if self.binary.is_none() || self.db_path.is_none() {
            return false;
        }
        if self.is_active() {
            return true;
        }
        if self.respawn_count >= MAX_RESPAWN_ATTEMPTS {
            tracing::warn!(
                attempts = MAX_RESPAWN_ATTEMPTS,
                "embed sidecar exceeded max respawn attempts — Step 4 permanently disabled"
            );
            return false;
        }

        let backoff_secs = (1u64 << self.respawn_count).min(30);
        tracing::info!(
            attempt = self.respawn_count + 1,
            max = MAX_RESPAWN_ATTEMPTS,
            backoff_secs,
            "respawning embed sidecar"
        );
        std::thread::sleep(std::time::Duration::from_secs(backoff_secs));
        self.respawn_count += 1;

        let binary = self.binary.as_deref().unwrap();
        let db_path = self.db_path.as_deref().unwrap();
        let model_id = self.model_id.as_deref().unwrap_or("");

        match EmbedSidecar::spawn(binary, db_path, model_id) {
            Ok(sidecar) => {
                let mid = sidecar.caps.model_id.clone();
                tracing::info!(model_id = %mid, attempt = self.respawn_count, "embed sidecar respawned");
                self.model_id = Some(mid);
                self.inner = Some(Arc::new(Mutex::new(sidecar)));
                true
            }
            Err(e) => {
                tracing::warn!(
                    attempt = self.respawn_count,
                    "embed sidecar respawn failed: {e}"
                );
                self.inner = None;
                false
            }
        }
    }
}
