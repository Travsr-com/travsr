//! Language subscription verdict cache (RFC-013 D-4/D-5, ADR-019 Rules 3–6).
//!
//! Persistent, machine-local record of verification verdicts at
//! `~/.travsr/subscriptions.lock` (override: `TRAVSR_SUBSCRIPTIONS_FILE`).
//! Never committed to a repo — a verdict is binary- and arch-specific, and a
//! committed verdict would be an admission-bypass vector (T17).
//!
//! Two separate questions, never conflated:
//! - "Is this binary structurally sound?" → this cache, global, hash-keyed.
//! - "Is *this repo* allowed to load it?" → per-corpus trust (`trust.rs`).
//!
//! The cache is **advisory, not authoritative** (T17): the binary hash is
//! re-checked at lookup (gated on mtime+size so an unchanged binary is not
//! re-hashed every spawn), and the ADR-017 trust gate still runs regardless
//! of a cached `Active`.
//!
//! HARD/SOFT split (ADR-019 Rule 3): only `Active` and `Rejected` are
//! persisted against the binary hash. `Unverified` (machine faults) is held
//! in-memory for the current process only — the binary is re-probed on the
//! next trigger, never punished for bad luck.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::resolver::PluginSpec;
use crate::verify::{verify_sidecar, PROBE_KIT_VERSION};
use travsr_plugin_protocol::PROTOCOL_VERSION;

/// Subscription verdict (replaces enum-membership as the admission gate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Admitted. `supports_phase_b` is the binary's declared capability; the
    /// effective tier (`Active(A+B)` vs `Active(A only)`, RFC-013 D-6) also
    /// requires the Phase B toolchain, which is re-checked live at each
    /// registration so a toolchain appearing later auto-upgrades the tier
    /// with no re-probe.
    Active { supports_phase_b: bool },
    /// HARD — reproducible binary fault. Sticky per sha256; cleared only by a
    /// changed binary or an explicit `--force` re-verify.
    Rejected { reason: String },
    /// SOFT — machine fault at probe time. Never attributed to the binary.
    Unverified { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    language: String,
    binary_path: String,
    sha256: String,
    plugin_version: String,
    protocol_version: u32,
    probe_kit_version: u32,
    /// "active" | "rejected" — SOFT verdicts are never persisted (Rule 3).
    verdict: String,
    #[serde(default)]
    supports_phase_b: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    verified_at_unix: u64,
    /// mtime+size fast path: if both match the binary on disk, reuse `sha256`
    /// without re-hashing (ADR-019 Rule 5).
    mtime_secs: u64,
    size: u64,
}

/// D-7: a shared Phase B analyzer, stored once and referenced by languages.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AnalyzerEntry {
    /// Underlying tool binary name (e.g. "scip-java") — the catalog `command`.
    tool: String,
    sha256: String,
    verified_at_unix: u64,
    mtime_secs: u64,
    size: u64,
}

/// D-7: (analyzer, language) pairing — proves the analyzer emits valid output
/// *for this language*, not just that the binary is sound.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PairingEntry {
    analyzer_sha256: String,
    language: String,
    verified_at_unix: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct LockFile {
    #[serde(default)]
    entries: Vec<Entry>,
    #[serde(default)]
    analyzers: Vec<AnalyzerEntry>,
    #[serde(default)]
    pairings: Vec<PairingEntry>,
}

struct Manager {
    lock: LockFile,
    /// Per-process decision memo — guarantees at most one probe per language
    /// per process even when the daemon's parallel workers all register
    /// sidecars concurrently.
    decisions: HashMap<String, Verdict>,
}

static MANAGER: OnceLock<Mutex<Manager>> = OnceLock::new();

fn manager() -> &'static Mutex<Manager> {
    MANAGER.get_or_init(|| {
        Mutex::new(Manager {
            lock: load_lock_file(),
            decisions: HashMap::new(),
        })
    })
}

pub fn lock_file_path() -> PathBuf {
    if let Some(p) = std::env::var_os("TRAVSR_SUBSCRIPTIONS_FILE") {
        return PathBuf::from(p);
    }
    dirs::home_dir()
        .unwrap_or_default()
        .join(".travsr")
        .join("subscriptions.lock")
}

fn load_lock_file() -> LockFile {
    let path = lock_file_path();
    match std::fs::read_to_string(&path) {
        Ok(s) => toml::from_str(&s).unwrap_or_else(|e| {
            tracing::warn!(path = %path.display(), err = %e, "corrupt subscriptions.lock — starting fresh");
            LockFile::default()
        }),
        Err(_) => LockFile::default(),
    }
}

fn save_lock_file(lock: &LockFile) {
    let path = lock_file_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match toml::to_string_pretty(lock) {
        Ok(s) => {
            let header = "# Auto-generated verdict cache (ADR-019). Machine-local — do not commit.\n";
            if let Err(e) = std::fs::write(&path, format!("{header}{s}")) {
                tracing::warn!(path = %path.display(), err = %e, "failed to write subscriptions.lock");
            }
        }
        Err(e) => tracing::warn!(err = %e, "failed to serialise subscriptions.lock"),
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn file_stamp(path: &Path) -> Option<(u64, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some((mtime, meta.len()))
}

/// sha256 of a file, hex-encoded. Reads the whole file — call behind the
/// mtime+size gate.
pub fn hash_file_hex(path: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Some(hex::encode(hasher.finalize()))
}

mod hex {
    pub fn encode(bytes: impl AsRef<[u8]>) -> String {
        bytes
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

/// SUBSCRIBE→AUTHORIZE→VERIFY→ADMIT decision for one sidecar language
/// (RFC-013 D-4). Called from `register_phase_a_sidecars`; the caller admits
/// (registers the transport) only on `Active`.
///
/// - AUTHORIZE: the caller has already applied ADR-017 gates (sandbox
///   available; binary resolved from trusted install locations). ADR-020
///   signature verification is pending sidecar release signing and is
///   documented as an accepted gap.
/// - VERIFY: cached verdict by `(language, sha256, protocol, probe_kit)` when
///   valid; otherwise the conformance probe runs now (once per process).
/// - `force = true` re-probes regardless of any cached verdict, including a
///   HARD `Rejected` (ADR-019 Rule 5 escape hatch).
pub fn check_subscription(spec: &PluginSpec, force: bool) -> Verdict {
    let mut mgr = manager().lock().expect("subscription manager poisoned");

    if !force {
        if let Some(v) = mgr.decisions.get(&spec.language) {
            return v.clone();
        }
    }

    let binary = Path::new(&spec.program);
    let Some((mtime, size)) = file_stamp(binary) else {
        // Unavailable{BinaryMissing}: verdict retained, nothing to probe.
        return Verdict::Unverified {
            reason: "Unavailable: binary missing on disk".into(),
        };
    };

    // mtime+size fast path → reuse recorded hash; else re-hash.
    let recorded = mgr
        .lock
        .entries
        .iter()
        .find(|e| e.language == spec.language && e.binary_path == spec.program);
    let sha = match recorded {
        Some(e) if e.mtime_secs == mtime && e.size == size => e.sha256.clone(),
        _ => match hash_file_hex(binary) {
            Some(h) => h,
            None => {
                return Verdict::Unverified {
                    reason: "ProbeIoError: cannot hash binary".into(),
                }
            }
        },
    };

    // Cache hit by the full key tuple (ADR-019 Rule 4). The verdict survives
    // delete/restore and path moves because it is hash-keyed (Rule 5).
    if !force {
        if let Some(e) = mgr.lock.entries.iter().find(|e| {
            e.language == spec.language
                && e.sha256 == sha
                && e.protocol_version == PROTOCOL_VERSION
                && e.probe_kit_version == PROBE_KIT_VERSION
        }) {
            let verdict = match e.verdict.as_str() {
                "active" => Verdict::Active {
                    supports_phase_b: e.supports_phase_b,
                },
                _ => Verdict::Rejected {
                    reason: e.reason.clone().unwrap_or_else(|| "ConformanceFailed".into()),
                },
            };
            mgr.decisions.insert(spec.language.clone(), verdict.clone());
            return verdict;
        }
    }

    // VERIFY: run the conformance probe now.
    tracing::info!(lang = %spec.language, binary = %spec.program, "verifying plugin (conformance probe)");
    let (verdict, handshake) = verify_sidecar(spec);

    // Persist HARD outcomes only (Rule 3); SOFT stays in-memory.
    match &verdict {
        Verdict::Active { supports_phase_b } => {
            let plugin_version = handshake
                .as_ref()
                .map(|h| h.plugin_version.clone())
                .unwrap_or_default();
            upsert_entry(
                &mut mgr.lock,
                Entry {
                    language: spec.language.clone(),
                    binary_path: spec.program.clone(),
                    sha256: sha,
                    plugin_version,
                    protocol_version: PROTOCOL_VERSION,
                    probe_kit_version: PROBE_KIT_VERSION,
                    verdict: "active".into(),
                    supports_phase_b: *supports_phase_b,
                    reason: None,
                    verified_at_unix: now_unix(),
                    mtime_secs: mtime,
                    size,
                },
            );
            save_lock_file(&mgr.lock);
        }
        Verdict::Rejected { reason } => {
            upsert_entry(
                &mut mgr.lock,
                Entry {
                    language: spec.language.clone(),
                    binary_path: spec.program.clone(),
                    sha256: sha,
                    plugin_version: String::new(),
                    protocol_version: PROTOCOL_VERSION,
                    probe_kit_version: PROBE_KIT_VERSION,
                    verdict: "rejected".into(),
                    supports_phase_b: false,
                    reason: Some(reason.clone()),
                    verified_at_unix: now_unix(),
                    mtime_secs: mtime,
                    size,
                },
            );
            save_lock_file(&mgr.lock);
        }
        Verdict::Unverified { .. } => {}
    }

    mgr.decisions.insert(spec.language.clone(), verdict.clone());
    verdict
}

fn upsert_entry(lock: &mut LockFile, entry: Entry) {
    // One entry per (language, sha) key; replace stale entries for the same
    // language+path so the file does not grow unboundedly.
    lock.entries.retain(|e| {
        !(e.language == entry.language
            && (e.sha256 == entry.sha256 || e.binary_path == entry.binary_path))
    });
    lock.entries.push(entry);
}

// ── D-7: shared analyzers + (analyzer, language) pairing ─────────────────────

/// Resolve + hash the underlying Phase B analyzer for `language` from the
/// catalog `command` (mtime+size gated). Returns None when the tool is absent.
fn analyzer_sha_for(mgr: &mut Manager, language: &str) -> Option<String> {
    let entry = crate::phase_b::catalog::lookup(language)?;
    let tool_path = crate::resolver::which_binary(entry.command)?;
    let (mtime, size) = file_stamp(Path::new(&tool_path))?;

    if let Some(a) = mgr
        .lock
        .analyzers
        .iter()
        .find(|a| a.tool == entry.command && a.mtime_secs == mtime && a.size == size)
    {
        return Some(a.sha256.clone());
    }
    let sha = hash_file_hex(Path::new(&tool_path))?;
    mgr.lock.analyzers.retain(|a| a.tool != entry.command);
    mgr.lock.analyzers.push(AnalyzerEntry {
        tool: entry.command.to_string(),
        sha256: sha.clone(),
        verified_at_unix: now_unix(),
        mtime_secs: mtime,
        size,
    });
    save_lock_file(&mgr.lock);
    Some(sha)
}

/// D-7 pairing gate: validate one language's Phase B output before it merges
/// into the graph; on first success record the `(analyzer, language)` pairing.
///
/// Returns `true` when the output may be admitted. Invalid output is held out
/// of the graph (auto-onboard rule: "silent" means no prompt, never "admit
/// unverified nodes") — Phase A is unaffected.
pub fn check_phase_b_pairing(
    language: &str,
    nodes: &[travsr_core::Node],
    edges: &[travsr_core::Edge],
) -> bool {
    if nodes.is_empty() && edges.is_empty() {
        return true; // nothing to admit
    }

    // Structural validation of the analyzer output (language attribution is
    // per-file in SCIP output — a JVM analyzer legitimately emits several
    // languages — so the declared-language check is skipped here).
    if let Err(reason) = crate::verify::validate_nodes_edges(nodes, edges, None) {
        tracing::warn!(
            lang = language,
            %reason,
            "Phase B output failed the pairing check — held out of the graph"
        );
        return false;
    }

    let mut mgr = manager().lock().expect("subscription manager poisoned");
    if let Some(sha) = analyzer_sha_for(&mut mgr, language) {
        let known = mgr
            .lock
            .pairings
            .iter()
            .any(|p| p.analyzer_sha256 == sha && p.language == language);
        if !known {
            tracing::info!(
                lang = language,
                analyzer_sha = %sha,
                "recorded (analyzer, language) pairing — auto-onboard verified"
            );
            mgr.lock.pairings.push(PairingEntry {
                analyzer_sha256: sha,
                language: language.to_string(),
                verified_at_unix: now_unix(),
            });
            save_lock_file(&mgr.lock);
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_file_roundtrip() {
        let lock = LockFile {
            entries: vec![Entry {
                language: "cpp".into(),
                binary_path: "/x/travsr-lang-cpp".into(),
                sha256: "abc".into(),
                plugin_version: "0.7.0".into(),
                protocol_version: PROTOCOL_VERSION,
                probe_kit_version: PROBE_KIT_VERSION,
                verdict: "active".into(),
                supports_phase_b: true,
                reason: None,
                verified_at_unix: 1,
                mtime_secs: 2,
                size: 3,
            }],
            analyzers: vec![],
            pairings: vec![PairingEntry {
                analyzer_sha256: "abc".into(),
                language: "kotlin".into(),
                verified_at_unix: 1,
            }],
        };
        let s = toml::to_string_pretty(&lock).unwrap();
        let back: LockFile = toml::from_str(&s).unwrap();
        assert_eq!(back.entries.len(), 1);
        assert_eq!(back.entries[0].language, "cpp");
        assert!(back.entries[0].supports_phase_b);
        assert_eq!(back.pairings.len(), 1);
    }

    #[test]
    fn missing_binary_is_soft_unavailable() {
        let spec = PluginSpec {
            language: "nosuchlang".into(),
            program: "/nonexistent/travsr-lang-nosuch".into(),
            args: vec![],
            policy: crate::sandbox::policy::SandboxPolicy::Standard,
        };
        match check_subscription(&spec, false) {
            Verdict::Unverified { reason } => assert!(reason.contains("Unavailable")),
            other => panic!("expected Unverified, got {other:?}"),
        }
    }
}
