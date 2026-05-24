//! Cross-language FFI marker types (RFC-005).
//!
//! Each per-language indexer populates `FfiMarker` records in `ParseOutput`.
//! The second-pass `ffi_resolver` consumes them to emit `EdgeKind::FFICall` edges.

use travsr_core::NodeId;

/// The kind of FFI boundary a marker represents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FfiMarkerKind {
    /// Rust `#[napi]` or `#[napi(js_name = "x")]` export (TS↔Rust via napi-rs).
    NapiExport,
    /// Rust `#[pyfunction]` or `#[pyo3(name = "x")]` export (Python↔Rust via PyO3).
    PyO3Export,
    /// Go `//export <name>` directive (Go↔C via cgo); destination is synthetic.
    CgoExport,
    /// TypeScript function declaration in a napi-emitted `.d.ts` (call-site).
    NapiCall,
    /// Python call-site: `.pyi` stub adjacent to `_*.so` OR `from pkg import _rust_module`.
    PyO3Call,
    /// Go `C.<name>` call-site in a cgo file.
    GoCallC,
}

/// A single FFI boundary marker emitted by a per-language indexer.
///
/// The `ffi_resolver` matches markers across language boundaries to emit
/// `Edge::ffi_call` edges with a calibrated confidence score.
#[derive(Debug, Clone)]
pub struct FfiMarker {
    /// The definition or call-site node that carries this marker.
    pub node_id: NodeId,
    pub kind: FfiMarkerKind,
    /// Name of the symbol on *this* side of the FFI boundary.
    pub local_name: String,
    /// Explicit alias if present (`js_name`, `pyo3(name = "...")`).
    pub bound_name: Option<String>,
    /// Argument count, if known from the signature.
    pub arity: Option<u8>,
    /// Module/package scope used to narrow name-index lookups.
    pub module: Option<String>,
    /// Corpus of the node — used to enforce the corpus-scoping invariant.
    pub corpus: String,
}

impl FfiMarker {
    /// Construct a validated `FfiMarker`, enforcing RFC-005 sanitisation rules:
    /// - `local_name` and `bound_name` must be ≤ 256 bytes
    /// - Must not contain control characters `\x00..=\x1F`, `\x7F`, or leading `<`
    /// - Must not contain newlines
    ///
    /// Returns `None` and logs a warning if validation fails (name is dropped,
    /// not truncated — truncation creates spoofing opportunities).
    pub fn try_new(
        node_id: NodeId,
        kind: FfiMarkerKind,
        local_name: impl Into<String>,
        bound_name: Option<impl Into<String>>,
        arity: Option<u8>,
        module: Option<impl Into<String>>,
        corpus: impl Into<String>,
    ) -> Option<Self> {
        let local_name = local_name.into();
        let bound_name = bound_name.map(Into::into);

        if !is_valid_marker_name(&local_name) {
            tracing::warn!(
                name = %local_name,
                "ffi marker local_name failed validation — dropping marker"
            );
            return None;
        }
        if let Some(ref bn) = bound_name {
            if !is_valid_marker_name(bn) {
                tracing::warn!(
                    name = %bn,
                    "ffi marker bound_name failed validation — dropping marker"
                );
                return None;
            }
        }

        Some(Self {
            node_id,
            kind,
            local_name,
            bound_name,
            arity,
            module: module.map(Into::into),
            corpus: corpus.into(),
        })
    }

    /// The effective name used for cross-language matching.
    /// Prefers `bound_name` (explicit alias) over `local_name`.
    pub fn effective_name(&self) -> &str {
        self.bound_name.as_deref().unwrap_or(&self.local_name)
    }
}

fn is_valid_marker_name(s: &str) -> bool {
    if s.is_empty() || s.len() > 256 {
        return false;
    }
    if s.starts_with('<') {
        return false;
    }
    !s.chars()
        .any(|c| matches!(c, '\x00'..='\x1F' | '\x7F') || c == '\n' || c == '\r')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_id() -> NodeId {
        travsr_core::NodeId(42)
    }

    #[test]
    fn try_new_accepts_valid_name() {
        let m = FfiMarker::try_new(
            dummy_id(),
            FfiMarkerKind::NapiExport,
            "add",
            None::<String>,
            Some(2),
            None::<String>,
            "corpus",
        );
        assert!(m.is_some());
        assert_eq!(m.unwrap().local_name, "add");
    }

    #[test]
    fn try_new_rejects_control_chars() {
        let m = FfiMarker::try_new(
            dummy_id(),
            FfiMarkerKind::NapiExport,
            "bad\x00name",
            None::<String>,
            None,
            None::<String>,
            "corpus",
        );
        assert!(m.is_none());
    }

    #[test]
    fn try_new_rejects_too_long() {
        let long = "a".repeat(257);
        let m = FfiMarker::try_new(
            dummy_id(),
            FfiMarkerKind::NapiExport,
            long,
            None::<String>,
            None,
            None::<String>,
            "corpus",
        );
        assert!(m.is_none());
    }

    #[test]
    fn effective_name_prefers_bound_name() {
        let m = FfiMarker::try_new(
            dummy_id(),
            FfiMarkerKind::NapiExport,
            "greet_impl",
            Some("greet"),
            None,
            None::<String>,
            "corpus",
        )
        .unwrap();
        assert_eq!(m.effective_name(), "greet");
    }

    #[test]
    fn try_new_rejects_leading_angle_bracket() {
        let m = FfiMarker::try_new(
            dummy_id(),
            FfiMarkerKind::CgoExport,
            "<cgo_synthetic>",
            None::<String>,
            None,
            None::<String>,
            "corpus",
        );
        assert!(m.is_none());
    }
}
