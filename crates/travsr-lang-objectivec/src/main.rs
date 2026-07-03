//! travsr-lang-objectivec — ObjectiveC language sidecar (RFC-013 Direction A).
//!
//! Phase A: delegates to `travsr_analysis::objc::parse` — the exact code
//! the host ran when parsing was in-process, so graph output is identical.
//! Phase B: not provided by this binary; the host's Phase B catalog routes to
//! the official provider when installed.

use travsr_lang_common::{run, LangSidecar};
use travsr_plugin_sdk::Language;

fn main() {
    run(LangSidecar {
        language: Language::ObjectiveC,
        extensions: travsr_analysis::objc::CONFIG.extensions,
        parse: travsr_analysis::objc::parse,
        phase_b: None,
    });
}
