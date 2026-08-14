use std::path::PathBuf;

/// Version of the CLI query protocol carried by [`ControlMessage::Query`].
///
/// Bump this whenever the payload schemas produced by `travsr-mcp::query`
/// (GraphPayload / AskPayload / StatusPayload) change shape. A CLI whose
/// version does not match the daemon's falls back to opening the store
/// directly, so version skew degrades to the slow path instead of
/// mis-rendering (#318 O1).
pub const QUERY_PROTOCOL_VERSION: u32 = 2;

/// Messages sent to the daemon's control plane.
///
/// Serialized as `{"op":"<variant>", ...fields}` (kebab-case tag) so the
/// format matches the `.travsr/daemon-<hex>.sock` wire protocol.
// DEBT(travsr-261): add #[non_exhaustive] once all variant additions for WS2-WS6 are confirmed.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum ControlMessage {
    ReindexCommit {
        sha: String,
    },
    ReindexPaths {
        paths: Vec<PathBuf>,
    },
    Status,
    Shutdown,
    /// WS3 (#420): pause the daemon's auto-reindex and gracefully cancel any
    /// in-flight embed reindex (writes the cancel sentinel, grace-polls the
    /// sidecar, then force-kills as a fallback). The pause is in-memory for the
    /// daemon's lifetime (§4.5) — a restart resumes normal auto-reindex.
    /// Daemons older than this variant answer with a parse error; the CLI
    /// reports the feature as unavailable rather than mis-routing.
    StopEmbed,
    /// WS3: clear the auto-reindex pause set by [`ControlMessage::StopEmbed`].
    ResumeEmbed,
    /// Run a read-only CLI query (`ask`/`graph`/`status`) against the daemon's
    /// warm store (#318 O1). Daemons older than this variant fail to parse the
    /// line and answer with a parse error — the CLI treats that as "route
    /// unavailable" and falls back to a direct store open.
    Query {
        protocol: u32,
        tool: String,
        args: serde_json::Value,
    },
}

/// Response from the daemon's control plane.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ControlResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Echo of [`QUERY_PROTOCOL_VERSION`] on query responses. `None` for
    /// non-query ops and for responses from daemons that predate queries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<u32>,
    /// Query result payload (`travsr-mcp::query` schema). `None` unless the
    /// request was a successful [`ControlMessage::Query`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
}

impl ControlResponse {
    pub fn ok(message: impl Into<Option<String>>) -> Self {
        Self {
            ok: true,
            message: message.into(),
            protocol: None,
            result: None,
        }
    }

    pub fn err(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            message: Some(message.into()),
            protocol: None,
            result: None,
        }
    }

    /// Successful query response carrying `result` and the protocol version.
    pub fn query_result(result: serde_json::Value) -> Self {
        Self {
            ok: true,
            message: None,
            protocol: Some(QUERY_PROTOCOL_VERSION),
            result: Some(result),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_message_round_trips() {
        let msg = ControlMessage::Query {
            protocol: QUERY_PROTOCOL_VERSION,
            tool: "graph".into(),
            args: serde_json::json!({"query": "PaymentService", "depth": 3}),
        };
        let line = serde_json::to_string(&msg).unwrap();
        assert!(line.contains(r#""op":"query""#));
        let back: ControlMessage = serde_json::from_str(&line).unwrap();
        match back {
            ControlMessage::Query { protocol, tool, .. } => {
                assert_eq!(protocol, QUERY_PROTOCOL_VERSION);
                assert_eq!(tool, "graph");
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }

    #[test]
    fn legacy_response_without_query_fields_deserializes() {
        // A pre-#318 daemon answers without `protocol`/`result` — the new CLI
        // must still parse it (and then fall back to direct open).
        let resp: ControlResponse =
            serde_json::from_str(r#"{"ok":false,"message":"parse error: unknown variant"}"#)
                .unwrap();
        assert!(!resp.ok);
        assert!(resp.protocol.is_none());
        assert!(resp.result.is_none());
    }

    #[test]
    fn stop_and_resume_embed_round_trip() {
        for (msg, tag) in [
            (ControlMessage::StopEmbed, "stop-embed"),
            (ControlMessage::ResumeEmbed, "resume-embed"),
        ] {
            let line = serde_json::to_string(&msg).unwrap();
            assert!(line.contains(&format!(r#""op":"{tag}""#)), "got {line}");
            // Must deserialize back to the same variant.
            let back: ControlMessage = serde_json::from_str(&line).unwrap();
            assert_eq!(std::mem::discriminant(&back), std::mem::discriminant(&msg));
        }
    }

    #[test]
    fn query_result_sets_protocol() {
        let resp = ControlResponse::query_result(serde_json::json!({"nodes": []}));
        assert!(resp.ok);
        assert_eq!(resp.protocol, Some(QUERY_PROTOCOL_VERSION));
        assert!(resp.result.is_some());
    }
}
