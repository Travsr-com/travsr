//! travsr-lang-swift — Swift language sidecar (RFC-013 Direction A).
//!
//! Phase A: delegates to `travsr_analysis::swift::parse` — the exact code
//! the host ran when parsing was in-process, so graph output is identical.
//! Phase B: not provided by this binary; the host's Phase B catalog routes to
//! the official provider when installed.

use travsr_lang_common::{run, LangSidecar};
use travsr_plugin_sdk::Language;

fn main() {
    run(LangSidecar {
        language: Language::Swift,
        extensions: travsr_analysis::swift::CONFIG.extensions,
        parse: travsr_analysis::swift::parse,
        fixture: Some(("probe.swift", r#"import Foundation
protocol Shape { func area() -> Int }
class Widget: Shape { func area() -> Int { return 1 } }
func helper() -> Int { return 1 }
"#)),
        phase_b: None,
    });
}
