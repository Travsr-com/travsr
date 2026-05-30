//! Per-corpus trust gate (ADR-017 Rule 3).
//! Phase B is spawned ONLY after an explicit trust grant in the global config.

use std::collections::HashSet;

pub struct TrustConfig {
    trusted_corpora: HashSet<String>,
}

impl TrustConfig {
    /// Load from environment for now; real config file in P5-S5.
    /// A corpus is trusted if TRAVSR_TRUST_<CORPUS_SANITIZED>=1 is set,
    /// OR if the global config file has plugins.trust.<corpus> = true.
    pub fn new() -> Self {
        Self { trusted_corpora: HashSet::new() }
    }

    pub fn trust(&mut self, corpus: impl Into<String>) {
        self.trusted_corpora.insert(corpus.into());
    }

    pub fn is_trusted(&self, corpus: &str) -> bool {
        self.trusted_corpora.contains(corpus)
    }
}
impl Default for TrustConfig { fn default() -> Self { Self::new() } }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untrusted_by_default() {
        let cfg = TrustConfig::new();
        assert!(!cfg.is_trusted("github.com/acme/repo"));
    }

    #[test]
    fn explicit_trust_grant() {
        let mut cfg = TrustConfig::new();
        cfg.trust("github.com/acme/repo");
        assert!(cfg.is_trusted("github.com/acme/repo"));
        assert!(!cfg.is_trusted("github.com/acme/other"));
    }
}
