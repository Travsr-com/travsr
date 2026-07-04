//! travsr-lang-go — Go language sidecar (RFC-013 Direction A).
//!
//! Phase A: delegates to `travsr_analysis::go::parse` — the exact code the
//! host ran when parsing was in-process (including cgo FFI markers), so graph
//! output is identical.
//! Phase B: wraps `scip-go` (catalog args: `--output {output} {root}`) and
//! ingests the SCIP index. Requires `scip-go` in ~/.travsr/bin or on PATH
//! (`travsr lang install go`).

use std::path::Path;

use travsr_lang_common::{find_tool, run, run_scip_tool, scip_output_path, LangSidecar};
use travsr_plugin_sdk::Language;

const EXTENSIONS: &[&str] = &["go"];

fn run_scip_go(root: &Path, scratch: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    let Some(tool) = find_tool("scip-go") else {
        return Ok(None);
    };

    let output = scip_output_path(scratch, "go");
    let bytes = run_scip_tool(
        &tool,
        &[
            "--output".as_ref(),
            output.as_os_str(),
            root.as_os_str(),
        ],
        root,
        &output,
    )?;
    Ok(Some(bytes))
}

fn main() {
    run(LangSidecar {
        language: Language::Go,
        extensions: EXTENSIONS,
        parse: travsr_analysis::go::parse,
        fixture: Some(("probe.go", r#"package probe

import "fmt"

type Point struct{ X, Y int }

func Add(a, b int) int { fmt.Sprint(a); return a + b }
"#)),
        phase_b: Some(run_scip_go),
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
            language: Language::Go,
            extensions: EXTENSIONS,
            parse: travsr_analysis::go::parse,
        fixture: Some(("probe.go", r#"package probe

import "fmt"

type Point struct{ X, Y int }

func Add(a, b int) int { fmt.Sprint(a); return a + b }
"#)),
            phase_b: Some(run_scip_go),
        }
        .invoke_phase_b(&req);
        assert!(resp.nodes.is_empty() && resp.edges.is_empty());
    }
}
