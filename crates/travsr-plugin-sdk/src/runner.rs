use std::io::{self, BufReader, BufWriter};
use travsr_plugin_protocol::{
    codec::{decode_message, write_message},
    HandshakeResponse, Plugin, PluginRequest, PluginResponse, PROTOCOL_VERSION,
};
use tracing::{error, info};

/// Run the plugin event loop on stdin/stdout. Blocks until stdin closes.
pub fn run_plugin<P: Plugin>(plugin: P) {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = BufWriter::new(stdout.lock());

    loop {
        let req: PluginRequest = match decode_message(&mut reader) {
            Ok(r) => r,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => { error!("framing error: {e}"); break; }
        };

        let resp = match req {
            PluginRequest::Handshake(h) => {
                info!("handshake: daemon_version={}", h.daemon_protocol_version);
                PluginResponse::Handshake(HandshakeResponse {
                    protocol_version: PROTOCOL_VERSION,
                    plugin_version: env!("CARGO_PKG_VERSION").to_string(),
                    language: plugin.language().as_str().to_string(),
                    extensions: plugin.extensions().iter().map(|s| s.to_string()).collect(),
                    supports_phase_b: plugin.supports_phase_b(),
                })
            }
            PluginRequest::Parse(req) => PluginResponse::Parse(plugin.parse(&req)),
            PluginRequest::Invoke(req) => PluginResponse::Invoke(plugin.invoke_phase_b(&req)),
        };

        if let Err(e) = write_message(&mut writer, &resp) {
            error!("write error: {e}"); break;
        }
    }
}

/// Entry point for the `__plugin <lang>` hidden subcommand.
/// P5-S1: stubs only. Real plugin impls wire up in P5-S2.
pub fn run_builtin(lang: &str) {
    match lang {
        "typescript" | "javascript" => panic!("TypeScript plugin: wire impl in P5-S2"),
        "rust"   => panic!("Rust plugin: wire impl in P5-S2"),
        "python" => panic!("Python plugin: wire impl in P5-S2"),
        "go"     => panic!("Go plugin: wire impl in P5-S2"),
        other    => panic!("unknown built-in plugin language: {other:?}"),
    }
}
