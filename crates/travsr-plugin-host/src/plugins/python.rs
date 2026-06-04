use super::parse_output_to_response;
use travsr_core::Language;
use travsr_plugin_protocol::{InvokeRequest, InvokeResponse, ParseRequest, ParseResponse, Plugin};

pub struct PythonPlugin;

impl Plugin for PythonPlugin {
    fn language(&self) -> Language {
        Language::Python
    }
    fn extensions(&self) -> &[&str] {
        &["py", "pyi"]
    }
    fn supports_phase_b(&self) -> bool {
        true
    }
    fn parse(&self, req: &ParseRequest) -> ParseResponse {
        match travsr_indexer::python_parse(&req.corpus, &req.path, &req.vname_path) {
            Ok(mut out) => {
                // Best-effort pyright enrichment (same as old Indexer path)
                let pyright = travsr_indexer::python_lsif::parse_python_with_pyright(
                    &req.path,
                    std::time::Duration::from_secs(30),
                )
                .unwrap_or_default();
                out.merge_deduped(pyright);
                parse_output_to_response(out)
            }
            Err(e) => {
                tracing::warn!("python parse {}: {e}", req.path.display());
                ParseResponse::default()
            }
        }
    }
    fn invoke_phase_b(&self, req: &InvokeRequest) -> InvokeResponse {
        match travsr_indexer::run_scip_python(&req.root, &req.corpus) {
            Ok(Some(bytes)) => match travsr_indexer::ingest_scip(&bytes, &req.corpus) {
                Ok(out) => InvokeResponse {
                    nodes: out.nodes,
                    edges: out.edges,
                },
                Err(e) => {
                    tracing::warn!("python scip ingest: {e}");
                    InvokeResponse::default()
                }
            },
            Ok(None) => {
                tracing::info!(
                    "scip-python not found — Python Phase B skipped \
                     (install: npm install -g @sourcegraph/scip-python)"
                );
                InvokeResponse::default()
            }
            Err(e) => {
                tracing::warn!("scip-python failed: {e}");
                InvokeResponse::default()
            }
        }
    }
}
