use crate::transport::Transport;
use std::collections::HashMap;
use std::sync::Arc;
use travsr_error::IndexError;
use travsr_plugin_protocol::{
    language_from_proto_str, HandshakeResponse, ParseRequest, ParseResponse, PROTOCOL_VERSION,
};

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
        // project that uses `.h` for headers — LLVM, Google style, most of the
        // ecosystem — had its headers parsed by a grammar with no class, no
        // namespace and no template. A header-declared API was invisible to
        // the graph, and its SCIP definitions had nothing to unify against.
        //
        // Nothing in the filename can settle it, so settle it on content, the
        // way clangd and linguist do. Only `.h` pays the read, and only when a
        // C++ plugin is actually registered.
        let (ext, source) = if ext == "h" {
            match std::fs::read_to_string(path) {
                Ok(text) if travsr_analysis::cpp::header_is_cxx(&text) => ("hpp", Some(text)),
                Ok(text) => ("h", Some(text)),
                // Unreadable or not UTF-8: fall through to the extension table
                // and let the parser report the failure, as before.
                Err(_) => ("h", None),
            }
        } else {
            (ext, None)
        };
        match self.by_ext.get(ext) {
            Some(t) => {
                let req = ParseRequest {
                    path: path.to_path_buf(),
                    vname_path: vname_path.to_string(),
                    corpus: corpus.to_string(),
                    package: package.to_string(),
                    // Reuse the text already read for the sniff rather than
                    // making the parser open the file a second time.
                    source: source.map(String::into_bytes),
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
