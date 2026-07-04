//! travsr-lang-cpp — C++ language sidecar (RFC-013 Direction A).
//!
//! Phase A: delegates to `travsr_analysis::cpp::parse` — the exact code the
//! host ran when parsing was in-process, so graph output is identical.
//! Phase B: wraps `scip-clang` against the repo's compile_commands.json and
//! ingests the SCIP index. Requires `scip-clang` in ~/.travsr/bin or on PATH
//! (`travsr lang install cpp`).

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

    let output = scip_output_path(scratch, "cpp");
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
        language: Language::Cpp,
        extensions: travsr_analysis::cpp::CONFIG.extensions,
        parse: travsr_analysis::cpp::parse,
        fixture: Some(("probe.cpp", r#"#include <vector>
namespace probe {
class Widget { public: void draw(); };
struct Pod { int n; };
int compute(int x) { return x * 2; }
}
"#)),
        phase_b: Some(run_scip_clang),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use travsr_plugin_protocol::{InvokeRequest, Plugin as _};

    fn sidecar() -> LangSidecar {
        LangSidecar {
            language: Language::Cpp,
            extensions: travsr_analysis::cpp::CONFIG.extensions,
            parse: travsr_analysis::cpp::parse,
        fixture: Some(("probe.cpp", r#"#include <vector>
namespace probe {
class Widget { public: void draw(); };
struct Pod { int n; };
int compute(int x) { return x * 2; }
}
"#)),
            phase_b: Some(run_scip_clang),
        }
    }

    /// Phase B must degrade to an empty response when scip-clang (or the
    /// compile database) is absent — never panic or error the wire.
    #[test]
    fn phase_b_degrades_gracefully() {
        let tmp = tempfile::tempdir().unwrap();
        let req = InvokeRequest {
            root: tmp.path().to_path_buf(),
            corpus: "github.com/travsr/test".into(),
            scratch: tmp.path().to_path_buf(),
            files: None,
        };
        let resp = sidecar().invoke_phase_b(&req);
        assert!(resp.nodes.is_empty() && resp.edges.is_empty());
    }

    /// Phase A through the sidecar must produce the same structural nodes the
    /// in-process path produced.
    #[test]
    fn phase_a_extracts_structure() {
        use std::io::Write as _;
        let mut f = tempfile::Builder::new().suffix(".cpp").tempfile().unwrap();
        f.write_all(b"#include <vector>\nnamespace app { class Widget { public: void draw(); }; }\n")
            .unwrap();
        f.flush().unwrap();

        let req = travsr_plugin_protocol::ParseRequest {
            path: f.path().to_path_buf(),
            vname_path: "fixture.cpp".into(),
            corpus: "github.com/travsr/test".into(),
            package: String::new(),
            source: None,
        };
        let resp = sidecar().parse(&req);
        let kinds: std::collections::HashSet<&str> =
            resp.nodes.iter().map(|n| n.kind.as_str()).collect();
        for expected in ["file", "class", "namespace", "import"] {
            assert!(kinds.contains(expected), "missing {expected}: {kinds:?}");
        }
    }
}
