use std::collections::HashMap;
use std::sync::Arc;
use travsr_error::IndexError;
use travsr_plugin_protocol::{
    HandshakeResponse, ParseRequest, ParseResponse, language_from_proto_str, PROTOCOL_VERSION,
};
use crate::transport::Transport;

/// Maps file extension → Transport. Built from plugin handshakes.
/// Replaces the RFC-003 enum-match dispatcher.
pub struct Dispatcher {
    pub corpus: String,
    by_ext: HashMap<String, Arc<dyn Transport>>,
}

impl Dispatcher {
    pub fn new(corpus: impl Into<String>) -> Self {
        Self { corpus: corpus.into(), by_ext: HashMap::new() }
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
            return Err(IndexError::UnknownLanguage { reported: handshake.language });
        }
        for ext in handshake.extensions {
            self.by_ext.insert(ext, Arc::clone(&transport));
        }
        Ok(())
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
        match self.by_ext.get(ext) {
            Some(t) => {
                let req = ParseRequest {
                    path: path.to_path_buf(),
                    vname_path: vname_path.to_string(),
                    corpus: corpus.to_string(),
                    package: package.to_string(),
                    source: None,
                };
                Ok(Some(t.parse(req)?))
            }
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_extension_returns_none() {
        let d = Dispatcher::new("github.com/acme/foo");
        let result = d.parse_file(std::path::Path::new("main.xyz"), "main.xyz", "github.com/acme/foo", "acme");
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
