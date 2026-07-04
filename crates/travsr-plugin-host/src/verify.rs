//! Conformance verification probe (RFC-013 D-4, ADR-019 Rule 2).
//!
//! VERIFY step of the subscription lifecycle: after AUTHORIZE clears a plugin
//! binary to run, the probe checks — inside the sandbox — what protects *our*
//! invariants, never the plugin's semantics:
//!
//! | Tier | Checks |
//! |---|---|
//! | Liveness | spawns, handshakes, protocol_version matches, valid LanguageId |
//! | Structural | well-formed nodes/edges; `node.language == declared id`; edges reference emitted nodes |
//! | Determinism | parse the same bytes twice → byte-identical response |
//!
//! Failure classification follows the ADR-019 reproducibility rule: a failure
//! is HARD (`Rejected`, sticky, hash-keyed) only if it reproduces across all
//! attempts; environmental failures are SOFT (`Unverified`, never attributed
//! to the binary, re-probed on the next trigger).

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use travsr_plugin_protocol::{HandshakeResponse, ParseRequest, ParseResponse};

use crate::resolver::PluginSpec;
use crate::subscriptions::Verdict;
use crate::transport::{Sidecar, Transport};

/// Version of the probe kit — a component of the verdict cache key
/// (ADR-019 Rule 4). Bump when the checks below change so previously cached
/// verdicts are re-earned under the new probe.
pub const PROBE_KIT_VERSION: u32 = 1;

/// Wall-clock deadline per probe parse (ADR-018 `probe_deadline`).
const PROBE_DEADLINE: Duration = Duration::from_secs(10);

/// In-attempt retries for the reproducibility rule (ADR-019 Rule 3).
const PROBE_ATTEMPTS: u32 = 2;
const PROBE_BACKOFF: Duration = Duration::from_millis(250);

enum ProbeFailure {
    /// Reproducible-binary-property candidate. Sticky only if it repeats
    /// across all attempts.
    Hard(String),
    /// Machine property at probe time — never attributed to the binary.
    Soft(String),
}

/// Run the full VERIFY step for a sidecar plugin binary.
///
/// Pre-flight (ADR-019 Rule 3): environment preconditions we own are checked
/// first; their failure yields a SOFT verdict against the environment, never
/// the binary. Returns the verdict plus the successful handshake (for the
/// caller to persist `plugin_version` / `supports_phase_b`).
pub fn verify_sidecar(spec: &PluginSpec) -> (Verdict, Option<HandshakeResponse>) {
    // Pre-flight environment gate.
    let probe_dir = match tempfile::Builder::new().prefix("travsr-probe-").tempdir() {
        Ok(d) => d,
        Err(e) => {
            return (
                Verdict::Unverified {
                    reason: format!("ProbeIoError: cannot create probe dir: {e}"),
                },
                None,
            )
        }
    };

    let mut last_hard: Option<String> = None;
    for attempt in 1..=PROBE_ATTEMPTS {
        match probe_once(spec, probe_dir.path()) {
            Ok(hs) => {
                return (
                    Verdict::Active {
                        supports_phase_b: hs.supports_phase_b,
                    },
                    Some(hs),
                )
            }
            Err(ProbeFailure::Soft(reason)) => {
                tracing::info!(
                    lang = %spec.language,
                    attempt,
                    %reason,
                    "probe SOFT failure — environment, not binary (re-probed on next trigger)"
                );
                return (Verdict::Unverified { reason }, None);
            }
            Err(ProbeFailure::Hard(reason)) => {
                tracing::warn!(lang = %spec.language, attempt, %reason, "probe HARD-class failure");
                last_hard = Some(reason);
                if attempt < PROBE_ATTEMPTS {
                    std::thread::sleep(PROBE_BACKOFF);
                }
            }
        }
    }

    // Reproduced across all attempts → binary-intrinsic → sticky.
    (
        Verdict::Rejected {
            reason: last_hard.unwrap_or_else(|| "ConformanceFailed".into()),
        },
        None,
    )
}

fn probe_once(spec: &PluginSpec, probe_dir: &Path) -> Result<HandshakeResponse, ProbeFailure> {
    // Liveness: spawn + handshake. Spawn failure is a machine property (the
    // resolver already confirmed the binary exists); protocol skew is HARD.
    let (sidecar, hs) = match Sidecar::spawn_with_handshake(spec, probe_dir) {
        Ok(pair) => pair,
        Err(travsr_error::IndexError::ProtocolVersionMismatch { expected, got }) => {
            return Err(ProbeFailure::Hard(format!(
                "ProtocolSkew: expected v{expected}, plugin speaks v{got}"
            )))
        }
        Err(e) => return Err(ProbeFailure::Soft(format!("SpawnFailed: {e}"))),
    };

    // Identity: declared LanguageId must be well-formed and match the catalog
    // entry we are probing (ADR-019 T18 spoof guard).
    if travsr_plugin_protocol::language_id_from_proto_str(&hs.language).is_none() {
        return Err(ProbeFailure::Hard(format!(
            "ConformanceFailed: malformed LanguageId {:?}",
            hs.language
        )));
    }
    if hs.language != spec.language {
        return Err(ProbeFailure::Hard(format!(
            "ConformanceFailed: binary declares language {:?}, catalog expects {:?}",
            hs.language, spec.language
        )));
    }

    // Fixture: plugin-shipped golden fixture, or an empty file fallback.
    let (file_name, source) = match &hs.golden_fixture {
        Some(f) => (f.file_name.clone(), f.source.clone()),
        None => {
            let ext = hs.extensions.first().cloned().unwrap_or_default();
            (format!("probe.{ext}"), String::new())
        }
    };
    let fixture_path = probe_dir.join(&file_name);
    if let Err(e) = std::fs::write(&fixture_path, &source) {
        return Err(ProbeFailure::Soft(format!(
            "ProbeIoError: writing fixture: {e}"
        )));
    }

    // Determinism: parse the same bytes twice → byte-identical responses.
    let sidecar = Arc::new(sidecar);
    let first = parse_with_deadline(&sidecar, &fixture_path, &file_name)?;
    let second = parse_with_deadline(&sidecar, &fixture_path, &file_name)?;

    let a = serde_json::to_vec(&first).map_err(|e| ProbeFailure::Soft(e.to_string()))?;
    let b = serde_json::to_vec(&second).map_err(|e| ProbeFailure::Soft(e.to_string()))?;
    if a != b {
        return Err(ProbeFailure::Hard(
            "NonDeterministic: same input produced different output".into(),
        ));
    }

    // Structural conformance over the first response.
    validate_response(&first, &hs.language).map_err(ProbeFailure::Hard)?;

    Ok(hs)
}

/// Parse with the ADR-018 `probe_deadline` enforced host-side: the parse runs
/// on a helper thread; on deadline the child is killed (which unblocks the
/// helper) and the failure is SOFT (`Timeout` — a machine property).
fn parse_with_deadline(
    sidecar: &Arc<Sidecar>,
    fixture_path: &Path,
    file_name: &str,
) -> Result<ParseResponse, ProbeFailure> {
    let req = ParseRequest {
        path: fixture_path.to_path_buf(),
        vname_path: file_name.to_string(),
        corpus: "travsr.probe".into(),
        package: String::new(),
        source: None,
    };

    let (tx, rx) = std::sync::mpsc::channel();
    let side = Arc::clone(sidecar);
    std::thread::spawn(move || {
        let _ = tx.send(side.parse(req));
    });

    match rx.recv_timeout(PROBE_DEADLINE) {
        Ok(Ok(resp)) => Ok(resp),
        // A crash during parse is a HARD-class candidate; the reproducibility
        // loop in verify_sidecar decides whether it sticks.
        Ok(Err(e)) => Err(ProbeFailure::Hard(format!("ConformanceFailed: {e}"))),
        Err(_) => {
            sidecar.kill();
            Err(ProbeFailure::Soft(format!(
                "Timeout: probe parse exceeded {}s",
                PROBE_DEADLINE.as_secs()
            )))
        }
    }
}

/// Structural conformance (ADR-019 Rule 2): well-formed nodes, language
/// attribution matches the declared id, edges reference emitted nodes.
/// Shared with the D-7 analyzer pairing check.
pub fn validate_response(resp: &ParseResponse, declared_language: &str) -> Result<(), String> {
    validate_nodes_edges(&resp.nodes, &resp.edges, Some(declared_language))
}

/// Core node/edge validation. `declared_language = None` skips the language
/// attribution check (Phase B SCIP output attributes per-file languages).
pub fn validate_nodes_edges(
    nodes: &[travsr_core::Node],
    edges: &[travsr_core::Edge],
    declared_language: Option<&str>,
) -> Result<(), String> {
    let mut ids: HashSet<travsr_core::NodeId> = HashSet::with_capacity(nodes.len());
    for node in nodes {
        if node.vname.signature.is_empty() {
            return Err("ConformanceFailed: node with empty VName signature".into());
        }
        if node.vname.path.is_empty() {
            return Err("ConformanceFailed: node with empty VName path".into());
        }
        if let Some(lang) = declared_language {
            if node.vname.language != lang {
                return Err(format!(
                    "ConformanceFailed: node.language {:?} != declared LanguageId {:?}",
                    node.vname.language, lang
                ));
            }
        }
        ids.insert(node.id);
    }
    for edge in edges {
        if !ids.contains(&edge.src) || !ids.contains(&edge.dst) {
            return Err(
                "ConformanceFailed: edge references a node not present in the response".into(),
            );
        }
    }
    Ok(())
}
