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
    /// Raw embedding dimensionality (e.g. 256 for MRL-256 nomic int8).
    pub embedding_dim: u32,
    /// Maximum texts the plugin accepts in one EmbedRequest.
    pub max_batch: u32,
    /// Human-readable backend label for `travsr embed status`.
    pub backend: String,
    pub plugin_version: String,
}

// ── I/O pair type alias ───────────────────────────────────────────────────────

type SidecarIo = (BufWriter<ChildStdin>, BufReader<ChildStdout>);

// ── EmbedSidecar ─────────────────────────────────────────────────────────────

/// Manages one `travsr-embed-<backend>` subprocess over framed-JSON IPC.
///
/// Unlike language plugin sidecars which are spawned per indexing run,
/// EmbedSidecar is persistent — it stays alive for the daemon's lifetime so the
/// model is loaded once into the plugin process's memory.
pub struct EmbedSidecar {
    #[allow(dead_code)] // held to keep the process alive; drop kills it
    child: Child,
    io: Mutex<SidecarIo>,
    pub caps: EmbedCapabilities,
    alive: Mutex<bool>,
    /// RFC-019: absolute path to embed.db (sibling of graph.db). Forwarded in
    /// KnnRequest so the sidecar opens embed.db with sqlite-vec for ANN — never
    /// the shared graph.db WAL which would reintroduce write contention.
    embed_db_path: PathBuf,
}

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
            .stderr(Stdio::null())
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

        let mut writer = BufWriter::new(raw_stdin);
        let mut reader = BufReader::new(raw_stdout);

        // Handshake — send EmbedHandshakeRequest, receive EmbedHandshakeResponse.
        write_message(
            &mut writer,
            &EmbedPluginRequest::Handshake(EmbedHandshakeRequest {
                daemon_protocol_version: EMBED_PROTOCOL_VERSION,
            }),
        )
        .map_err(|e| EmbedError::Handshake(e.to_string()))?;

        let hs: EmbedPluginResponse =
            decode_message(&mut reader).map_err(|e| EmbedError::Handshake(e.to_string()))?;

        let caps = match hs {
            EmbedPluginResponse::Handshake(h) => {
                if h.protocol_version != EMBED_PROTOCOL_VERSION {
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
                return Err(EmbedError::Handshake(
                    "expected EmbedHandshakeResponse".into(),
                ))
            }
        };

        tracing::debug!(
            model_id = %caps.model_id,
            dim = caps.embedding_dim,
            max_batch = caps.max_batch,
            "embed sidecar handshake ok"
        );

        Ok(Self {
            child,
            io: Mutex::new((writer, reader)),
            caps,
            alive: Mutex::new(true),
            // RFC-019: KNN queries target embed.db, not graph.db, to avoid
            // competing with the daemon's WAL write lock.
            embed_db_path: db_path.with_file_name("embed.db"),
        })
    }

    /// Returns `false` if the plugin has crashed (I/O error on a previous call).
    pub fn is_alive(&self) -> bool {
        *self.alive.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn mark_crashed(&self) {
        if let Ok(mut alive) = self.alive.lock() {
            *alive = false;
        }
    }

    /// Send a batch of texts to embed. Returns one BLOB per input text (same order).
    /// The BLOB layout is backend-defined; the daemon stores it opaquely in
    /// `node_embeddings.embedding`.
    pub fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<u8>>, EmbedError> {
        if texts.is_empty() {
            return Ok(vec![]);
        }

        let req = EmbedPluginRequest::Embed(EmbedRequest {
            texts: texts.to_vec(),
        });

        let io_lock = self.io.lock().map_err(|_| EmbedError::PluginCrashed)?;
        let (writer, reader) = &mut *{ io_lock };

        write_message(writer, &req).map_err(|e| {
            self.mark_crashed();
            EmbedError::Io(e.to_string())
        })?;

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
    }

    /// Send a KNN query. The sidecar opens `self.embed_db_path` (embed.db) with
    /// sqlite-vec and returns ranked `(node_id, cosine_similarity_score)` pairs
    /// in descending score order.
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

        let io_lock = self.io.lock().map_err(|_| EmbedError::PluginCrashed)?;
        let (writer, reader) = &mut *{ io_lock };

        write_message(writer, &req).map_err(|e| {
            self.mark_crashed();
            EmbedError::Io(e.to_string())
        })?;

        match decode_message::<EmbedPluginResponse>(reader) {
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
        }
    }
}
