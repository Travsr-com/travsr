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
        Self {
            trusted_corpora: HashSet::new(),
        }
    }

    pub fn trust(&mut self, corpus: impl Into<String>) {
        self.trusted_corpora.insert(corpus.into());
    }

    pub fn is_trusted(&self, corpus: &str) -> bool {
        self.trusted_corpora.contains(corpus)
    }
}
impl Default for TrustConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl TrustConfig {
    /// Load trusted corpora from ~/.travsr/lang.toml (written by `travsr lang add --corpus`).
    pub fn from_disk() -> Self {
        let mut cfg = Self::new();
        let Some(home) = dirs::home_dir() else {
            return cfg;
        };
        let path = home.join(".travsr").join("lang.toml");
        let Ok(content) = std::fs::read_to_string(&path) else {
            return cfg;
        };
        let Ok(table) = toml::from_str::<toml::Value>(&content) else {
            return cfg;
        };
        if let Some(corpora) = table.get("trusted_corpora").and_then(|v| v.as_array()) {
            for c in corpora {
                if let Some(s) = c.as_str() {
                    cfg.trust(s);
                }
            }
        }
        cfg
    }
}

/// Read the `registered` language list from ~/.travsr/lang.toml.
/// Override path via `TRAVSR_LANG_TOML` env var (for tests).
pub fn registered_languages_from_disk() -> Vec<String> {
    let path = if let Ok(p) = std::env::var("TRAVSR_LANG_TOML") {
        std::path::PathBuf::from(p)
    } else {
        let Some(home) = dirs::home_dir() else {
            return vec![];
        };
        home.join(".travsr").join("lang.toml")
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return vec![];
    };
    let Ok(table) = toml::from_str::<toml::Value>(&content) else {
        return vec![];
    };
    table
        .get("registered")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

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
