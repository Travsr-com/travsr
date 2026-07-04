//! travsr-lang-scala — Scala language sidecar (RFC-013 Direction A).
//!
//! Phase A: delegates to `travsr_analysis::scala::parse` — the exact code
//! the host ran when parsing was in-process, so graph output is identical.
//! Phase B: not provided by this binary; the host's Phase B catalog routes to
//! the official provider when installed.

use travsr_lang_common::{run, LangSidecar};
use travsr_plugin_sdk::Language;

fn main() {
    run(LangSidecar {
        language: Language::Scala,
        extensions: travsr_analysis::scala::CONFIG.extensions,
        parse: travsr_analysis::scala::parse,
        fixture: Some(("probe.scala", r#"package probe
import scala.collection.mutable
class Widget { def draw(): Int = 1 }
object Main { def run(): Unit = () }
trait Shape { def area(): Int }
"#)),
        phase_b: None,
    });
}
