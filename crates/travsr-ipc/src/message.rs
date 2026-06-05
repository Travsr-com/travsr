use std::path::PathBuf;

/// Messages sent to the daemon's control plane.
///
/// Serialized as `{"op":"<variant>", ...fields}` (kebab-case tag) so the
/// format matches the `.travsr/daemon-<hex>.sock` wire protocol.
// DEBT(travsr-261): add #[non_exhaustive] once all variant additions for WS2-WS6 are confirmed.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum ControlMessage {
    ReindexCommit { sha: String },
    ReindexPaths { paths: Vec<PathBuf> },
    Status,
    Shutdown,
}

/// Response from the daemon's control plane.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ControlResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl ControlResponse {
    pub fn ok(message: impl Into<Option<String>>) -> Self {
        Self {
            ok: true,
            message: message.into(),
        }
    }

    pub fn err(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            message: Some(message.into()),
        }
    }
}
