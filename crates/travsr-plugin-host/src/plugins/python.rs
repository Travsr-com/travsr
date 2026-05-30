use travsr_core::Language;
use travsr_plugin_protocol::{ParseRequest, ParseResponse, Plugin, InvokeRequest, InvokeResponse};
use super::parse_output_to_response;

pub struct PythonPlugin;

impl Plugin for PythonPlugin {
    fn language(&self) -> Language { Language::Python }
    fn extensions(&self) -> &[&str] { &["py", "pyi"] }
    fn parse(&self, req: &ParseRequest) -> ParseResponse {
        match travsr_indexer::python_parse(&req.corpus, &req.path, &req.vname_path) {
            Ok(mut out) => {
                // Best-effort pyright enrichment (same as old Indexer path)
                let pyright = travsr_indexer::python_lsif::parse_python_with_pyright(
                    &req.path,
                    std::time::Duration::from_secs(30),
                ).unwrap_or_default();
                out.merge_deduped(pyright);
                parse_output_to_response(out)
            }
            Err(e) => { tracing::warn!("python parse {}: {e}", req.path.display()); ParseResponse::default() }
        }
    }
    fn invoke_phase_b(&self, _req: &InvokeRequest) -> InvokeResponse {
        InvokeResponse::default() // Python Phase B deferred
    }
}
