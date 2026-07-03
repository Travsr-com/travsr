//! travsr-lang-c — C language sidecar (RFC-013 Direction A).
//!
//! Phase A: delegates to `travsr_analysis::c::parse` — the exact code the
//! host ran when parsing was in-process, so graph output is identical.
//! Phase B: wraps `scip-clang` (same tool as C++) against the repo's
//! compile_commands.json.

use std::path::Path;

use travsr_lang_common::{find_tool, run, run_scip_tool, scip_output_path, LangSidecar};
use travsr_plugin_sdk::Language;

fn run_scip_clang(root: &Path, scratch: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    let Some(tool) = find_tool("scip-clang") else {
        return Ok(None);
    };

    let compdb = root.join("compile_commands.json");
    anyhow::ensure!(
        compdb.exists(),
        "compile_commands.json not found at {} — generate it first \
         (e.g. cmake -DCMAKE_EXPORT_COMPILE_COMMANDS=ON, or bear -- make)",
        root.display()
    );

    let output = scip_output_path(scratch, "c");
    let bytes = run_scip_tool(
        &tool,
        &[
            "--compdb-path".as_ref(),
            compdb.as_os_str(),
            "--output".as_ref(),
            output.as_os_str(),
        ],
        root,
        &output,
    )?;
    Ok(Some(bytes))
}

fn main() {
    run(LangSidecar {
        language: Language::C,
        extensions: travsr_analysis::c::CONFIG.extensions,
        parse: travsr_analysis::c::parse,
        phase_b: Some(run_scip_clang),
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
            language: Language::C,
            extensions: travsr_analysis::c::CONFIG.extensions,
            parse: travsr_analysis::c::parse,
            phase_b: Some(run_scip_clang),
        }
        .invoke_phase_b(&req);
        assert!(resp.nodes.is_empty() && resp.edges.is_empty());
    }
}
