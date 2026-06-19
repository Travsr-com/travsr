//! ONNX embedding support for RFC-012 A2 F2 (L2-B semantic search).
//!
//! Compiled only when the `embeddings` feature is active. All public items
//! are called through feature-gated hooks in `SqliteStore`; they are not
//! re-exported from the crate root.
//!
//! Model: nomic-embed-text-v1.5 (int8 ONNX, 137 MB).
//! Output: MRL-256 — first 256 dims of the model's 768-dim output, L2-normalised.

use std::{
    path::Path,
    sync::{Mutex, OnceLock},
};

use anyhow::Context as _;
use ndarray::{Axis, Ix3};
use ort::session::{builder::GraphOptimizationLevel, Session};
use rusqlite::Connection;
use tokenizers::Tokenizer;

use crate::{NodeId, StoreError};

struct EmbedState {
    session: Session,
    tokenizer: Tokenizer,
}

// Mutex because Session::run takes &mut self in ort 2.0.
// Inference is CPU-bound and single-threaded per our config (intra_threads=1),
// so lock contention is negligible for the incremental-update write path.
static STATE: OnceLock<Mutex<EmbedState>> = OnceLock::new();

/// Load the ONNX model and tokenizer from `model_dir`.
///
/// Expected files in `model_dir`:
/// - `model_int8.onnx` — nomic-embed-text-v1.5 int8 ONNX export
/// - `tokenizer.json` — HuggingFace tokenizer config
///
/// Idempotent: if already loaded, returns `Ok(())` immediately.
/// Run `travsr embed init` to download the files and call this.
pub fn init_model(model_dir: &Path) -> Result<(), StoreError> {
    if STATE.get().is_some() {
        return Ok(());
    }
    let onnx_path = model_dir.join("model_int8.onnx");
    let tok_path = model_dir.join("tokenizer.json");

    let session = Session::builder()
        .map_err(|e| StoreError::Database(format!("ort builder: {e}")))?
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(|e| StoreError::Database(format!("ort opt level: {e}")))?
        .with_intra_threads(1)
        .map_err(|e| StoreError::Database(format!("ort threads: {e}")))?
        .commit_from_file(&onnx_path)
        .map_err(|e| StoreError::Database(format!("loading {}: {e}", onnx_path.display())))?;

    let tokenizer = Tokenizer::from_file(&tok_path)
        .map_err(|e| StoreError::Database(format!("loading {}: {e}", tok_path.display())))?;

    // OnceLock::set fails only if already set — the race is harmless.
    let _ = STATE.set(Mutex::new(EmbedState { session, tokenizer }));
    Ok(())
}

/// Returns `true` if the model has been loaded via [`init_model`].
///
/// Used by write-path hooks to skip embedding when the model isn't initialised
/// (e.g. during `travsr init` before `travsr embed init` has been run).
pub fn model_loaded() -> bool {
    STATE.get().is_some()
}

/// Embed `text` with a nomic task prefix and return an MRL-256 unit vector.
///
/// `task_prefix` must be one of the nomic-embed-text task strings:
/// - `"search_document: "` — indexing (write path)
/// - `"search_query: "`    — retrieval (read path / ANN search)
///
/// The model outputs 768 dims; we take the first 256 and L2-normalise.
fn embed_text(task_prefix: &str, text: &str) -> Result<[f32; 256], StoreError> {
    let mutex = STATE
        .get()
        .ok_or_else(|| StoreError::Database("embed model not initialised".into()))?;
    let mut state = mutex
        .lock()
        .map_err(|_| StoreError::Database("embed state mutex poisoned".into()))?;

    let prefixed = format!("{task_prefix}{text}");
    let encoding = state
        .tokenizer
        .encode(prefixed, false)
        .map_err(|e| StoreError::Database(format!("tokenisation: {e}")))?;

    let seq_len = encoding.len();
    let ids: Vec<i64> = encoding.get_ids().iter().map(|&x| x as i64).collect();
    let mask: Vec<i64> = encoding
        .get_attention_mask()
        .iter()
        .map(|&x| x as i64)
        .collect();

    // Build input tensors.  The ([shape], &slice) tuple form is ort's raw
    // tensor API — no ndarray allocation required for inputs.
    let t_ids = ort::value::TensorRef::from_array_view(([1, seq_len], &*ids))
        .map_err(|e| StoreError::Database(format!("input_ids tensor: {e}")))?;
    let t_mask = ort::value::TensorRef::from_array_view(([1, seq_len], &*mask))
        .map_err(|e| StoreError::Database(format!("attention_mask tensor: {e}")))?;

    // ort::inputs! returns a Vec directly — the ? is on session.run, not inputs!.
    let outputs = state
        .session
        .run(ort::inputs!["input_ids" => t_ids, "attention_mask" => t_mask])
        .map_err(|e| StoreError::Database(format!("ort inference: {e}")))?;

    // Extract output[0] — may be [1, 768] (pre-pooled) or [1, seq_len, 768]
    // (last_hidden_state). The ndarray ort feature is required for try_extract_array.
    let raw = outputs[0]
        .try_extract_array::<f32>()
        .map_err(|e| StoreError::Database(format!("extracting model output: {e}")))?;

    const HIDDEN: usize = 768;

    let full: Vec<f32> = match raw.ndim() {
        2 => {
            // [1, 768] — already mean-pooled by the ONNX graph.
            raw.as_slice()
                .ok_or_else(|| StoreError::Database("non-contiguous 2-D output".into()))?
                .to_vec()
        }
        3 => {
            // [1, seq_len, 768] — last_hidden_state; mean-pool over token axis.
            let r3 = raw
                .into_dimensionality::<Ix3>()
                .map_err(|e| StoreError::Database(format!("reshape to Ix3: {e}")))?;
            let n_tokens = r3.shape()[1];
            let mut pooled = vec![0f32; HIDDEN];
            for s in 0..n_tokens {
                for (d, &v) in r3.index_axis(Axis(1), s).iter().enumerate() {
                    if d < HIDDEN {
                        pooled[d] += v;
                    }
                }
            }
            let factor = n_tokens as f32;
            pooled.iter_mut().for_each(|x| *x /= factor);
            pooled
        }
        n => {
            return Err(StoreError::Database(format!("unexpected output ndim {n}")));
        }
    };

    if full.len() < 256 {
        return Err(StoreError::Database(format!(
            "model output dim {} < 256 — wrong model?",
            full.len()
        )));
    }

    // MRL-256: first 256 dims, L2-normalised.
    let mut result = [0f32; 256];
    result.copy_from_slice(&full[..256]);
    let norm: f32 = result.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-8 {
        result.iter_mut().for_each(|x| *x /= norm);
    }

    Ok(result)
}

/// Upsert the embedding for `node_id` into `node_embeddings`.
///
/// No-ops silently if the model is not loaded, so the incremental write path
/// works without embeddings before `travsr embed init` has been run.
/// Errors from the model or the INSERT are returned to the caller (who should
/// log and continue — never let an embed failure block a node write).
pub fn put_node_embedding(
    conn: &Connection,
    node_id: NodeId,
    sig: &str,
    kind: &str,
    path: &str,
) -> anyhow::Result<()> {
    if !model_loaded() {
        return Ok(());
    }
    let text = format!("{sig} {kind} {path}");
    let vec = embed_text("search_document: ", &text).map_err(|e| anyhow::anyhow!("{e}"))?;
    // vec0 FLOAT[256] columns accept raw IEEE 754 little-endian bytes.
    // rusqlite can bind &[u8] as a blob; &[f32] has no ToSql impl.
    let bytes: Vec<u8> = vec.iter().flat_map(|f| f.to_le_bytes()).collect();
    conn.execute(
        "INSERT OR REPLACE INTO node_embeddings(node_id, embedding) VALUES(?1, ?2)",
        rusqlite::params![node_id.0 as i64, bytes],
    )
    .context("upserting node_embeddings")?;
    Ok(())
}

/// Remove the embedding row for `node_id` from `node_embeddings`.
///
/// Called from `delete_node_fts` so embeddings stay in sync with the node table.
pub fn delete_node_embedding(conn: &Connection, node_id: NodeId) -> anyhow::Result<()> {
    conn.execute(
        "DELETE FROM node_embeddings WHERE node_id = ?1",
        rusqlite::params![node_id.0 as i64],
    )
    .context("deleting from node_embeddings")?;
    Ok(())
}

/// ANN search via vec0 KNN. Returns `NodeId`s ordered by distance (closest first).
///
/// Returns an empty `Vec` immediately when the model is not loaded so callers
/// never need to guard against the uninitialized case themselves.
///
/// `k` is the maximum number of neighbours to return (typically 20 for Step 4
/// in `search_nodes_fuzzy`).
pub fn vec_search(conn: &Connection, query: &str, k: usize) -> Result<Vec<NodeId>, StoreError> {
    if !model_loaded() {
        return Ok(Vec::new());
    }
    // Use "search_query: " prefix for retrieval — nomic task-prefix contract.
    let query_vec = embed_text("search_query: ", query)?;
    // vec0 MATCH expects the same raw LE-bytes blob format used for inserts.
    let bytes: Vec<u8> = query_vec.iter().flat_map(|f| f.to_le_bytes()).collect();

    let mut stmt = conn
        .prepare(
            "SELECT node_id FROM node_embeddings \
             WHERE embedding MATCH ?1 AND k = ?2",
        )
        .map_err(|e| StoreError::Database(format!("preparing vec0 KNN: {e}")))?;

    let ids = stmt
        .query_map(rusqlite::params![bytes, k as i64], |row| {
            let id_i64: i64 = row.get(0)?;
            Ok(NodeId(id_i64 as u64))
        })
        .map_err(|e| StoreError::Database(format!("executing vec0 KNN: {e}")))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| StoreError::Database(format!("collecting vec0 KNN results: {e}")))?;

    Ok(ids)
}
