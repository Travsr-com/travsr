use std::path::PathBuf;

/// Version of the CLI query protocol carried by [`ControlMessage::Query`].
///
/// Bump this whenever the payload schemas produced by `travsr-mcp::query`
/// (GraphPayload / AskPayload / StatusPayload) change shape. A CLI whose
/// version does not match the daemon's falls back to opening the store
/// directly, so version skew degrades to the slow path instead of
/// mis-rendering (#318 O1).
pub const QUERY_PROTOCOL_VERSION: u32 = 1;

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
    /// #688: an editor reports which files it currently sees as broken.
    ///
    /// This is the write half of the *editor plane*: a volatile, externally
    /// owned view that sits beside the graph and never enters it. The graph is
    /// reproducible from the repository; diagnostics depend on which extensions
    /// a particular developer has installed and on the contents of an unsaved
    /// buffer, so they can never be an edge or a node. Keeping them in a
    /// separate plane is what lets both stay true.
    ///
    /// Three properties make this survive more than one editor:
    ///
    /// - **Keyed by `session`**, not by repo. Two windows on one repo, or VS
    ///   Code beside another editor, each hold their own view. A single slot
    ///   would have made the last writer the only truth, with no way to tell
    ///   whose truth it was.
    /// - **Leased, not stored.** `ttl_secs` is how long this report stays
    ///   believable. A window that closes stops renewing and its view expires,
    ///   instead of asserting forever what some editor saw once. `ttl_secs: 0`
    ///   is an explicit detach.
    /// - **Per file, not aggregate.** `4 errors` answers nothing; *which* files
    ///   are broken composes with the graph, which is the entire point.
    ///
    /// Diagnostic *messages* are never sent. They are source-derived text, and
    /// this plane exists to be queried, not to mirror the editor's UI.
    ///
    /// Daemons older than this variant answer with a parse error, which the
    /// extension ignores: a report is never worth surfacing to a user.
    ReportLspDiagnostics {
        /// Stable for the lifetime of one editor window.
        session: String,
        /// How long this report stays valid. `0` detaches the session now.
        ttl_secs: u64,
        /// Only the files that have something wrong. Clean files are absent,
        /// which keeps the payload proportional to breakage, not to repo size.
        files: Vec<FileDiagnostics>,
        /// How many distinct files the editor examined for this report.
        seen: usize,
        /// How many of `seen` had no diagnostic provider reporting at all, and
        /// so are unknown rather than clean.
        undiagnosed: usize,
    },
    /// #688: read the editor plane (`travsr daemon lsp`).
    ///
    /// Answers from memory across all live sessions, dropping expired ones as
    /// it goes. A daemon restart empties it, which is correct: the plane
    /// describes what editors are seeing now, and after a restart the daemon
    /// has not heard from any of them.
    LspStatus,
}

/// One file's current diagnostic state, as an editor sees it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileDiagnostics {
    /// Repo-relative, forward slashes, matching the graph's own path keys so
    /// the two planes can be joined without a translation step.
    pub path: String,
    pub errors: usize,
    pub warnings: usize,
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

    // #688: the extension hand-builds this line in TypeScript
    // (packages/travsr-vscode/src/daemonIpc.ts) rather than sharing a
    // serializer, so the exact wire tag and field names are the contract. If
    // this test is edited, that file has to change with it.
    #[test]
    fn report_lsp_diagnostics_wire_shape_matches_the_extension() {
        let line = r#"{"op":"report-lsp-diagnostics","session":"vscode-1-abc","ttl_secs":900,
            "files":[{"path":"src/a.ts","errors":2,"warnings":1}],"seen":3,"undiagnosed":1}"#;
        match serde_json::from_str::<ControlMessage>(line).expect("extension line must parse") {
            ControlMessage::ReportLspDiagnostics {
                session,
                ttl_secs,
                files,
                seen,
                undiagnosed,
            } => {
                assert_eq!(session, "vscode-1-abc");
                assert_eq!(ttl_secs, 900);
                assert_eq!(seen, 3);
                assert_eq!(undiagnosed, 1);
                assert_eq!(files.len(), 1);
                assert_eq!(files[0].path, "src/a.ts");
                assert_eq!((files[0].errors, files[0].warnings), (2, 1));
            }
            other => panic!("expected ReportLspDiagnostics, got {other:?}"),
        }
    }

    // ttl 0 is the detach signal, so it has to survive the wire as itself
    // rather than being treated as "unset" and defaulted to something alive.
    #[test]
    fn a_zero_lease_parses_as_a_zero_lease() {
        let line = r#"{"op":"report-lsp-diagnostics","session":"s","ttl_secs":0,
            "files":[],"seen":0,"undiagnosed":0}"#;
        match serde_json::from_str::<ControlMessage>(line).expect("detach must parse") {
            ControlMessage::ReportLspDiagnostics { ttl_secs, .. } => assert_eq!(ttl_secs, 0),
            other => panic!("expected ReportLspDiagnostics, got {other:?}"),
        }
    }

    #[test]
    fn lsp_status_round_trips() {
        let line = serde_json::to_string(&ControlMessage::LspStatus).unwrap();
        assert!(line.contains(r#""op":"lsp-status""#), "got {line}");
        assert!(matches!(
            serde_json::from_str::<ControlMessage>(&line),
            Ok(ControlMessage::LspStatus)
        ));
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
