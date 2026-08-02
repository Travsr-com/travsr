// Embed plugin protocol — RFC-018.
//
// Completely separate from the language Plugin/PluginRequest/PluginResponse
// system. An embed plugin is a different binary (`travsr-embed-<backend>`) that
// speaks a different message envelope over the same framed-JSON codec.
// Language plugins and embed plugins never share a subprocess or message stream.
//
// Wire layout (same as language plugins):
//   [4-byte big-endian length][JSON payload]
// The payload type alternates between EmbedPluginRequest (host→plugin) and
// EmbedPluginResponse (plugin→host).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::types::PluginError;

/// Bump when any EmbedPluginRequest or EmbedPluginResponse field changes in a
/// breaking way. Independent from PROTOCOL_VERSION (language plugins).
pub const EMBED_PROTOCOL_VERSION: u32 = 1;

// ── Trait ────────────────────────────────────────────────────────────────────

/// Implemented once per embed backend (e.g. bge-small-en-v1.5, bge-base-en-v1.5).
/// Stateless and Send+Sync — one instance drives the whole sidecar loop.
///
/// embed_batch and knn are the only two operations the host ever calls.
/// The sidecar binary entry-point is `run_embed_plugin` in travsr-plugin-sdk.
pub trait EmbedPlugin: Send + Sync {
    /// Opaque backend tag — matches the `model_id` column in `node_embeddings`.
    /// Example: `"bge-small-en-v1.5"`, `"bge-base-en-v1.5"`.
    fn model_id(&self) -> &str;

    /// Number of floats per embedding (before any quantisation).
    /// Used by the host to validate response length.
    fn embedding_dim(&self) -> u32;

    /// Human-readable backend label for `travsr embed status`.
    fn backend(&self) -> &str;

    /// The concrete plugin binary's own version (its crate's
    /// `CARGO_PKG_VERSION`), reported in the handshake so the host can gate
    /// version-sensitive features (e.g. `EmbedCapabilities::supports_doc_space`).
    ///
    /// Must be implemented by evaluating `env!("CARGO_PKG_VERSION")` inside the
    /// concrete plugin crate itself (e.g. `travsr-embed`'s `main.rs`), never
    /// here or in `travsr-plugin-sdk`'s runner: `env!` expands using the
    /// *lexically containing* crate's `Cargo.toml`, not the caller's, so a
    /// shared default would report `travsr-plugin-protocol`'s own workspace
    /// version for every backend regardless of which binary is actually
    /// running — the exact bug this method exists to prevent (#376 Phase 2:
    /// `supports_doc_space` silently gated `false` for every sidecar because
    /// `run_embed_plugin` previously hardcoded `env!("CARGO_PKG_VERSION")`
    /// itself).
    fn plugin_version(&self) -> &str;

    /// Maximum texts per `EmbedRequest`. Reported in the handshake so the
    /// daemon can chunk large batches. Defaults to 100.
    fn max_batch(&self) -> u32 {
        100
    }

    /// Embed a batch of texts. Returns one BLOB per input, in the same order.
    /// The BLOB layout is backend-defined; the daemon stores it opaquely.
    fn embed_batch(&self, req: &EmbedRequest) -> EmbedResponse;

    /// K-nearest-neighbour search. The sidecar opens the DB with its own
    /// connection (plus sqlite-vec or brute-force) and returns ranked node ids.
    fn knn(&self, req: &KnnRequest) -> KnnResponse;
}

// ── Handshake ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedHandshakeRequest {
    pub daemon_protocol_version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedHandshakeResponse {
    pub protocol_version: u32,
    pub plugin_version: String,
    /// Must match EmbedPlugin::model_id().
    pub model_id: String,
    /// Dimensionality of the raw (pre-quantised) embedding vector.
    pub embedding_dim: u32,
    /// Human-readable backend label (e.g. "BGE-Small-EN-v1.5 ONNX 384-dim").
    pub backend: String,
    /// Maximum texts per EmbedRequest. Old plugin binaries that pre-date this
    /// field get the safe default of 100 via serde(default).
    #[serde(default = "default_max_batch")]
    pub max_batch: u32,
}

fn default_max_batch() -> u32 {
    100
}

// ── Embed batch ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedRequest {
    /// Texts to embed, in order.
    pub texts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedResponse {
    /// One BLOB per input text, same order as EmbedRequest.texts.
    /// Layout is backend-defined (e.g. 384 × f32 little-endian for bge-small).
    /// The daemon stores these bytes opaquely in node_embeddings.embedding.
    #[serde(with = "embeddings_serde")]
    pub embeddings: Vec<Vec<u8>>,
}

// ── KNN search ───────────────────────────────────────────────────────────────

/// #376 Phase 2: which embedding index a `KnnRequest` searches. A sidecar keeps
/// one HNSW index per space (`hnsw.usearch` for `Code`, `hnsw-docs.usearch` for
/// `Docs`) since the two corpora differ by 2-4 orders of magnitude in size and
/// must never rank against each other (plan §4.1: no cross-modal normalisation).
///
/// Deliberately a single `space` field, not the plan's `Vec<Space>` — the host
/// issues one round trip per space rather than fusing both into one request.
/// A doc query and a code query for the same text embed independently on the
/// host side; latency parity with the historical single-space call is restored
/// by a short-TTL query-embedding memo cache inside the sidecar
/// (`travsr-embed/src/main.rs`) instead of by request-level fusion, which would
/// have required changing the `EmbedKnnHook` closure signature shared by every
/// code-path caller and test double.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Space {
    #[default]
    Code,
    Docs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnnRequest {
    /// Absolute path to the SQLite DB file.
    /// The sidecar opens its own connection (with sqlite-vec or brute-force).
    pub db_path: PathBuf,
    /// Natural-language query to embed and search for.
    pub query_text: String,
    /// Which embedding model's rows to scan (matches node_embeddings.model_id).
    pub model_id: String,
    /// Number of nearest neighbours to return.
    pub k: u32,
    /// #376 Phase 2: which index to search. Missing on the wire (every
    /// pre-#376-Phase-2 host) deserializes to `Space::Code` — the historical,
    /// only-ever-existed behavior. A host must not send `Space::Docs` before
    /// confirming sidecar support (see `MIN_DOC_SPACE_PLUGIN_VERSION` in
    /// travsr-plugin-host) — an old sidecar has no `Docs` index and returns an
    /// empty result rather than an error, which would otherwise look
    /// indistinguishable from "no doc chunk cleared the floor".
    #[serde(default)]
    pub space: Space,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnnResponse {
    /// Nearest-neighbour node_ids, ordered by descending similarity.
    pub node_ids: Vec<i64>,
    /// Corresponding similarity scores in [0.0, 1.0].
    pub scores: Vec<f32>,
}

// ── Top-level envelopes ──────────────────────────────────────────────────────

/// Messages sent by the host (daemon) to the embed plugin sidecar.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EmbedPluginRequest {
    Handshake(EmbedHandshakeRequest),
    Embed(EmbedRequest),
    Knn(KnnRequest),
}

/// Messages sent by the embed plugin sidecar to the host (daemon).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EmbedPluginResponse {
    Handshake(EmbedHandshakeResponse),
    Embed(EmbedResponse),
    Knn(KnnResponse),
    Error(PluginError),
}

// ── Serde helper: Vec<Vec<u8>> as base64 strings ─────────────────────────────
//
// Embedding BLOBs can be large (hundreds of bytes each, many nodes per batch).
// serde_json encodes Vec<u8> as a JSON array of integers by default, which
// inflates a 1 KB blob to ~4 KB of `[1,2,3,...]`. Base64 keeps wire size
// compact without a separate binary framing.
mod embeddings_serde {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[Vec<u8>], s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = s.serialize_seq(Some(v.len()))?;
        for blob in v {
            seq.serialize_element(&BASE64.encode(blob))?;
        }
        seq.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Vec<u8>>, D::Error> {
        let strings: Vec<String> = Vec::deserialize(d)?;
        strings
            .iter()
            .map(|s| {
                BASE64
                    .decode(s.as_bytes())
                    .map_err(serde::de::Error::custom)
            })
            .collect()
    }

    use base64::Engine as _;
    static BASE64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode_message, encode_message};
    use std::io::Cursor;

    #[test]
    fn embed_request_round_trip() {
        let req = EmbedPluginRequest::Embed(EmbedRequest {
            texts: vec!["hello world".into(), "fn main() {}".into()],
        });
        let encoded = encode_message(&req).unwrap();
        let mut cursor = Cursor::new(encoded);
        let decoded: EmbedPluginRequest = decode_message(&mut cursor).unwrap();
        assert!(matches!(decoded, EmbedPluginRequest::Embed(_)));
    }

    #[test]
    fn embed_response_blob_base64_round_trip() {
        let blob: Vec<u8> = (0u8..=255).collect();
        let resp = EmbedPluginResponse::Embed(EmbedResponse {
            embeddings: vec![blob.clone(), blob.clone()],
        });
        let encoded = encode_message(&resp).unwrap();
        let mut cursor = Cursor::new(encoded);
        let decoded: EmbedPluginResponse = decode_message(&mut cursor).unwrap();
        match decoded {
            EmbedPluginResponse::Embed(r) => {
                assert_eq!(r.embeddings.len(), 2);
                assert_eq!(r.embeddings[0], blob);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn knn_request_round_trip() {
        let req = EmbedPluginRequest::Knn(KnnRequest {
            db_path: std::path::PathBuf::from("/tmp/graph.db"),
            query_text: "payment service".into(),
            model_id: "bge-small-en-v1.5".into(),
            k: 10,
            space: Space::Code,
        });
        let encoded = encode_message(&req).unwrap();
        let mut cursor = Cursor::new(encoded);
        let decoded: EmbedPluginRequest = decode_message(&mut cursor).unwrap();
        assert!(matches!(decoded, EmbedPluginRequest::Knn(_)));
    }

    /// #376 Phase 2: a request with no `space` field on the wire (every
    /// pre-#376-Phase-2 host) must deserialize to `Space::Code` — the
    /// historical, only-ever-existed behavior.
    #[test]
    fn knn_request_missing_space_field_defaults_to_code() {
        let json = r#"{"type":"knn","db_path":"/tmp/graph.db","query_text":"payment service","model_id":"bge-small-en-v1.5","k":10}"#;
        let decoded: EmbedPluginRequest = serde_json::from_str(json).unwrap();
        match decoded {
            EmbedPluginRequest::Knn(req) => assert_eq!(req.space, Space::Code),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn knn_request_docs_space_round_trip() {
        let req = EmbedPluginRequest::Knn(KnnRequest {
            db_path: std::path::PathBuf::from("/tmp/graph.db"),
            query_text: "how are floors calibrated".into(),
            model_id: "arctic-embed-m-v1.5".into(),
            k: 3,
            space: Space::Docs,
        });
        let encoded = encode_message(&req).unwrap();
        let mut cursor = Cursor::new(encoded);
        let decoded: EmbedPluginRequest = decode_message(&mut cursor).unwrap();
        match decoded {
            EmbedPluginRequest::Knn(req) => assert_eq!(req.space, Space::Docs),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn handshake_round_trip() {
        let resp = EmbedPluginResponse::Handshake(EmbedHandshakeResponse {
            protocol_version: EMBED_PROTOCOL_VERSION,
            plugin_version: "0.7.0".into(),
            model_id: "bge-small-en-v1.5".into(),
            embedding_dim: 384,
            backend: "BGE-Small-EN-v1.5 ONNX 384-dim".into(),
            max_batch: 100,
        });
        let encoded = encode_message(&resp).unwrap();
        let mut cursor = Cursor::new(encoded);
        let decoded: EmbedPluginResponse = decode_message(&mut cursor).unwrap();
        assert!(matches!(decoded, EmbedPluginResponse::Handshake(_)));
    }

    // Verify the two envelopes are disjoint: an EmbedPluginRequest cannot be
    // decoded as a PluginRequest and vice versa — they share no variant names.
    #[test]
    fn embed_and_language_envelopes_are_disjoint() {
        use crate::types::PluginRequest;
        let embed_req = EmbedPluginRequest::Embed(EmbedRequest {
            texts: vec!["test".into()],
        });
        let encoded = encode_message(&embed_req).unwrap();
        let mut cursor = Cursor::new(encoded);
        // PluginRequest has variants Handshake/Parse/Invoke — "embed" is unknown.
        let result: Result<PluginRequest, _> = decode_message(&mut cursor);
        assert!(
            result.is_err(),
            "embed message must not decode as PluginRequest"
        );
    }
}
