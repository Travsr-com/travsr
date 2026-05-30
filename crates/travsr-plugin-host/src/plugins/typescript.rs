use super::parse_output_to_response;
use travsr_core::Language;
use travsr_plugin_protocol::{InvokeRequest, InvokeResponse, ParseRequest, ParseResponse, Plugin};

pub struct TypeScriptPlugin;

impl Plugin for TypeScriptPlugin {
    fn language(&self) -> Language {
        Language::TypeScript
    }
    fn extensions(&self) -> &[&str] {
        &["ts", "tsx", "mts", "cts"]
    }
    fn supports_phase_b(&self) -> bool {
        true
    } // travsr-lsif-ts
    fn parse(&self, req: &ParseRequest) -> ParseResponse {
        if let Some(src_bytes) = &req.source {
            // git-blob path: write to tempfile then parse
            use std::io::Write;
            let ext = req
                .path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("ts");
            match tempfile::Builder::new()
                .suffix(&format!(".{ext}"))
                .tempfile()
            {
                Ok(mut tmp) => {
                    let _ = tmp.write_all(src_bytes);
                    let _ = tmp.flush();
                    match travsr_indexer::typescript_parse(&req.corpus, tmp.path(), &req.vname_path)
                    {
                        Ok(out) => return parse_output_to_response(out),
                        Err(e) => {
                            tracing::warn!("ts parse (blob): {e}");
                            return ParseResponse::default();
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("tempfile: {e}");
                    return ParseResponse::default();
                }
            }
        }
        match travsr_indexer::typescript_parse(&req.corpus, &req.path, &req.vname_path) {
            Ok(out) => parse_output_to_response(out),
            Err(e) => {
                tracing::warn!("ts parse {}: {e}", req.path.display());
                ParseResponse::default()
            }
        }
    }
    fn invoke_phase_b(&self, req: &InvokeRequest) -> InvokeResponse {
        // travsr-lsif-ts must be on PATH (npm install -g travsr-lsif-ts)
        let tsconfig = req.root.join("tsconfig.json");
        if !tsconfig.exists() {
            tracing::info!("ts phase_b: no tsconfig.json in {}", req.root.display());
            return InvokeResponse::default();
        }
        match travsr_indexer::run_lsif_emitter(&tsconfig) {
            Ok(dump) => match travsr_indexer::ingest_lsif(&dump) {
                Ok(lsif_out) => InvokeResponse {
                    nodes: lsif_out.nodes,
                    edges: lsif_out.edges,
                },
                Err(e) => {
                    tracing::warn!("ts lsif ingest: {e}");
                    InvokeResponse::default()
                }
            },
            Err(e) => {
                tracing::warn!("ts lsif emitter: {e}");
                InvokeResponse::default()
            }
        }
    }
}
