//! travsr-lang-kotlin — Kotlin language sidecar (RFC-013 Direction A).
//!
//! Phase A: delegates to `travsr_analysis::kotlin::parse` — the exact code the
//! host ran when parsing was in-process, so graph output is identical.
//! Phase B: wraps `scip-java index` (scip-java indexes Kotlin via its
//! Gradle/Maven integration) and ingests the SCIP index. Requires an elevated
//! sandbox approval (build tools need network) — see lang.toml.

use std::path::Path;

use travsr_lang_common::{find_tool, run, run_scip_tool, scip_output_path, LangSidecar};
use travsr_plugin_sdk::Language;

fn run_scip_java(root: &Path, scratch: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    let Some(tool) = find_tool("scip-java") else {
        return Ok(None);
    };

    let output = scip_output_path(scratch, "kotlin");
    let bytes = run_scip_tool(
        &tool,
        &["index".as_ref(), "--output".as_ref(), output.as_os_str()],
        root,
        &output,
    )?;
    Ok(Some(bytes))
}

fn main() {
    run(LangSidecar {
        language: Language::Kotlin,
        extensions: travsr_analysis::kotlin::CONFIG.extensions,
        parse: travsr_analysis::kotlin::parse,
        fixture: Some(("probe.kt", r#"import java.time.Instant
class Widget {
    fun draw(): Int = 1
}
object Registry { fun lookup(name: String): String = name }
"#)),
        phase_b: Some(run_scip_java),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use travsr_plugin_protocol::{InvokeRequest, Plugin as _};

    #[test]
    fn phase_b_degrades_gracefully() {
        let tmp = tempfile::tempdir().unwrap();
        let req = InvokeRequest {
            root: tmp.path().to_path_buf(),
            corpus: "github.com/travsr/test".into(),
            scratch: tmp.path().to_path_buf(),
            files: None,
        };
        let resp = LangSidecar {
            language: Language::Kotlin,
            extensions: travsr_analysis::kotlin::CONFIG.extensions,
            parse: travsr_analysis::kotlin::parse,
        fixture: Some(("probe.kt", r#"import java.time.Instant
class Widget {
    fun draw(): Int = 1
}
object Registry { fun lookup(name: String): String = name }
"#)),
            phase_b: Some(run_scip_java),
        }
        .invoke_phase_b(&req);
        assert!(resp.nodes.is_empty() && resp.edges.is_empty());
    }
}
