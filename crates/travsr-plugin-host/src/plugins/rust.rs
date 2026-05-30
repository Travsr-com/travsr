use super::parse_output_to_response;
use travsr_core::Language;
use travsr_plugin_protocol::{InvokeRequest, InvokeResponse, ParseRequest, ParseResponse, Plugin};

pub struct RustPlugin;

impl Plugin for RustPlugin {
    fn language(&self) -> Language {
        Language::Rust
    }
    fn extensions(&self) -> &[&str] {
        &["rs"]
    }
    fn supports_phase_b(&self) -> bool {
        travsr_indexer::ra_runner::ra_available()
    }
    fn parse(&self, req: &ParseRequest) -> ParseResponse {
        match travsr_indexer::rust_parse(&req.corpus, &req.path, &req.vname_path) {
            Ok(out) => parse_output_to_response(out),
            Err(e) => {
                tracing::warn!("rust parse {}: {e}", req.path.display());
                ParseResponse::default()
            }
        }
    }
    fn invoke_phase_b(&self, req: &InvokeRequest) -> InvokeResponse {
        use travsr_indexer::{ingest_rust, run_ra_lsif, sandbox::SandboxConfig};
        let cfg = SandboxConfig {
            repo_root: req.root.clone(),
            ..Default::default()
        };
        match run_ra_lsif(&req.root, &cfg) {
            Ok(Some(dump)) => match ingest_rust(&dump, "") {
                Ok(out) => InvokeResponse {
                    nodes: out.nodes,
                    edges: out.edges,
                },
                Err(e) => {
                    tracing::warn!("rust lsif ingest: {e}");
                    InvokeResponse::default()
                }
            },
            Ok(None) => {
                tracing::info!("rust-analyzer not available");
                InvokeResponse::default()
            }
            Err(e) => {
                tracing::warn!("rust-analyzer failed: {e}");
                InvokeResponse::default()
            }
        }
    }
}
