// EmbedSupervisor — daemon-level lifecycle manager for the embed sidecar.
//
// Zero-cost when no embed plugin binary is installed: the supervisor holds
// Option<Arc<Mutex<EmbedSidecar>>> = None and all methods are no-ops.
// The SqliteStore embed_knn_hook is never injected in that case.

use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use travsr_core::NodeId;
use travsr_error::StoreError;
use travsr_plugin_protocol::Space;

use crate::embed_sidecar::{EmbedCapabilities, EmbedSidecar};

/// Callback type for Step 4 — matches `travsr_store::EmbedKnnHook`.
type KnnHook = Arc<dyn Fn(&str, u32) -> Result<Vec<(NodeId, f32)>, StoreError> + Send + Sync>;

/// RFC-019: callback that embeds one query and returns its raw vector BLOB
/// (backend-defined layout, same as stored `node_embeddings.embedding`). The
/// injection site decodes it and reads candidate vectors via `travsr_store`.
/// Kept SQLite-free so the plugin-host crate gains no storage dependency.
pub type EmbedQueryHook = Arc<dyn Fn(&str) -> Result<Vec<u8>, StoreError> + Send + Sync>;

/// FT-H3: maximum respawn attempts before permanently disabling the supervisor.
const MAX_RESPAWN_ATTEMPTS: u32 = 3;

/// Wall-clock budget for a single KNN round-trip. If the sidecar does not
/// respond within this window (model still loading ONNX + HNSW), the hook
/// returns an empty seed set immediately so the query falls back to FTS.
/// The sidecar thread continues loading in the background; subsequent calls
/// after warm-up complete in <200 ms and always beat this threshold.
const KNN_CALL_TIMEOUT_MS: u64 = 600;

/// Pending-request depth for one hook worker (issue #736 A3).
///
/// Kept deliberately small: a caller abandons its request after
/// [`KNN_CALL_TIMEOUT_MS`], so anything queued deeper than a couple of slots
/// behind a slow sidecar would be computed for nobody. When the queue is full
/// the request is shed immediately (empty result → FTS fallback), which is
/// the same outcome the caller's timeout would produce, minus the wasted work.
const HOOK_QUEUE_DEPTH: usize = 2;

/// One long-lived worker thread serving one hook type (issue #736 A3).
///
/// Replaces the previous thread-per-invocation pattern, where every hook call
/// spawned a fresh OS thread and abandoned it on timeout. Under a slow or
/// cold sidecar those abandoned threads piled up behind the sidecar mutex
/// without bound — a stack (~2 MiB reserved) plus the owned query per thread,
/// and a CPU/memory feedback loop once throttling made the sidecar slower.
///
/// Now exactly one thread per hook type exists for the supervisor's lifetime.
/// Requests travel over a bounded channel; the caller waits on a per-request
/// reply channel with the same timeout as before, so the observable contract
/// (empty result after 600 ms, warm calls fast) is unchanged. A reply whose
/// caller already gave up is dropped on send — no thread is ever stranded.
///
/// The worker exits when the last hook closure holding `tx` is dropped
/// (channel disconnect), i.e. at daemon shutdown.
struct HookWorker<Req: Send + 'static, Resp: Send + 'static> {
    tx: mpsc::SyncSender<(Req, mpsc::Sender<Resp>)>,
}

impl<Req: Send + 'static, Resp: Send + 'static> HookWorker<Req, Resp> {
    fn spawn(name: &'static str, handler: impl Fn(Req) -> Option<Resp> + Send + 'static) -> Self {
        let (tx, rx) = mpsc::sync_channel::<(Req, mpsc::Sender<Resp>)>(HOOK_QUEUE_DEPTH);
        std::thread::Builder::new()
            .name(name.to_string())
            .spawn(move || {
                while let Ok((req, reply)) = rx.recv() {
                    if let Some(resp) = handler(req) {
                        // Caller may have timed out and dropped its receiver —
                        // a failed send is the expected shape of that race.
                        let _ = reply.send(resp);
                    }
                }
            })
            .ok();
        Self { tx }
    }

    /// Submit a request and wait up to `timeout` for the reply. Returns None
    /// when the queue is full (worker backed up) or the reply timed out.
    fn call(&self, req: Req, timeout: Duration) -> Option<Resp> {
        let (reply_tx, reply_rx) = mpsc::channel();
        if self.tx.try_send((req, reply_tx)).is_err() {
            return None;
        }
        reply_rx.recv_timeout(timeout).ok()
    }
}

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
    ///
    /// ## Cold-start timeout
    ///
    /// The embed sidecar responds to the handshake before loading ONNX weights
    /// or the HNSW index, so the first real KNN call would otherwise block the
    /// calling thread for the full load time (~30 s on kubernetes).
    ///
    /// One long-lived [`HookWorker`] thread owns the blocking sidecar I/O
    /// (issue #736 A3 — previously a fresh thread per invocation, abandoned on
    /// timeout, piled up behind the sidecar mutex without bound). The calling
    /// thread waits at most `KNN_CALL_TIMEOUT_MS` (600 ms). If the sidecar has
    /// not responded by then, the hook returns an empty set immediately and
    /// the query falls back to FTS seeds. The worker keeps processing in the
    /// background; once the model is loaded subsequent calls are fast.
    pub fn knn_hook(&self, model_id: String) -> Option<KnnHook> {
        let arc = self.inner.as_ref()?.clone();
        let worker = HookWorker::spawn("embed-knn-worker", move |(query, k): (String, u32)| {
            let Ok(sidecar) = arc.lock() else { return None };
            if !sidecar.is_alive() {
                return None;
            }
            match sidecar.knn(&query, k, &model_id, Space::Code) {
                Ok(pairs) => {
                    // Kythe VName hashes are signed i64 and can be negative;
                    // usearch stores them as u64 via bit-reinterpretation and
                    // returns the same bit pattern. Cast roundtrips correctly.
                    Some(
                        pairs
                            .into_iter()
                            .map(|(id, score)| (NodeId(id as u64), score))
                            .collect::<Vec<(NodeId, f32)>>(),
                    )
                }
                Err(e) => {
                    tracing::warn!("embed knn failed (non-fatal): {e}");
                    None
                }
            }
        });
        Some(Arc::new(move |query: &str, k: u32| {
            // Empty set on shed (worker backed up) or timeout — same contract
            // as before, without the per-call thread.
            Ok(worker
                .call(
                    (query.to_string(), k),
                    Duration::from_millis(KNN_CALL_TIMEOUT_MS),
                )
                .unwrap_or_default())
        }))
    }

    /// #376 Phase 2: doc-space counterpart to [`Self::knn_hook`]. `None` both
    /// when the supervisor is inactive and when the connected sidecar predates
    /// doc-space support (`EmbedCapabilities::supports_doc_space`) — an old
    /// sidecar must never be asked for `Space::Docs` (see that method's doc
    /// comment). Same cold-start / circuit-breaker shape as `knn_hook`, on an
    /// independent round trip: the sidecar-side query-embedding memo cache
    /// (`travsr-embed`) absorbs the latency this would otherwise double,
    /// without changing `KnnHook`'s signature or any of its existing callers.
    pub fn doc_knn_hook(&self, model_id: String) -> Option<KnnHook> {
        let arc = self.inner.as_ref()?.clone();
        if !self.capabilities()?.supports_doc_space() {
            return None;
        }
        let worker = HookWorker::spawn("embed-doc-knn-worker", move |(query, k): (String, u32)| {
            let Ok(sidecar) = arc.lock() else { return None };
            if !sidecar.is_alive() {
                return None;
            }
            match sidecar.knn(&query, k, &model_id, Space::Docs) {
                Ok(pairs) => Some(
                    pairs
                        .into_iter()
                        .map(|(id, score)| (NodeId(id as u64), score))
                        .collect::<Vec<(NodeId, f32)>>(),
                ),
                Err(e) => {
                    tracing::warn!("embed doc knn failed (non-fatal): {e}");
                    None
                }
            }
        });
        Some(Arc::new(move |query: &str, k: u32| {
            Ok(worker
                .call(
                    (query.to_string(), k),
                    Duration::from_millis(KNN_CALL_TIMEOUT_MS),
                )
                .unwrap_or_default())
        }))
    }

    /// Build the RFC-019 query-embedding hook: embeds one short query text and
    /// returns its raw vector BLOB. `None` when the supervisor is inactive.
    ///
    /// Same cold-start protection as [`Self::knn_hook`]: one long-lived
    /// [`HookWorker`] does the blocking `embed_batch` I/O while the calling
    /// thread waits at most `KNN_CALL_TIMEOUT_MS`. On shed, timeout, or error
    /// the hook returns an **empty** BLOB — the caller's `decode_embedding`
    /// then fails and no candidates are scored (unknown), so a slow/cold
    /// sidecar can never stall `get_context` or fabricate a false cosine.
    /// Warm `embed_batch` of one short text is ~tens of ms.
    pub fn embed_query_hook(&self) -> Option<EmbedQueryHook> {
        let arc = self.inner.as_ref()?.clone();
        let worker = HookWorker::spawn("embed-query-worker", move |query: String| {
            let Ok(sidecar) = arc.lock() else { return None };
            if !sidecar.is_alive() {
                return None;
            }
            match sidecar.embed_batch(std::slice::from_ref(&query)) {
                Ok(mut blobs) if !blobs.is_empty() => Some(blobs.swap_remove(0)),
                Ok(_) => None,
                Err(e) => {
                    tracing::warn!("embed query failed (non-fatal): {e}");
                    None
                }
            }
        });
        Some(Arc::new(move |query: &str| {
            Ok(worker
                .call(
                    query.to_string(),
                    Duration::from_millis(KNN_CALL_TIMEOUT_MS),
                )
                .unwrap_or_default())
        }))
    }

    /// The model_id negotiated at handshake, or `None` when inactive.
    pub fn model_id(&self) -> Option<&str> {
        self.model_id.as_deref()
    }

    /// Eagerly load the ONNX model + HNSW index by firing one dummy KNN.
    ///
    /// The sidecar answers the handshake *before* loading any weights or index
    /// data, so the first real KNN call would otherwise pay the full cold-start
    /// cost (model load ~0.6 s) — long enough to trip the host's 600 ms KNN
    /// circuit-breaker and degrade that first query to FTS (`embed_used=false`).
    ///
    /// Blocking by design: callers run this on a long-lived host's background
    /// init thread (MCP `embed-hook-init`, daemon startup) and arm the KNN hook
    /// only *after* it returns, so the hook never goes live cold. Holds the
    /// sidecar lock for the duration of the load. No-op when inactive.
    pub fn prewarm(&self) {
        let Some(arc) = self.inner.as_ref() else {
            return;
        };
        let Some(model_id) = self.model_id.as_deref() else {
            return;
        };
        if let Ok(sidecar) = arc.lock() {
            let _ = sidecar.knn("_prewarm_", 1, model_id, Space::Code);
            tracing::info!("embed sidecar prewarm complete — KNN ready for queries");
        }
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

        // Guarded by the presence check at the top of this method, but a
        // let-else keeps a future refactor from desyncing the two (#414 L2):
        // a missing field is a failed respawn, never a panic.
        let (Some(binary), Some(db_path)) = (self.binary.as_deref(), self.db_path.as_deref())
        else {
            return false;
        };
        let model_id = self.model_id.as_deref().unwrap_or("");

        match EmbedSidecar::spawn(binary, db_path, model_id) {
            Ok(new_sidecar) => {
                let mid = new_sidecar.caps.model_id.clone();
                tracing::info!(model_id = %mid, attempt = self.respawn_count, "embed sidecar respawned");
                self.model_id = Some(mid);
                // #736 item 6: swap the sidecar INSIDE the existing Arc. The
                // injected hook closures hold clones of this Arc — replacing
                // the Arc itself (the previous behaviour) left every hook
                // pointing at the dead child forever, which is what made this
                // whole method dead code in the daemon. Assigning through the
                // guard also drops the old EmbedSidecar, whose Drop impl
                // kills and reaps the dead process.
                match &self.inner {
                    Some(arc) => {
                        let mut guard = arc.lock().unwrap_or_else(|p| p.into_inner());
                        *guard = new_sidecar;
                    }
                    None => self.inner = Some(Arc::new(Mutex::new(new_sidecar))),
                }
                true
            }
            Err(e) => {
                tracing::warn!(
                    attempt = self.respawn_count,
                    "embed sidecar respawn failed: {e}"
                );
                // Keep `inner` (and its Arc identity) so a later successful
                // attempt can still swap in place for the already-injected
                // hooks; clearing it would orphan them permanently.
                false
            }
        }
    }
}
