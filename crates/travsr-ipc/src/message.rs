use std::path::PathBuf;

/// Version of the CLI query protocol carried by [`ControlMessage::Query`].
///
/// Bump this whenever the payload schemas produced by `travsr-mcp::query`
/// (GraphPayload / AskPayload / StatusPayload) change shape. A CLI whose
/// version does not match the daemon's falls back to opening the store
/// directly, so version skew degrades to the slow path instead of
/// mis-rendering (#318 O1).
///
/// v2 (#565 / RFC-002): `travsr graph` ambiguity resolution changed the query
/// wire schema — `GraphQueryArgs` gained `path`, `GraphPayload` gained
/// `candidates`, and `NodeEntry` gained `line`. Without a bump a new CLI could
/// hit an old warm daemon (protocol still v1) that ignores the `path` argument
/// and returns an arbitrary seed with no `candidates`, silently defeating
/// disambiguation. The bump forces that skew onto the direct-open path, where
/// the new CLI runs the ambiguity logic locally.
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
        /// Absolute workspace root this report describes.
        ///
        /// Delivery cannot be trusted to pick the right daemon, so the report
        /// says who it is for and the daemon drops anything that is not its
        /// own (#698 review, P1). Discovery enumerates a *namespace*, not a
        /// repo: on Windows every daemon's pipe matches `travsr-*`, and on
        /// Unix the #592 runtime-directory fallback holds `daemon-<hex>.sock`
        /// for every repo of that user, with the hex opaque to the client.
        /// Without this field the first daemon that accepted the connection
        /// kept the report, so one repo's diagnostics could be answered under
        /// another repo's graph.
        ///
        /// Compared after the same normalization [`ControlAddr::for_repo`]
        /// applies (canonicalize, and lowercase on Windows), so the two agree
        /// on identity by construction.
        repo_root: String,
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
    /// RFC-027 daemon-driven positions: an editor asks the daemon which
    /// references in a dirty file it should resolve.
    ///
    /// The daemon runs the native extractor over the file, keeps the references
    /// its own lexical lane cannot settle, and answers with a
    /// [`LiveResolutionTarget`] per reference — line, name, edge kind, and which
    /// provider to run. The editor then resolves each and reports back with
    /// [`ControlMessage::ReportLiveResolution`]. This replaces the editor's blind
    /// `identifier(` scan, so reference detection lives with the parser and the
    /// graph rather than in an English-shaped regex.
    ///
    /// The answer rides `ControlResponse::result` as a JSON array. Daemons older
    /// than this variant answer with a parse error, which the extension treats as
    /// "no targets" — the live lane simply stays at its lexical floor.
    RequestLiveResolutionTargets {
        /// Absolute workspace root, checked against the daemon's own repo exactly
        /// as `ReportLiveResolution` does (#698 review, P1): discovery enumerates
        /// a namespace, not a repo.
        repo_root: String,
        /// Stable for the lifetime of one editor window.
        session: String,
        /// The dirty file, repo-relative with forward slashes.
        file: String,
        /// Editor buffer version, echoed so the editor can drop a stale batch.
        buffer_version: i64,
    },
    /// RFC-027: an editor reports where a reference in a dirty file actually
    /// resolves to, so the daemon can close the between-commits semantic gap.
    ///
    /// This looks like the editor plane above but is deliberately a different
    /// thing, and the difference is the whole safety argument. #688 keeps
    /// editor data *out* of the graph because diagnostics are the editor's
    /// claim about the code. Here the editor never makes a claim about the
    /// graph: it reports a **position**, and the daemon resolves that position
    /// against its own SCIP-owned identity. The editor cannot name a node,
    /// cannot mint a VName, and cannot say which symbols are related. It
    /// answers exactly one question a language server is authoritative for
    /// ("what does the cursor at this position point at?") and the graph owns
    /// everything downstream of that answer.
    ///
    /// The resulting edges are written with `provenance='live'` and swept when
    /// commit-time SCIP ratifies the region (RFC-027 sections 8.3 and 8.4), so
    /// the durable graph stays SCIP-pinned and deterministic. Non-determinism
    /// is confined to the ephemeral overlay, which is why persisting these does
    /// not reopen what #688 closed.
    ///
    /// Unlike [`ControlMessage::ReportLspDiagnostics`] there is no `ttl_secs`:
    /// a live edge's lifetime is bounded by ratification, not by a lease, so a
    /// TTL here would be a second expiry mechanism with no consumer.
    ///
    /// Daemons older than this variant answer with a parse error, which the
    /// extension ignores. Losing the report costs freshness, never truth.
    ReportLiveResolution {
        /// Absolute workspace root this report describes. Checked against the
        /// daemon's own repo for the same reason as `ReportLspDiagnostics`
        /// (#698 review, P1): discovery enumerates a namespace, not a repo.
        repo_root: String,
        /// Stable for the lifetime of one editor window.
        session: String,
        /// The dirty file these references live in. Repo-relative, forward
        /// slashes, matching the graph's own path keys.
        file: String,
        /// One entry per reference the editor was able to resolve. References
        /// it could not resolve are simply absent: the live lane is fail-closed
        /// (RFC-027 section 8.1), so an omission becomes a `pending` marker
        /// rather than a guessed edge.
        resolutions: Vec<LiveResolution>,
    },
    /// #688: read the editor plane (`travsr daemon lsp`).
    ///
    /// Answers from memory across all live sessions, dropping expired ones as
    /// it goes. A daemon restart empties it, which is correct: the plane
    /// describes what editors are seeing now, and after a restart the daemon
    /// has not heard from any of them.
    LspStatus,
}

/// One resolved reference: where the call site is, and where the editor's
/// language provider says it points.
///
/// Both ends are positions, never identities. The daemon maps them to nodes
/// itself (RFC-027 section 7.5: current Tree-sitter spans for dirty files, SCIP
/// ranges for clean ones), which is what keeps VName minting SCIP's exclusive
/// job.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LiveResolution {
    /// 1-based line of the reference in the dirty file.
    pub ref_line: u32,
    /// 0-based UTF-16 column, as the editor counts them.
    pub ref_col: u32,
    /// The referenced name, used to narrow candidates before position mapping
    /// and to record an honest `pending` row when mapping fails.
    pub name: String,
    /// Repo-relative path the definition lives in, forward slashes. A target
    /// outside the workspace is dropped by the extension rather than sent, so
    /// the live lane stays intra-corpus (RFC-027 section 8.2).
    pub target_path: String,
    /// 1-based line of the definition the editor resolved to.
    pub target_line: u32,
    /// Editor buffer version this answer was computed against. The daemon drops
    /// a resolution whose buffer has moved on rather than trusting a stale
    /// position (RFC-027 section 11).
    pub buffer_version: i64,
    /// The graph edge kind this reference resolves to, as the stable
    /// `EdgeKind::as_str` string (`ref/call`, `ref/field`, `ref/imports`,
    /// `is-implementation`, `overrides`). The editor knows which reference shape
    /// it queried and which provider answered, so it names the kind; the daemon
    /// emits an edge of exactly that kind rather than assuming a call
    /// (RFC-027 live edge-kind scope). `#[serde(default)]` yields `ref/call`,
    /// preserving the pre-expansion payload where every live edge was a call.
    #[serde(default = "default_live_edge_kind")]
    pub edge_kind: String,
}

/// Back-compat default for [`LiveResolution::edge_kind`]: an older extension
/// only ever resolved calls, so a payload without the field means `ref/call`.
fn default_live_edge_kind() -> String {
    "ref/call".to_string()
}

/// One reference the editor should resolve, computed by the daemon from the
/// native extractor's reference set (RFC-027 daemon-driven positions).
///
/// The daemon says *which* reference and *which* edge kind; the editor finds the
/// exact column of `name` on `ref_line`, runs `provider`, and reports the answer
/// back as a [`LiveResolution`]. This is what replaces the editor's blind
/// `identifier(` scan: reference detection moves to the daemon (which has the
/// parser and the graph), leaving the editor only the provider round-trip.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LiveResolutionTarget {
    /// 1-based line of the reference in the dirty file. The editor searches this
    /// line for `name` to recover the column the native extractor does not carry.
    pub ref_line: u32,
    /// The referenced name, so the editor can pin the column and so the daemon
    /// can record an honest `pending` row if the editor's answer maps to nothing.
    pub name: String,
    /// The graph edge kind this reference resolves to, as `EdgeKind::as_str`
    /// (`ref/call`, `ref/field`, `ref/imports`, `is-implementation`,
    /// `overrides`). Echoed back on the [`LiveResolution`] so the daemon emits an
    /// edge of exactly this kind.
    pub edge_kind: String,
    /// Which editor provider answers this reference shape: `definition` (calls,
    /// fields, imports) or `implementation` (implements clauses, overrides).
    pub provider: String,
}

/// The targets in one dependent file the editor should also resolve
/// (RFC-027 section 8.7.5, the interface-edit closure).
///
/// When the saved file adds or renames a symbol other files reference by name,
/// their edges into it were stranded and the editor, which only ever publishes
/// the saved document, never re-resolves them. The daemon names those files
/// here so the same target request restores them, keeping the editor the
/// initiator (§10.1). The editor opens each file, resolves its `targets`, and
/// reports back under that file's own path — so a dependent's edges attach to a
/// definition in the dependent (self-healing on its next save), never to one in
/// the saved file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DependentTargets {
    /// Repo-relative path of the dependent, forward slashes.
    pub file: String,
    /// The references in `file` to resolve, same shape as the saved file's own.
    pub targets: Vec<LiveResolutionTarget>,
}

/// The full answer to a target request: the saved file's own references plus the
/// dependents the interface-edit closure wants re-resolved (RFC-027 §8.7.5).
///
/// Rides `ControlResponse::result` as a JSON object. An extension too old to
/// know this shape reads `result` as an array, finds an object, and treats it as
/// no targets — the live lane simply stays at its lexical floor for that save,
/// which is fail-closed (§8.1), never a wrong edge. New extensions read `own`
/// for the saved document and open each `dependents` file to resolve it.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LiveResolutionTargets {
    /// References in the saved file, resolved against its live buffer.
    pub own: Vec<LiveResolutionTarget>,
    /// Dependent files whose stranded edges this save can restore (§8.7.5).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependents: Vec<DependentTargets>,
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
        let line = r#"{"op":"report-lsp-diagnostics","repo_root":"/home/alice/proj",
            "session":"vscode-1-abc","ttl_secs":900,
            "files":[{"path":"src/a.ts","errors":2,"warnings":1}],"seen":3,"undiagnosed":1}"#;
        match serde_json::from_str::<ControlMessage>(line).expect("extension line must parse") {
            ControlMessage::ReportLspDiagnostics {
                repo_root,
                session,
                ttl_secs,
                files,
                seen,
                undiagnosed,
            } => {
                assert_eq!(repo_root, "/home/alice/proj");
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

    // RFC-027: same contract as the diagnostics line above. The extension
    // hand-builds this in TypeScript (packages/travsr-vscode/src/daemonIpc.ts),
    // so the wire tag and field names here are the contract. If this test is
    // edited, that file has to change with it.
    #[test]
    fn report_live_resolution_wire_shape_matches_the_extension() {
        let line = r#"{"op":"report-live-resolution","repo_root":"/home/alice/proj",
            "session":"vscode-1-abc","file":"src/order.ts",
            "resolutions":[{"ref_line":42,"ref_col":8,"name":"save",
              "target_path":"src/user.ts","target_line":17,"buffer_version":9}]}"#;
        match serde_json::from_str::<ControlMessage>(line).expect("extension line must parse") {
            ControlMessage::ReportLiveResolution {
                repo_root,
                session,
                file,
                resolutions,
            } => {
                assert_eq!(repo_root, "/home/alice/proj");
                assert_eq!(session, "vscode-1-abc");
                assert_eq!(file, "src/order.ts");
                assert_eq!(resolutions.len(), 1);
                let r = &resolutions[0];
                assert_eq!((r.ref_line, r.ref_col), (42, 8));
                assert_eq!(r.name, "save");
                assert_eq!(r.target_path, "src/user.ts");
                assert_eq!(r.target_line, 17);
                assert_eq!(r.buffer_version, 9);
                // An extension too old to send edge_kind means the pre-expansion
                // behaviour: every live edge was a call.
                assert_eq!(
                    r.edge_kind, "ref/call",
                    "a payload without edge_kind defaults to ref/call"
                );
            }
            other => panic!("expected ReportLiveResolution, got {other:?}"),
        }
    }

    // The expanded wire shape: an editor that resolved a field carries the edge
    // kind, so the daemon emits ref/field rather than assuming a call.
    #[test]
    fn a_live_resolution_carries_its_edge_kind() {
        let line = r#"{"op":"report-live-resolution","repo_root":"/r","session":"s",
            "file":"src/zoo.rs","resolutions":[{"ref_line":5,"ref_col":8,"name":"count",
              "target_path":"src/zoo.rs","target_line":2,"buffer_version":3,
              "edge_kind":"ref/field"}]}"#;
        match serde_json::from_str::<ControlMessage>(line).expect("must parse") {
            ControlMessage::ReportLiveResolution { resolutions, .. } => {
                assert_eq!(resolutions[0].edge_kind, "ref/field");
            }
            other => panic!("expected ReportLiveResolution, got {other:?}"),
        }
    }

    // RFC-027 daemon-driven positions: the request the extension sends to ask
    // which references it should resolve.
    #[test]
    fn request_live_resolution_targets_wire_shape_matches_the_extension() {
        let line = r#"{"op":"request-live-resolution-targets","repo_root":"/home/alice/proj",
            "session":"vscode-1-abc","file":"src/order.ts","buffer_version":9}"#;
        match serde_json::from_str::<ControlMessage>(line).expect("request line must parse") {
            ControlMessage::RequestLiveResolutionTargets {
                repo_root,
                session,
                file,
                buffer_version,
            } => {
                assert_eq!(repo_root, "/home/alice/proj");
                assert_eq!(session, "vscode-1-abc");
                assert_eq!(file, "src/order.ts");
                assert_eq!(buffer_version, 9);
            }
            other => panic!("expected RequestLiveResolutionTargets, got {other:?}"),
        }
    }

    // The daemon serialises targets into `ControlResponse::result`; the field
    // names here are the contract the extension parses.
    #[test]
    fn a_resolution_target_serialises_to_the_shape_the_extension_reads() {
        let target = LiveResolutionTarget {
            ref_line: 19,
            name: "save".to_string(),
            edge_kind: "ref/call".to_string(),
            provider: "definition".to_string(),
        };
        let v = serde_json::to_value(&target).expect("serialise");
        assert_eq!(v["ref_line"], 19);
        assert_eq!(v["name"], "save");
        assert_eq!(v["edge_kind"], "ref/call");
        assert_eq!(v["provider"], "definition");
    }

    // RFC-027 section 8.7.5: the target response carries the saved file's own
    // references under `own` and the interface-edit closure under `dependents`,
    // each dependent naming its own file so the editor reports it back keyed
    // there. `dependents` is omitted when empty so a body edit's answer stays the
    // shape it always was, only nested one level under `own`.
    #[test]
    fn a_target_response_carries_own_and_dependents() {
        let resp = LiveResolutionTargets {
            own: vec![LiveResolutionTarget {
                ref_line: 12,
                name: "run".to_string(),
                edge_kind: "ref/call".to_string(),
                provider: "definition".to_string(),
            }],
            dependents: vec![DependentTargets {
                file: "src/main.go".to_string(),
                targets: vec![LiveResolutionTarget {
                    ref_line: 4,
                    name: "Start".to_string(),
                    edge_kind: "ref/call".to_string(),
                    provider: "definition".to_string(),
                }],
            }],
        };
        let v = serde_json::to_value(&resp).expect("serialise");
        assert_eq!(v["own"][0]["name"], "run");
        assert_eq!(v["dependents"][0]["file"], "src/main.go");
        assert_eq!(v["dependents"][0]["targets"][0]["name"], "Start");

        // A body edit yields no dependents, and the field is then absent.
        let bare = LiveResolutionTargets {
            own: vec![],
            dependents: vec![],
        };
        let v = serde_json::to_value(&bare).expect("serialise");
        assert!(v.get("dependents").is_none(), "empty dependents omitted");
        // An old-shape answer (bare array) is not this struct, which is exactly
        // why an old extension reading `result` as an array degrades to no
        // targets rather than mis-parsing.
        let round: LiveResolutionTargets =
            serde_json::from_value(serde_json::json!({"own": []})).expect("default dependents");
        assert!(round.dependents.is_empty());
    }

    // An empty resolution list is meaningful: the editor looked and resolved
    // nothing, which the daemon records as pending rather than as "no report".
    #[test]
    fn an_empty_live_resolution_report_parses() {
        let line = r#"{"op":"report-live-resolution","repo_root":"/r","session":"s",
            "file":"src/a.ts","resolutions":[]}"#;
        match serde_json::from_str::<ControlMessage>(line).expect("empty report must parse") {
            ControlMessage::ReportLiveResolution { resolutions, .. } => {
                assert!(resolutions.is_empty())
            }
            other => panic!("expected ReportLiveResolution, got {other:?}"),
        }
    }

    // ttl 0 is the detach signal, so it has to survive the wire as itself
    // rather than being treated as "unset" and defaulted to something alive.
    #[test]
    fn a_zero_lease_parses_as_a_zero_lease() {
        let line = r#"{"op":"report-lsp-diagnostics","repo_root":"/r","session":"s","ttl_secs":0,
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
