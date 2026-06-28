use crate::ffi_marker::FfiMarker;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use travsr_core::{Edge, Node, ScipRef, UnresolvedCall};

/// Current protocol version. Bump on any breaking wire change.
pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParseRequest {
    /// Absolute path on disk — used to read/mmap the file.
    pub path: PathBuf,
    /// Repo-relative path used in VName construction (the stable graph key).
    /// Must match the `vname_path` passed to `Indexer::parse_file_with_vname`.
    pub vname_path: String,
    pub corpus: String,
    pub package: String,
    /// Populated only for git-blob indexing (content not on disk).
    pub source: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ParseResponse {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub ffi_markers: Vec<FfiMarker>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvokeRequest {
    pub root: PathBuf,
    /// Corpus identifier (e.g. `github.com/org/repo`). Used by SCIP ingest to
    /// produce correct VNames. Defaults to empty string for backwards compatibility
    /// with plugin binaries that predate this field.
    #[serde(default)]
    pub corpus: String,
    /// Sandbox-authorized writable scratch directory. Sidecar tools that need to
    /// write temp files (e.g. scip-clang SCIP output, scip-ruby index) MUST place
    /// them under this path — the sandbox's write grant covers only this directory.
    /// Defaults to empty (older daemons) in which case the sidecar may fall back to
    /// `std::env::temp_dir()`, accepting that the sandbox may deny the write.
    #[serde(default)]
    pub scratch: PathBuf,
    /// Pre-walked list of source files for this language (repo-root-relative paths),
    /// forwarded from the daemon's Phase A walk (P6 — #329).
    /// `None` = old daemon that doesn't support P6 → sidecar MUST walk itself.
    /// `Some(paths)` = daemon pre-walked; sidecar SHOULD use this list.
    /// (P1 ensures this is never `Some([])` for a lang without source files —
    /// sidecars for absent languages are never spawned.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InvokeResponse {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    /// G2 attribution records — reference occurrences with call-site line numbers.
    /// Old sidecar binaries omit this field; `#[serde(default)]` provides `[]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<ScipRef>,
    /// Cross-crate calls that Phase B could not resolve to a concrete NodeId.
    /// The daemon resolves these after Phase B using Phase A nodes in the store.
    /// Old sidecar binaries omit this field; `#[serde(default)]` provides `[]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unresolved_calls: Vec<UnresolvedCall>,
}

impl InvokeResponse {
    pub fn unsupported() -> Self {
        Self::default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandshakeRequest {
    pub daemon_protocol_version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandshakeResponse {
    pub protocol_version: u32,
    pub plugin_version: String,
    /// Canonical lowercase language string — must match the normative table in language_map.rs.
    pub language: String,
    pub extensions: Vec<String>,
    pub supports_phase_b: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginRequest {
    Handshake(HandshakeRequest),
    Parse(ParseRequest),
    Invoke(InvokeRequest),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginResponse {
    Handshake(HandshakeResponse),
    Parse(ParseResponse),
    Invoke(InvokeResponse),
    Error(PluginError),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginError {
    pub file: String,
    pub message: String,
}
