use crate::protocol_compat::ensure_protocol_compatible;
use std::io::{self, BufReader, BufWriter, Read, Write};
use tracing::{error, info};
use travsr_plugin_protocol::{
    codec::{decode_message, write_message},
    EmbedHandshakeResponse, EmbedPlugin, EmbedPluginRequest, EmbedPluginResponse,
    EMBED_PROTOCOL_VERSION,
};

/// Run the embed plugin event loop on stdin/stdout. Blocks until stdin closes.
///
/// Entry point for every `travsr-embed-<backend>` binary. The host (daemon)
/// sends `EmbedPluginRequest` frames; this function dispatches to the plugin
/// and writes `EmbedPluginResponse` frames back.
///
/// The language-plugin `run_plugin()` loop is a completely separate function —
/// the two never interact; each drives its own binary.
pub fn run_embed_plugin<P: EmbedPlugin>(plugin: P) {
    let stdin = io::stdin();
    let stdout = io::stdout();
    run_embed_plugin_loop(
        &plugin,
        &mut BufReader::new(stdin.lock()),
        &mut BufWriter::new(stdout.lock()),
    );
}

fn run_embed_plugin_loop<P: EmbedPlugin>(
    plugin: &P,
    reader: &mut impl Read,
    writer: &mut impl Write,
) {
    loop {
        let req: EmbedPluginRequest = match decode_message(reader) {
            Ok(r) => r,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => {
                error!("embed plugin framing error: {e}");
                break;
            }
        };

        let resp = match req {
            EmbedPluginRequest::Handshake(h) => {
                if let Err(refusal) =
                    ensure_protocol_compatible(h.daemon_protocol_version, EMBED_PROTOCOL_VERSION)
                {
                    error!("{}", refusal.message);
                    let _ = write_message(writer, &EmbedPluginResponse::Error(refusal));
                    break;
                }
                info!(
                    "embed handshake: daemon_version={}, model={}",
                    h.daemon_protocol_version,
                    plugin.model_id()
                );
                EmbedPluginResponse::Handshake(EmbedHandshakeResponse {
                    protocol_version: EMBED_PROTOCOL_VERSION,
                    plugin_version: plugin.plugin_version().to_string(),
                    model_id: plugin.model_id().to_string(),
                    embedding_dim: plugin.embedding_dim(),
                    backend: plugin.backend().to_string(),
                    max_batch: plugin.max_batch(),
                })
            }
            EmbedPluginRequest::Embed(req) => EmbedPluginResponse::Embed(plugin.embed_batch(&req)),
            EmbedPluginRequest::Knn(req) => EmbedPluginResponse::Knn(plugin.knn(&req)),
        };

        if let Err(e) = write_message(writer, &resp) {
            error!("embed plugin write error: {e}");
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use travsr_plugin_protocol::{
        codec::encode_message, EmbedHandshakeRequest, EmbedRequest, EmbedResponse, KnnRequest,
        KnnResponse,
    };

    #[derive(Default)]
    struct CountingEmbedPlugin {
        embeds: AtomicUsize,
    }

    impl EmbedPlugin for CountingEmbedPlugin {
        fn model_id(&self) -> &str {
            "test-model"
        }

        fn embedding_dim(&self) -> u32 {
            4
        }

        fn backend(&self) -> &str {
            "test-backend"
        }

        fn plugin_version(&self) -> &str {
            "0.0.0-test"
        }

        fn embed_batch(&self, req: &EmbedRequest) -> EmbedResponse {
            self.embeds.fetch_add(1, Ordering::SeqCst);
            EmbedResponse {
                embeddings: req.texts.iter().map(|_| vec![0u8; 4]).collect(),
            }
        }

        fn knn(&self, _req: &KnnRequest) -> KnnResponse {
            KnnResponse {
                node_ids: Vec::new(),
                scores: Vec::new(),
            }
        }
    }

    fn embed_request() -> EmbedPluginRequest {
        EmbedPluginRequest::Embed(EmbedRequest {
            texts: vec!["fn main() {}".to_string()],
        })
    }

    fn drive(
        plugin: &CountingEmbedPlugin,
        requests: &[EmbedPluginRequest],
    ) -> Vec<EmbedPluginResponse> {
        let mut input = Vec::new();
        for req in requests {
            input.extend_from_slice(&encode_message(req).expect("encode request"));
        }
        let mut output = Vec::new();
        run_embed_plugin_loop(plugin, &mut Cursor::new(input), &mut output);

        let mut cursor = Cursor::new(output);
        let mut responses = Vec::new();
        while (cursor.position() as usize) < cursor.get_ref().len() {
            responses.push(decode_message(&mut cursor).expect("decode response"));
        }
        responses
    }

    #[test]
    fn matching_protocol_version_serves_work_requests() {
        let plugin = CountingEmbedPlugin::default();
        let responses = drive(
            &plugin,
            &[
                EmbedPluginRequest::Handshake(EmbedHandshakeRequest {
                    daemon_protocol_version: EMBED_PROTOCOL_VERSION,
                }),
                embed_request(),
            ],
        );

        assert_eq!(responses.len(), 2, "got {responses:?}");
        match &responses[0] {
            EmbedPluginResponse::Handshake(h) => {
                assert_eq!(h.protocol_version, EMBED_PROTOCOL_VERSION);
                assert_eq!(h.model_id, "test-model");
                assert_eq!(h.plugin_version, "0.0.0-test");
            }
            other => panic!("expected handshake response, got {other:?}"),
        }
        assert!(matches!(responses[1], EmbedPluginResponse::Embed(_)));
        assert_eq!(plugin.embeds.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn mismatched_protocol_version_refuses_and_stops() {
        let plugin = CountingEmbedPlugin::default();
        let responses = drive(
            &plugin,
            &[
                EmbedPluginRequest::Handshake(EmbedHandshakeRequest {
                    daemon_protocol_version: EMBED_PROTOCOL_VERSION + 98,
                }),
                embed_request(),
            ],
        );

        assert_eq!(responses.len(), 1, "got {responses:?}");
        match &responses[0] {
            EmbedPluginResponse::Error(e) => assert!(
                e.message.contains("incompatible daemon protocol version"),
                "unexpected message: {}",
                e.message
            ),
            other => panic!("expected error response, got {other:?}"),
        }
        assert_eq!(
            plugin.embeds.load(Ordering::SeqCst),
            0,
            "no work request may be served after a refused handshake"
        );
    }

    #[test]
    fn closed_stdin_exits_without_writing() {
        let plugin = CountingEmbedPlugin::default();
        assert!(drive(&plugin, &[]).is_empty());
    }
}
