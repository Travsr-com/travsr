//! Per-corpus trust gate (ADR-017 Rule 3).
//! External (non-builtin) Phase B plugins are spawned ONLY after an explicit
//! per-corpus trust grant in the global config (`travsr lang add <lang>
//! --corpus <corpus>`). Builtin analyzers ship inside the travsr binary and
//! run under Rule 4's first-party rules instead — they are not corpus-gated.
//! Enforced in `PluginIndexer::invoke_phase_b_all` (#414).

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
    /// Override path via `TRAVSR_LANG_TOML` env var (for tests), mirroring
    /// [`registered_languages_from_disk`] so both halves of the lang.toml gate
    /// read the same file. Callers that also need the registered list should
    /// use [`LangToml::from_disk`] to read the file once.
    pub fn from_disk() -> Self {
        LangToml::from_disk().trust_config()
    }
}

/// Both lang.toml gates in one read (#685 review): the `registered` language
/// list and the `trusted_corpora` grants. `registered_languages_from_disk` and
/// `TrustConfig::from_disk` each used to read and TOML-parse the same file
/// independently (with duplicated silently-default-on-failure handling);
/// callers that need both, like `invoke_phase_b_all`, load this once instead.
///
/// Read/parse failures deliberately yield the empty (deny-everything) default:
/// a missing or corrupt lang.toml must fail closed for the trust gate.
#[derive(Default)]
pub struct LangToml {
    pub registered: Vec<String>,
    pub trusted_corpora: HashSet<String>,
}

impl LangToml {
    /// One read + one parse of `~/.travsr/lang.toml` (or the
    /// `TRAVSR_LANG_TOML` test override).
    pub fn from_disk() -> Self {
        let Some(path) = lang_toml_path() else {
            return Self::default();
        };
        let Ok(content) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        let Ok(table) = toml::from_str::<toml::Value>(&content) else {
            return Self::default();
        };
        let str_list = |key: &str| {
            table
                .get(key)
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_string)))
                .into_iter()
                .flatten()
        };
        Self {
            registered: str_list("registered").collect(),
            trusted_corpora: str_list("trusted_corpora").collect(),
        }
    }

    /// The trust half as a [`TrustConfig`], for callers holding a `LangToml`.
    pub fn trust_config(self) -> TrustConfig {
        TrustConfig {
            trusted_corpora: self.trusted_corpora,
        }
    }
}

/// The lang.toml path: `TRAVSR_LANG_TOML` override (for tests) or
/// `~/.travsr/lang.toml`.
fn lang_toml_path() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("TRAVSR_LANG_TOML") {
        return Some(std::path::PathBuf::from(p));
    }
    dirs::home_dir().map(|home| home.join(".travsr").join("lang.toml"))
}

/// Read the `registered` language list from ~/.travsr/lang.toml.
/// Override path via `TRAVSR_LANG_TOML` env var (for tests). Callers that also
/// need the trust grants should use [`LangToml::from_disk`] instead.
pub fn registered_languages_from_disk() -> Vec<String> {
    LangToml::from_disk().registered
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
