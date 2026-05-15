use serde::Deserialize;

/// Incoming JSON-RPC 2.0 message (request or notification).
///
/// A notification has `id: None`; a request has `id: Some(...)`.
/// Notifications must never receive a response.
#[derive(Debug, Deserialize)]
pub struct RpcRequest {
    #[allow(dead_code)]
    pub jsonrpc: String,
    pub id: Option<serde_json::Value>,
    pub method: String,
    pub params: Option<serde_json::Value>,
}

// JSON-RPC 2.0 standard error codes.
pub const PARSE_ERROR: i32 = -32700;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
