//! Second-pass cross-language FFI edge resolver (RFC-005).
//!
//! After all per-language indexers have populated a `ParseOutput`, this module
//! matches `FfiMarker` records across language boundaries and emits
//! `EdgeKind::FFICall` edges with calibrated confidence scores.
//!
//! # Corpus invariant (SEC P1-C1)
//! Only markers sharing the same corpus are matched. Cross-corpus FFI edges
//! are silently dropped — this prevents confidence-spoofing attacks.

use std::collections::HashMap;

use travsr_core::{Edge, NodeId};

use crate::ffi::{FfiMarker, FfiMarkerKind};
#[cfg(test)]
use crate::ParseOutput;

/// Configuration for FFI edge resolution. Populated from `[ffi]` in travsr.toml.
#[derive(Debug, Clone)]
pub struct FfiConfig {
    /// Global kill switch. When false, resolver returns immediately.
    pub enabled: bool,
    /// Minimum confidence to emit an edge (0..=100). Default: 30.
    pub emit_threshold: u8,
    /// Timeout passed to pyright (seconds). Default: 30.
    pub pyright_timeout_secs: u64,
}

impl Default for FfiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            emit_threshold: 30,
            pyright_timeout_secs: 30,
        }
    }
}

/// Name index key: (corpus, language, effective_name_normalized).
type IndexKey = (String, String, String);

/// Second-pass resolver: matches FFI markers across language boundaries.
pub struct Resolver {
    /// Maps (corpus, language, normalized_name) → list of (NodeId, arity).
    name_index: HashMap<IndexKey, Vec<(NodeId, Option<u8>)>>,
}

impl Resolver {
    /// Build the name index from a slice of markers.
    ///
    /// This is the canonical entry point for repo-level cross-file resolution
    /// (RFC-005): call this after all per-language parsers have run and their
    /// markers have been accumulated into a single slice.
    ///
    /// O(M) where M = number of FfiMarkers.
    pub fn build_from_markers(markers: &[FfiMarker]) -> Self {
        let mut name_index: HashMap<IndexKey, Vec<(NodeId, Option<u8>)>> = HashMap::new();

        for marker in markers {
            let lang = marker_language(marker);
            let key: IndexKey = (
                marker.corpus.clone(),
                lang.to_string(),
                normalize_name(marker.effective_name()),
            );
            name_index
                .entry(key)
                .or_default()
                .push((marker.node_id, marker.arity));
        }

        Self { name_index }
    }

    /// Build the name index from all markers in `output`.
    ///
    /// Convenience wrapper for single-file tests; prefer `build_from_markers`
    /// for production repo-level resolution.
    #[cfg(test)]
    pub fn build(output: &ParseOutput) -> Self {
        Self::build_from_markers(&output.ffi_markers)
    }

    /// Resolve all FFI pairs and return the edges to add to the graph.
    ///
    /// Enforces corpus invariant: `src.corpus == dst.corpus` (SEC P1-C1).
    pub fn resolve(&self, markers: &[FfiMarker], cfg: &FfiConfig) -> Vec<Edge> {
        if !cfg.enabled {
            return Vec::new();
        }

        let mut edges = Vec::new();

        for src_marker in markers {
            type ScoreFn = fn(&FfiMarker, Option<u8>) -> u8;
            let (target_lang, score_fn): (&str, ScoreFn) = match src_marker.kind {
                // Call-site markers look for their export counterpart
                FfiMarkerKind::NapiCall => ("rust", score_napi),
                FfiMarkerKind::PyO3Call => ("rust", score_pyo3),
                // CgoExport is on the Go side; looks for the synthetic "c" target.
                FfiMarkerKind::CgoExport => ("c", score_cgo),
                // JniCall: call into Java via JNI — looks for "java" export target.
                FfiMarkerKind::JniCall => ("java", score_jni as ScoreFn),
                // Indexed-target markers — skip as sources
                FfiMarkerKind::NapiExport
                | FfiMarkerKind::PyO3Export
                | FfiMarkerKind::GoCallC
                | FfiMarkerKind::JniExport => continue,
            };

            // Corpus invariant: only match within the same corpus.
            let key: IndexKey = (
                src_marker.corpus.clone(),
                target_lang.to_string(),
                normalize_name(src_marker.effective_name()),
            );

            let Some(candidates) = self.name_index.get(&key) else {
                tracing::warn!(
                    name = %src_marker.effective_name(),
                    target_lang,
                    "ffi_resolver: no matching export found"
                );
                continue;
            };

            for &(dst_id, dst_arity) in candidates {
                let confidence = score_fn(src_marker, dst_arity);

                tracing::debug!(
                    src = ?src_marker.node_id,
                    dst = ?dst_id,
                    confidence,
                    "ffi_resolver: candidate scored"
                );

                if confidence < cfg.emit_threshold {
                    continue;
                }

                edges.push(Edge::ffi_call(src_marker.node_id, dst_id, confidence));

                // Emit reverse RefCall edge when confidence is high (RFC-005 §4).
                if confidence >= 80 {
                    edges.push(Edge::new(
                        dst_id,
                        src_marker.node_id,
                        travsr_core::EdgeKind::RefCall,
                    ));
                }
            }
        }

        edges
    }
}

/// Normalize a name for cross-language fuzzy matching.
/// Lowercases and strips underscores and hyphens.
fn normalize_name(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Determine the language string for a marker's node side.
///
/// This controls which key slot the marker occupies in the name index.
/// Export-side markers are indexed as targets; call-site markers are sources.
/// For cgo: CgoExport lives on the Go function ("go"), GoCallC lives on the
/// synthetic C node ("c") so the resolver can look it up as a "c" target.
fn marker_language(m: &FfiMarker) -> &'static str {
    match m.kind {
        FfiMarkerKind::NapiExport | FfiMarkerKind::PyO3Export => "rust",
        FfiMarkerKind::CgoExport => "go",
        FfiMarkerKind::NapiCall => "typescript",
        FfiMarkerKind::PyO3Call => "python",
        FfiMarkerKind::GoCallC => "c",
        // JniExport: the Java side is the target; indexed under "java".
        FfiMarkerKind::JniExport => "java",
        // JniCall: the call originates from Rust/Go; source is not Java.
        FfiMarkerKind::JniCall => "rust",
    }
}

// ── Confidence scorers ────────────────────────────────────────────────────────
// RFC-005 §5 confidence rubric.

fn score_napi(src: &FfiMarker, dst_arity: Option<u8>) -> u8 {
    // Arity match: both known and equal → 90; unknown → 50.
    match (src.arity, dst_arity) {
        (Some(a), Some(b)) if a == b => 90,
        (None, _) | (_, None) => 50,
        _ => 30, // arities known but mismatch — heuristic still emitted
    }
}

fn score_pyo3(src: &FfiMarker, dst_arity: Option<u8>) -> u8 {
    // bound_name match (explicit pyo3(name=...)) boosts confidence.
    let base: u8 = match (src.arity, dst_arity) {
        (Some(a), Some(b)) if a == b => 90,
        (None, _) | (_, None) => 50,
        _ => 30,
    };
    // If a bound_name alias was used to find this match, add 20 → cap at 100
    if src.bound_name.is_some() {
        base.saturating_add(20).min(100)
    } else {
        base
    }
}

fn score_cgo(src: &FfiMarker, _dst_arity: Option<u8>) -> u8 {
    // cgo //export + matching C name = exact → 90; no arity info available.
    if src.bound_name.is_none() {
        90
    } else {
        70
    }
}

fn score_jni(_src: &FfiMarker, _dst_arity: Option<u8>) -> u8 {
    // JNI name matching is always exact (JNI mangling is deterministic).
    90
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::{FfiMarker, FfiMarkerKind};
    use travsr_core::NodeId;

    fn napi_export(node_id: u64, name: &str, arity: Option<u8>) -> FfiMarker {
        FfiMarker::try_new(
            NodeId(node_id),
            FfiMarkerKind::NapiExport,
            name,
            None::<String>,
            arity,
            None::<String>,
            "corpus:test",
        )
        .unwrap()
    }

    fn napi_call(node_id: u64, name: &str, arity: Option<u8>) -> FfiMarker {
        FfiMarker::try_new(
            NodeId(node_id),
            FfiMarkerKind::NapiCall,
            name,
            None::<String>,
            arity,
            None::<String>,
            "corpus:test",
        )
        .unwrap()
    }

    fn output_with_markers(markers: Vec<FfiMarker>) -> ParseOutput {
        ParseOutput {
            nodes: vec![],
            edges: vec![],
            ffi_markers: markers,
        }
    }

    #[test]
    fn resolver_emits_edge_for_matching_napi_pair() {
        let export = napi_export(1, "add", Some(2));
        let call_site = napi_call(2, "add", Some(2));

        let output = output_with_markers(vec![export, call_site.clone()]);
        let resolver = Resolver::build(&output);
        let cfg = FfiConfig::default();
        let edges = resolver.resolve(&output.ffi_markers, &cfg);

        // Should have a FFICall edge from call-site to export
        assert!(
            edges.iter().any(|e| e.src == NodeId(2)
                && e.dst == NodeId(1)
                && e.kind == travsr_core::EdgeKind::FFICall),
            "expected FFICall edge from call site to export"
        );
    }

    #[test]
    fn resolver_enforces_corpus_invariant() {
        // Export in corpus A, call site in corpus B — must produce no edge.
        let export = FfiMarker::try_new(
            NodeId(1),
            FfiMarkerKind::NapiExport,
            "add",
            None::<String>,
            Some(2),
            None::<String>,
            "corpus:A",
        )
        .unwrap();
        let call_site = FfiMarker::try_new(
            NodeId(2),
            FfiMarkerKind::NapiCall,
            "add",
            None::<String>,
            Some(2),
            None::<String>,
            "corpus:B",
        )
        .unwrap();

        let output = output_with_markers(vec![export, call_site]);
        let resolver = Resolver::build(&output);
        let edges = resolver.resolve(&output.ffi_markers, &FfiConfig::default());

        assert!(
            edges.is_empty(),
            "corpus invariant violated: cross-corpus edge emitted"
        );
    }

    #[test]
    fn resolver_respects_emit_threshold() {
        let export = napi_export(1, "thing", None); // arity unknown → score 50
        let call_site = napi_call(2, "thing", None);

        let output = output_with_markers(vec![export, call_site]);
        let resolver = Resolver::build(&output);

        let cfg_high = FfiConfig {
            emit_threshold: 80,
            ..Default::default()
        };
        let edges = resolver.resolve(&output.ffi_markers, &cfg_high);
        assert!(
            edges.is_empty(),
            "should not emit when confidence < threshold"
        );

        let cfg_low = FfiConfig {
            emit_threshold: 30,
            ..Default::default()
        };
        let edges = resolver.resolve(&output.ffi_markers, &cfg_low);
        assert!(
            !edges.is_empty(),
            "should emit when confidence >= threshold"
        );
    }

    #[test]
    fn resolver_disabled_returns_no_edges() {
        let export = napi_export(1, "add", Some(2));
        let call_site = napi_call(2, "add", Some(2));
        let output = output_with_markers(vec![export, call_site]);
        let resolver = Resolver::build(&output);

        let cfg = FfiConfig {
            enabled: false,
            ..Default::default()
        };
        let edges = resolver.resolve(&output.ffi_markers, &cfg);
        assert!(edges.is_empty());
    }

    #[test]
    fn normalize_name_lowercases_and_strips_separators() {
        assert_eq!(normalize_name("getUserName"), "getusername");
        assert_eq!(normalize_name("get_user_name"), "getusername");
        assert_eq!(normalize_name("add"), "add");
    }

    #[test]
    fn score_napi_arity_match_gives_90() {
        let m = napi_call(1, "add", Some(2));
        assert_eq!(score_napi(&m, Some(2)), 90);
    }

    #[test]
    fn score_napi_unknown_arity_gives_50() {
        let m = napi_call(1, "add", None);
        assert_eq!(score_napi(&m, None), 50);
    }
}
