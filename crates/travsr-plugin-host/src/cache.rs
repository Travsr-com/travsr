use std::collections::HashMap;
use travsr_plugin_protocol::ParseResponse;

/// Daemon-computed parse cache key. plugin_version prevents stale cached
/// output after a plugin update; file_hash prevents serving old output for
/// changed files. Neither component is ever supplied by the plugin itself.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub plugin_version: String,
    pub file_hash: [u8; 32],
}

pub struct ParseCache {
    store: HashMap<CacheKey, ParseResponse>,
}

impl ParseCache {
    pub fn new() -> Self {
        Self {
            store: HashMap::new(),
        }
    }
    pub fn get(&self, plugin_version: &str, file_hash: [u8; 32]) -> Option<&ParseResponse> {
        self.store.get(&CacheKey {
            plugin_version: plugin_version.to_string(),
            file_hash,
        })
    }
    pub fn insert(&mut self, key: CacheKey, resp: ParseResponse) {
        self.store.insert(key, resp);
    }
}
impl Default for ParseCache {
    fn default() -> Self {
        Self::new()
    }
}
