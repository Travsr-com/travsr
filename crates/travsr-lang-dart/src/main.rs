//! travsr-lang-dart — Dart language sidecar (RFC-013 Direction A).
//!
//! Phase A: delegates to `travsr_analysis::dart::parse` — the exact code
//! the host ran when parsing was in-process, so graph output is identical.
//! Phase B: not provided by this binary; the host's Phase B catalog routes to
//! the official provider when installed.

use travsr_lang_common::{run, LangSidecar};
use travsr_plugin_sdk::Language;

fn main() {
    run(LangSidecar {
        language: Language::Dart,
        extensions: travsr_analysis::dart::CONFIG.extensions,
        parse: travsr_analysis::dart::parse,
        phase_b: None,
    });
}
