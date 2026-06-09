use super::parse_output_to_response;
use travsr_core::Language;
use travsr_plugin_protocol::{InvokeRequest, InvokeResponse, ParseRequest, ParseResponse, Plugin};

pub struct TypeScriptPlugin;

impl Plugin for TypeScriptPlugin {
    fn language(&self) -> Language {
        Language::TypeScript
    }
    fn extensions(&self) -> &[&str] {
        &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"]
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
        // Native Phase B: always runs, zero external-tool requirements.
        let (mut nodes, mut edges) =
            travsr_indexer::phase_b_native_typescript(&req.corpus, &req.root).unwrap_or_else(|e| {
                tracing::warn!("ts native phase_b: {e}");
                (vec![], vec![])
            });
        tracing::debug!(
            nodes = nodes.len(),
            edges = edges.len(),
            "ts native phase_b complete"
        );

        // LSIF enrichment: merge higher-fidelity edges when travsr-lsif-ts is available.
        let tsconfig = req.root.join("tsconfig.json");
        if tsconfig.exists() {
            match travsr_indexer::run_lsif_emitter(&tsconfig) {
                Ok(dump) => match travsr_indexer::ingest_lsif(&dump, &req.corpus) {
                    Ok(lsif_out) => {
                        tracing::debug!(
                            nodes = lsif_out.nodes.len(),
                            edges = lsif_out.edges.len(),
                            "ts lsif enrichment merged"
                        );
                        nodes.extend(lsif_out.nodes);
                        edges.extend(lsif_out.edges);
                    }
                    Err(e) => tracing::warn!("ts lsif ingest: {e}"),
                },
                Err(e) => tracing::debug!("ts lsif emitter not available: {e}"),
            }
        }

        // Dedup merged output
        nodes.sort_unstable_by_key(|n| n.id);
        nodes.dedup_by_key(|n| n.id);
        edges.sort_unstable_by_key(|e| (e.src, e.dst));
        edges.dedup_by(|a, b| a.src == b.src && a.dst == b.dst && a.kind == b.kind);

        InvokeResponse { nodes, edges }
    }
}
