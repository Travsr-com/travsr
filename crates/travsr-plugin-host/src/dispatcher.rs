use crate::transport::Transport;
use std::collections::HashMap;
use std::sync::Arc;
use travsr_error::IndexError;
use travsr_plugin_protocol::{
    language_from_proto_str, HandshakeResponse, ParseRequest, ParseResponse, PROTOCOL_VERSION,
};

/// Bytes of a `.h` read to decide whether it is C++.
///
/// Bounded, and deliberately smaller than the parser's own 10 MB ceiling. The
/// markers that settle the question are declarations, so they appear early;
/// reading a whole file to find `class ` on line 12 is waste, and reading a
/// generated or adversarial multi-megabyte header in full before the parser's
/// `metadata()` size guard has even run is worse (#708 review). A C++ header
/// whose first 64 KB contains no marker falls through to the C grammar, which
/// is the pre-existing behaviour and the safe direction.
const HEADER_SNIFF_BYTES: usize = 64 * 1024;

/// Whether the `.h` at `path` looks like C++.
///
/// Reads at most [`HEADER_SNIFF_BYTES`] rather than the whole file, and treats
/// an unreadable or non-UTF-8 header as C so the parser reports the real
/// failure exactly as it did before.
fn header_is_cxx_file(path: &std::path::Path) -> bool {
    use std::io::Read as _;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = vec![0u8; HEADER_SNIFF_BYTES];
    let Ok(n) = f.read(&mut buf) else {
        return false;
    };
    buf.truncate(n);
    // Lossy on purpose: a marker is ASCII, so a stray non-UTF-8 byte elsewhere
    // in the header must not stop the question being answered.
    travsr_analysis::cpp::header_is_cxx(&String::from_utf8_lossy(&buf))
}

/// Maps file extension → Transport. Built from plugin handshakes.
/// Replaces the RFC-003 enum-match dispatcher.
pub struct Dispatcher {
    pub corpus: String,
    by_ext: HashMap<String, Arc<dyn Transport>>,
    /// language → supports_phase_b, populated during register().
    phase_b_flags: HashMap<String, bool>,
}

impl Dispatcher {
    pub fn new(corpus: impl Into<String>) -> Self {
        Self {
            corpus: corpus.into(),
            by_ext: HashMap::new(),
            phase_b_flags: HashMap::new(),
        }
    }

    /// Register a plugin transport from its handshake. Fail-fast on version or language mismatch.
    pub fn register(
        &mut self,
        handshake: HandshakeResponse,
        transport: Arc<dyn Transport>,
    ) -> Result<(), IndexError> {
        if handshake.protocol_version != PROTOCOL_VERSION {
            return Err(IndexError::ProtocolVersionMismatch {
                expected: PROTOCOL_VERSION,
                got: handshake.protocol_version,
            });
        }
        if language_from_proto_str(&handshake.language).is_none() {
            return Err(IndexError::UnknownLanguage {
                reported: handshake.language,
            });
        }
        self.phase_b_flags
            .insert(handshake.language.clone(), handshake.supports_phase_b);
        for ext in handshake.extensions {
            self.by_ext.insert(ext, Arc::clone(&transport));
        }
        Ok(())
    }

    /// Return canonical language strings for plugins that declared supports_phase_b = true.
    pub fn phase_b_languages(&self) -> Vec<&str> {
        self.phase_b_flags
            .iter()
            .filter(|(_, &v)| v)
            .map(|(k, _)| k.as_str())
            .collect()
    }

    /// Dispatch a file parse. Returns Ok(None) for unrecognised extensions.
    /// `vname_path` is the repo-relative path used in VName construction.
    pub fn parse_file(
        &self,
        path: &std::path::Path,
        vname_path: &str,
        corpus: &str,
        package: &str,
    ) -> Result<Option<ParseResponse>, IndexError> {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        // `.h` is the one extension two registered languages both have a real
        // claim to, and the table can only hold one. It holds C, so every C++
        // project that uses `.h` for headers (LLVM, Google style, most of the
        // ecosystem) had its headers parsed by a grammar with no class, no
        // namespace and no template. A header-declared API was invisible to
        // the graph, and its SCIP definitions had nothing to unify against.
        //
        // Nothing in the filename can settle it, so settle it on content, the
        // way clangd and linguist do. Only `.h` pays this, and only the prefix
        // it takes to decide.
        let ext = if ext == "h" && header_is_cxx_file(path) {
            "hpp"
        } else {
            ext
        };
        match self.by_ext.get(ext) {
            Some(t) => {
                let req = ParseRequest {
                    path: path.to_path_buf(),
                    vname_path: vname_path.to_string(),
                    corpus: corpus.to_string(),
                    package: package.to_string(),
                    // Left unset: `GenericTreeSitterPlugin`, which backs both
                    // the C and C++ transports, ignores it and re-reads from
                    // `path` anyway. Passing the sniffed text here only looked
                    // like a saving (#708 review).
                    source: None,
                };
                Ok(Some(t.parse(req)?))
            }
            None => Ok(None),
        }
    }

    /// Iterate over unique transports — deduplicates by Arc pointer so Phase B
    /// is invoked once per language, not once per extension.
    pub fn transports(&self) -> impl Iterator<Item = &Arc<dyn Transport>> {
        let mut seen: std::collections::HashSet<*const ()> = std::collections::HashSet::new();
        self.by_ext
            .values()
            .filter(move |t| seen.insert(Arc::as_ptr(*t) as *const ()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_extension_returns_none() {
        let d = Dispatcher::new("github.com/acme/foo");
        let result = d.parse_file(
            std::path::Path::new("main.xyz"),
            "main.xyz",
            "github.com/acme/foo",
            "acme",
        );
        assert!(matches!(result, Ok(None)));
    }

    #[test]
    fn version_mismatch_returns_error() {
        let mut d = Dispatcher::new("github.com/acme/foo");
        use crate::transport::Sidecar;
        let t: Arc<dyn Transport> = Arc::new(Sidecar::stub("go"));
        let bad_handshake = HandshakeResponse {
            protocol_version: 9999,
            plugin_version: "0.1.0".into(),
            language: "go".into(),
            extensions: vec!["go".into()],
            supports_phase_b: false,
        };
        assert!(matches!(
            d.register(bad_handshake, t),
            Err(IndexError::ProtocolVersionMismatch { .. })
        ));
    }
}
