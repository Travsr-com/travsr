//! Sidecar process supervisor.
//! P5-S1: type definitions and crash-tracking stubs. Real spawn in P5-S3.

use std::collections::HashMap;
use tracing::warn;

const MAX_CRASHES: u32 = 3;

pub struct Supervisor {
    crash_counts: HashMap<String, u32>,
    disabled: HashMap<String, String>,
}

impl Supervisor {
    pub fn new() -> Self {
        Self {
            crash_counts: HashMap::new(),
            disabled: HashMap::new(),
        }
    }

    pub fn record_crash(&mut self, language: &str, reason: &str) {
        let count = self.crash_counts.entry(language.to_string()).or_insert(0);
        *count += 1;
        warn!("plugin crash #{} for {language}: {reason}", count);
        if *count >= MAX_CRASHES {
            self.disabled
                .insert(language.to_string(), reason.to_string());
            warn!("plugin {language} permanently disabled for this daemon run (>{MAX_CRASHES} crashes)");
        }
    }

    pub fn is_disabled(&self, language: &str) -> bool {
        self.disabled.contains_key(language)
    }
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}
