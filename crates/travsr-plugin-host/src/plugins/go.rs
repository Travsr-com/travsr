use travsr_core::Language;
use travsr_plugin_protocol::{ParseRequest, ParseResponse, Plugin, InvokeRequest, InvokeResponse};
use super::parse_output_to_response;

pub struct GoPlugin;

impl Plugin for GoPlugin {
    fn language(&self) -> Language { Language::Go }
    fn extensions(&self) -> &[&str] { &["go"] }
    fn parse(&self, req: &ParseRequest) -> ParseResponse {
        match travsr_indexer::go_parse(&req.corpus, &req.path, &req.vname_path) {
            Ok(out) => parse_output_to_response(out),
            Err(e) => { tracing::warn!("go parse {}: {e}", req.path.display()); ParseResponse::default() }
        }
    }
    fn invoke_phase_b(&self, _req: &InvokeRequest) -> InvokeResponse {
        InvokeResponse::default() // Go Phase B (scip-go) deferred to P5-S5
    }
}
