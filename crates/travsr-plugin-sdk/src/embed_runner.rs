use std::io::{self, BufReader, BufWriter};
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
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = BufWriter::new(stdout.lock());

    loop {
        let req: EmbedPluginRequest = match decode_message(&mut reader) {
            Ok(r) => r,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => {
                error!("embed plugin framing error: {e}");
                break;
            }
        };

        let resp = match req {
            EmbedPluginRequest::Handshake(h) => {
                info!(
                    "embed handshake: daemon_version={}, model={}",
                    h.daemon_protocol_version,
                    plugin.model_id()
                );
                EmbedPluginResponse::Handshake(EmbedHandshakeResponse {
                    protocol_version: EMBED_PROTOCOL_VERSION,
                    plugin_version: env!("CARGO_PKG_VERSION").to_string(),
                    model_id: plugin.model_id().to_string(),
                    embedding_dim: plugin.embedding_dim(),
                    backend: plugin.backend().to_string(),
                    max_batch: plugin.max_batch(),
                })
            }
            EmbedPluginRequest::Embed(req) => EmbedPluginResponse::Embed(plugin.embed_batch(&req)),
            EmbedPluginRequest::Knn(req) => EmbedPluginResponse::Knn(plugin.knn(&req)),
        };

        if let Err(e) = write_message(&mut writer, &resp) {
            error!("embed plugin write error: {e}");
            break;
        }
    }
}
