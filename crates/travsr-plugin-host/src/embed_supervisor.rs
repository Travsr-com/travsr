// EmbedSupervisor — daemon-level lifecycle manager for the embed sidecar.
//
// Zero-cost when no embed plugin binary is installed: the supervisor holds
// Option<Arc<Mutex<EmbedSidecar>>> = None and all methods are no-ops.
// The SqliteStore embed_knn_hook is never injected in that case.

use std::path::Path;
use std::sync::{Arc, Mutex};

use travsr_core::NodeId;
use travsr_error::StoreError;

use crate::embed_sidecar::{EmbedCapabilities, EmbedSidecar};

/// Callback type for Step 4 — matches `travsr_store::EmbedKnnHook`.
type KnnHook = Arc<dyn Fn(&str, u32) -> Result<Vec<NodeId>, StoreError> + Send + Sync>;

/// Manages the embed plugin subprocess for one daemon session.
///
/// Created once at daemon startup. If the binary is not installed or the
/// handshake fails, the supervisor is inactive and all methods are no-ops.
/// The daemon checks `is_active()` before injecting the knn hook into the store.
pub struct EmbedSupervisor {
    /// None when no plugin installed or handshake failed.
    inner: Option<Arc<Mutex<EmbedSidecar>>>,
    model_id: Option<String>,
}

impl EmbedSupervisor {
    /// Try to start the embed plugin supervisor.
    ///
    /// `binary` — absolute path to a `travsr-embed-<backend>` binary.
    /// `db_path` — absolute path to the repo's `graph.db` file.
    ///
    /// Returns a disabled supervisor (is_active() == false) if the binary is
    /// absent or the handshake fails — never panics, never returns an error.
    pub fn try_start(binary: &Path, db_path: &Path) -> Self {
        if !binary.exists() {
            tracing::debug!(
                binary = %binary.display(),
                "embed plugin binary not found — Step 4 (semantic ANN) disabled. \
                 Run `travsr embed init` to install."
            );
            return Self { inner: None, model_id: None };
        }

        match EmbedSidecar::spawn(binary, db_path) {
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
                }
            }
            Err(e) => {
                tracing::warn!("embed sidecar start failed — Step 4 disabled: {e}");
                Self { inner: None, model_id: None }
            }
        }
    }

    /// Returns `true` when the embed plugin subprocess is running and healthy.
    pub fn is_active(&self) -> bool {
        match &self.inner {
            None => false,
            Some(arc) => arc
                .lock()
                .map(|s| s.is_alive())
                .unwrap_or(false),
        }
    }

    /// Returns the negotiated capabilities, or `None` when inactive.
    pub fn capabilities(&self) -> Option<EmbedCapabilities> {
        self.inner.as_ref().and_then(|arc| {
            arc.lock().ok().map(|s| s.caps.clone())
        })
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
            let sidecar = arc.lock().map_err(|_| {
                StoreError::Database("embed sidecar mutex poisoned".into())
            })?;
            if !sidecar.is_alive() {
                return Ok(vec![]);
            }
            match sidecar.knn(query, k, &model_id) {
                Ok(ids) => Ok(ids.into_iter().map(|id| NodeId(id as u64)).collect()),
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
}
