use std::path::PathBuf;
use serde::{Deserialize, Serialize};
use travsr_core::{Node, Edge};
use crate::ffi_marker::FfiMarker;

/// Current protocol version. Bump on any breaking wire change.
pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParseRequest {
    pub path: PathBuf,
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
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InvokeResponse {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

impl InvokeResponse {
    pub fn unsupported() -> Self { Self::default() }
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
