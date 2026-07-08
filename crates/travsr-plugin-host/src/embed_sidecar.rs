// EmbedSidecar — subprocess transport for RFC-018 embed plugin binaries.
//
// Mirrors the language-plugin Sidecar in transport.rs but speaks
// EmbedPluginRequest / EmbedPluginResponse instead of PluginRequest /
// PluginResponse. The two transports are completely separate; no shared code
// path, no shared subprocess.

use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use crate::stderr_ring::StderrRing;

use travsr_plugin_protocol::{
    codec::{decode_message, write_message},
    EmbedHandshakeRequest, EmbedPluginRequest, EmbedPluginResponse, EmbedRequest, KnnRequest,
    EMBED_PROTOCOL_VERSION,
};

// ── Error type ────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum EmbedError {
    Spawn(String),
    Handshake(String),
    VersionMismatch { expected: u32, got: u32 },
    Io(String),
    PluginCrashed,
}

impl std::fmt::Display for EmbedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EmbedError::Spawn(e) => write!(f, "embed sidecar spawn failed: {e}"),
            EmbedError::Handshake(e) => write!(f, "embed sidecar handshake failed: {e}"),
            EmbedError::VersionMismatch { expected, got } => {
                write!(
                    f,
                    "embed protocol version mismatch: expected {expected}, got {got}"
                )
            }
            EmbedError::Io(e) => write!(f, "embed sidecar I/O error: {e}"),
            EmbedError::PluginCrashed => write!(f, "embed plugin crashed"),
        }
    }
}

impl std::error::Error for EmbedError {}

// ── Capabilities ──────────────────────────────────────────────────────────────

/// Negotiated capabilities from the embed plugin handshake.
#[derive(Debug, Clone)]
pub struct EmbedCapabilities {
    /// Stable model identifier stored in `node_embeddings.model_id`.
    pub model_id: String,
    /// Raw embedding dimensionality (e.g. 384 for bge-small-en-v1.5).
    pub embedding_dim: u32,
    /// Maximum texts the plugin accepts in one EmbedRequest.
    pub max_batch: u32,
    /// Human-readable backend label for `travsr embed status`.
    pub backend: String,
    pub plugin_version: String,
}

// ── Constants ─────────────────────────────────────────────────────────────────

/// Per-call I/O timeout. If a pipe read/write does not complete within this
/// window the sidecar is killed via SIGTERM (same pattern as indexer.rs C1).
const EMBED_IO_TIMEOUT_SECS: u64 = 30;

// ── I/O pair type alias ───────────────────────────────────────────────────────

type SidecarIo = (BufWriter<ChildStdin>, BufReader<ChildStdout>);

// ── EmbedSidecar ─────────────────────────────────────────────────────────────

/// Manages one `travsr-embed-<backend>` subprocess over framed-JSON IPC.
///
/// Unlike language plugin sidecars which are spawned per indexing run,
/// EmbedSidecar is persistent — it stays alive for the daemon's lifetime so the
/// model is loaded once into the plugin process's memory.
pub struct EmbedSidecar {
    // FT-H4: behind Mutex so is_alive() can call try_wait() without &mut self.
    child: Mutex<Child>,
    // Stable OS PID for the watchdog threads (avoids locking child just for PID).
    pid: u32,
    io: Mutex<SidecarIo>,
    pub caps: EmbedCapabilities,
    alive: Mutex<bool>,
    /// RFC-019: absolute path to embed.db (sibling of graph.db). Forwarded in
    /// KnnRequest so the sidecar opens embed.db with sqlite-vec for ANN — never
    /// the shared graph.db WAL which would reintroduce write contention.
    embed_db_path: PathBuf,
    /// FT-M2: bounded ring buffer draining the sidecar's stderr so model-load
    /// errors (OOM, corrupt weights, missing descriptor) are surfaced in the
    /// daemon log on non-zero exit rather than silently discarded.
    stderr_ring: StderrRing,
}

// FT-C1: kill + reap the child on drop so the 270 MB–1.4 GB model process
// is never orphaned on daemon restart, panic, or CLI query exit.
impl Drop for EmbedSidecar {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

use crate::watchdog::with_io_watchdog;

impl EmbedSidecar {
    /// Spawn the embed plugin binary and perform the capabilities handshake.
    ///
    /// `binary`   — absolute path to `travsr-embed-<backend>`.
    /// `db_path`  — absolute path to the repo's `graph.db` file.
    /// `model_id` — backend catalog ID (e.g. `"bge-base-en-v1.5"`); passed as
    ///              `--model-id` so the sidecar loads the correct ONNX weights.
    pub fn spawn(binary: &Path, db_path: &Path, model_id: &str) -> Result<Self, EmbedError> {
        // Pass --db-path so the sidecar loads the per-repo HNSW index
        // (node IDs are SQLite rowids scoped to one db; a global index causes
        //  cross-repo collisions and wrong KNN results).
        // Pass --model-id so the sidecar selects the right ONNX model weights.
        let mut child = Command::new(binary)
            .arg("--db-path")
            .arg(db_path)
            .arg("--model-id")
            .arg(model_id)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()) // FT-M2: capture for error surfacing
            .spawn()
            .map_err(|e| EmbedError::Spawn(e.to_string()))?;

        let raw_stdin = child
            .stdin
            .take()
            .ok_or_else(|| EmbedError::Spawn("piped stdin not available after spawn".into()))?;
        let raw_stdout = child
            .stdout
            .take()
            .ok_or_else(|| EmbedError::Spawn("piped stdout not available after spawn".into()))?;
        let stderr_ring = if let Some(stderr) = child.stderr.take() {
            StderrRing::spawn(stderr)
        } else {
            // Spawned without piped stderr; create a no-op ring.
            StderrRing::spawn_empty()
        };

        let mut writer = BufWriter::new(raw_stdin);
        let mut reader = BufReader::new(raw_stdout);

        // FT-H5: guard the handshake with a watchdog so a slow/hung model load
        // doesn't block daemon startup indefinitely. A 60-second window covers
        // first-run model download + GPU init; subsequent starts are faster.
        // `with_io_watchdog` returns the instant the handshake decodes, so a
        // healthy spawn is not penalised the full 60 s (see helper).
        let hs_pid = child.id();
        let handshake_result = with_io_watchdog(hs_pid, Duration::from_secs(60), || {
            write_message(
                &mut writer,
                &EmbedPluginRequest::Handshake(EmbedHandshakeRequest {
                    daemon_protocol_version: EMBED_PROTOCOL_VERSION,
                }),
            )
            .map_err(|e| EmbedError::Handshake(e.to_string()))?;
            decode_message::<EmbedPluginResponse>(&mut reader)
                .map_err(|e| EmbedError::Handshake(e.to_string()))
        });

        let hs = match handshake_result {
            Ok(r) => r,
            Err(e) => {
                let tail = stderr_ring.tail();
                if !tail.is_empty() {
                    tracing::warn!(stderr = %tail, "embed sidecar handshake failed");
                }
                return Err(e);
            }
        };

        let caps = match hs {
            EmbedPluginResponse::Handshake(h) => {
                if h.protocol_version != EMBED_PROTOCOL_VERSION {
                    let tail = stderr_ring.tail();
                    if !tail.is_empty() {
                        tracing::warn!(stderr = %tail, "embed sidecar version mismatch");
                    }
                    return Err(EmbedError::VersionMismatch {
                        expected: EMBED_PROTOCOL_VERSION,
                        got: h.protocol_version,
                    });
                }
                EmbedCapabilities {
                    model_id: h.model_id,
                    embedding_dim: h.embedding_dim,
                    max_batch: h.max_batch,
                    backend: h.backend,
                    plugin_version: h.plugin_version,
                }
            }
            _ => {
                let tail = stderr_ring.tail();
                if !tail.is_empty() {
                    tracing::warn!(stderr = %tail, "embed sidecar unexpected handshake response");
                }
                return Err(EmbedError::Handshake(
                    "expected EmbedHandshakeResponse".into(),
                ));
            }
        };

        tracing::debug!(
            model_id = %caps.model_id,
            dim = caps.embedding_dim,
            max_batch = caps.max_batch,
            "embed sidecar handshake ok"
        );

        let pid = child.id();
        Ok(Self {
            pid,
            child: Mutex::new(child),
            io: Mutex::new((writer, reader)),
            caps,
            alive: Mutex::new(true),
            // RFC-019: KNN queries target embed.db, not graph.db, to avoid
            // competing with the daemon's WAL write lock.
            embed_db_path: db_path.with_file_name("embed.db"),
            stderr_ring,
        })
    }

    /// Returns `false` if the plugin has crashed or exited.
    ///
    /// FT-H4: polls the OS via `try_wait` so a silent child exit is detected
    /// before the next I/O call (which would otherwise fail with a broken-pipe
    /// error and mark the sidecar crashed only after the call already failed).
    pub fn is_alive(&self) -> bool {
        if !*self.alive.lock().unwrap_or_else(|e| e.into_inner()) {
            return false;
        }
        if let Ok(mut child) = self.child.lock() {
            // Health-check poll only — not a pipe-drain context. The sidecar's
            // stdout is consumed live via the IPC BufReader; try_wait here just
            // checks exit status without reading any pipe, so no deadlock risk.
            #[allow(clippy::disallowed_methods)]
            if let Ok(Some(status)) = child.try_wait() {
                drop(child);
                let tail = self.stderr_ring.tail();
                if !tail.is_empty() {
                    tracing::warn!(
                        exit = ?status.code(),
                        stderr = %tail,
                        "embed sidecar exited unexpectedly"
                    );
                }
                self.mark_crashed();
                return false;
            }
        }
        true
    }

    fn mark_crashed(&self) {
        if let Ok(mut alive) = self.alive.lock() {
            *alive = false;
        }
    }

    /// Send a batch of texts to embed. Returns one BLOB per input text (same order).
    /// The BLOB layout is backend-defined; the daemon stores it opaquely in
    /// `node_embeddings.embedding`.
    ///
    /// FT-C3: splits `texts` into chunks of at most `caps.max_batch` before each
    /// write to avoid the classic single-thread pipe-buffer deadlock (write huge
    /// frame → pipe fills → read never reached → deadlock).
    /// FT-C2: each chunk's read is guarded by an I/O watchdog thread that kills
    /// the sidecar if no response arrives within EMBED_IO_TIMEOUT_SECS.
    ///
    /// Serialization: this holds `self.io` for the entire request round-trip
    /// (write + blocking read). That is intentional: the sidecar loads ONE model
    /// and runs ONE inference loop (RFC-021), so concurrent callers must serialize
    /// at the transport anyway; narrowing the lock would only move the queue, not
    /// remove it. The per-call `with_io_watchdog` bounds how long any single caller
    /// can hold it (`EMBED_IO_TIMEOUT_SECS`), so one stalled inference cannot wedge
    /// the mutex indefinitely.
    pub fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<u8>>, EmbedError> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        let pid = self.pid;
        let max_batch = (self.caps.max_batch as usize).max(1);
        let mut all_embeddings: Vec<Vec<u8>> = Vec::with_capacity(texts.len());

        for chunk in texts.chunks(max_batch) {
            let req = EmbedPluginRequest::Embed(EmbedRequest {
                texts: chunk.to_vec(),
            });

            let io_lock = self.io.lock().map_err(|_| EmbedError::PluginCrashed)?;
            let (writer, reader) = &mut *{ io_lock };

            write_message(writer, &req).map_err(|e| {
                self.mark_crashed();
                EmbedError::Io(e.to_string())
            })?;

            // FT-C2: watchdog — kill sidecar if the read stalls beyond timeout,
            // but return the instant the response decodes (interruptible).
            let chunk_result =
                with_io_watchdog(pid, Duration::from_secs(EMBED_IO_TIMEOUT_SECS), || {
                    match decode_message::<EmbedPluginResponse>(reader) {
                        Ok(EmbedPluginResponse::Embed(resp)) => Ok(resp.embeddings),
                        Ok(EmbedPluginResponse::Error(e)) => Err(EmbedError::Io(format!(
                            "plugin returned error: {}",
                            e.message
                        ))),
                        Ok(_) => Err(EmbedError::Io("unexpected response type for embed".into())),
                        Err(e) => {
                            self.mark_crashed();
                            Err(EmbedError::Io(e.to_string()))
                        }
                    }
                });

            all_embeddings.extend(chunk_result?);
        }

        Ok(all_embeddings)
    }

    /// Send a KNN query. The sidecar opens `self.embed_db_path` (embed.db) with
    /// sqlite-vec and returns ranked `(node_id, cosine_similarity_score)` pairs
    /// in descending score order.
    ///
    /// FT-C2: guarded by a watchdog thread that kills the sidecar if no response
    /// arrives within EMBED_IO_TIMEOUT_SECS.
    ///
    /// Serialization: this holds `self.io` for the entire request round-trip
    /// (write + blocking read). That is intentional: the sidecar loads ONE model
    /// and runs ONE inference loop (RFC-021), so concurrent callers must serialize
    /// at the transport anyway; narrowing the lock would only move the queue, not
    /// remove it. The per-call `with_io_watchdog` bounds how long any single caller
    /// can hold it (`EMBED_IO_TIMEOUT_SECS`), so one stalled inference cannot wedge
    /// the mutex indefinitely.
    pub fn knn(
        &self,
        query_text: &str,
        k: u32,
        model_id: &str,
    ) -> Result<Vec<(i64, f32)>, EmbedError> {
        let req = EmbedPluginRequest::Knn(KnnRequest {
            db_path: self.embed_db_path.clone(),
            query_text: query_text.to_string(),
            model_id: model_id.to_string(),
            k,
        });

        let pid = self.pid;
        let io_lock = self.io.lock().map_err(|_| EmbedError::PluginCrashed)?;
        let (writer, reader) = &mut *{ io_lock };

        write_message(writer, &req).map_err(|e| {
            self.mark_crashed();
            EmbedError::Io(e.to_string())
        })?;

        // FT-C2: watchdog — kill sidecar if the read stalls beyond timeout, but
        // return the instant the response decodes (interruptible — see helper).
        with_io_watchdog(
            pid,
            Duration::from_secs(EMBED_IO_TIMEOUT_SECS),
            || match decode_message::<EmbedPluginResponse>(reader) {
                Ok(EmbedPluginResponse::Knn(resp)) => {
                    Ok(resp.node_ids.into_iter().zip(resp.scores).collect())
                }
                Ok(EmbedPluginResponse::Error(e)) => Err(EmbedError::Io(format!(
                    "plugin returned error: {}",
                    e.message
                ))),
                Ok(_) => Err(EmbedError::Io("unexpected response type for knn".into())),
                Err(e) => {
                    self.mark_crashed();
                    Err(EmbedError::Io(e.to_string()))
                }
            },
        )
    }
}
