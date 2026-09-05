use crate::protocol_compat::ensure_protocol_compatible;
use std::io::{self, BufReader, BufWriter, Read, Write};
use tracing::{error, info};
use travsr_plugin_protocol::{
    codec::{decode_message, write_message},
    HandshakeResponse, Plugin, PluginRequest, PluginResponse, PROTOCOL_VERSION,
};

/// Run the plugin event loop on stdin/stdout. Blocks until stdin closes.
pub fn run_plugin<P: Plugin>(plugin: P) {
    let stdin = io::stdin();
    let stdout = io::stdout();
    run_plugin_loop(
        &plugin,
        &mut BufReader::new(stdin.lock()),
        &mut BufWriter::new(stdout.lock()),
    );
}

fn run_plugin_loop<P: Plugin>(plugin: &P, reader: &mut impl Read, writer: &mut impl Write) {
    loop {
        let req: PluginRequest = match decode_message(reader) {
            Ok(r) => r,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => {
                error!("framing error: {e}");
                break;
            }
        };

        let resp = match req {
            PluginRequest::Handshake(h) => {
                if let Err(refusal) =
                    ensure_protocol_compatible(h.daemon_protocol_version, PROTOCOL_VERSION)
                {
                    error!("{}", refusal.message);
                    let _ = write_message(writer, &PluginResponse::Error(refusal));
                    break;
                }
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

        if let Err(e) = write_message(writer, &resp) {
            error!("write error: {e}");
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Language;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use travsr_plugin_protocol::{
        codec::encode_message, HandshakeRequest, InvokeRequest, InvokeResponse, ParseRequest,
        ParseResponse,
    };

    #[derive(Default)]
    struct CountingPlugin {
        parses: AtomicUsize,
    }

    impl Plugin for CountingPlugin {
        fn language(&self) -> Language {
            Language::Rust
        }

        fn extensions(&self) -> &[&str] {
            &["rs"]
        }

        fn parse(&self, _req: &ParseRequest) -> ParseResponse {
            self.parses.fetch_add(1, Ordering::SeqCst);
            ParseResponse::default()
        }

        fn invoke_phase_b(&self, _req: &InvokeRequest) -> InvokeResponse {
            InvokeResponse::unsupported()
        }
    }

    fn parse_request() -> PluginRequest {
        PluginRequest::Parse(ParseRequest {
            path: "src/lib.rs".into(),
            vname_path: "src/lib.rs".into(),
            corpus: "github.com/acme/foo".into(),
            package: "acme".into(),
            source: None,
        })
    }

    fn drive(plugin: &CountingPlugin, requests: &[PluginRequest]) -> Vec<PluginResponse> {
        let mut input = Vec::new();
        for req in requests {
            input.extend_from_slice(&encode_message(req).expect("encode request"));
        }
        let mut output = Vec::new();
        run_plugin_loop(plugin, &mut Cursor::new(input), &mut output);

        let mut cursor = Cursor::new(output);
        let mut responses = Vec::new();
        while (cursor.position() as usize) < cursor.get_ref().len() {
            responses.push(decode_message(&mut cursor).expect("decode response"));
        }
        responses
    }

    #[test]
    fn matching_protocol_version_serves_work_requests() {
        let plugin = CountingPlugin::default();
        let responses = drive(
            &plugin,
            &[
                PluginRequest::Handshake(HandshakeRequest {
                    daemon_protocol_version: PROTOCOL_VERSION,
                }),
                parse_request(),
            ],
        );

        assert_eq!(responses.len(), 2, "got {responses:?}");
        match &responses[0] {
            PluginResponse::Handshake(h) => {
                assert_eq!(h.protocol_version, PROTOCOL_VERSION);
                assert_eq!(h.language, "rust");
                assert_eq!(h.extensions, vec!["rs".to_string()]);
            }
            other => panic!("expected handshake response, got {other:?}"),
        }
        assert!(matches!(responses[1], PluginResponse::Parse(_)));
        assert_eq!(plugin.parses.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn mismatched_protocol_version_refuses_and_stops() {
        let plugin = CountingPlugin::default();
        let responses = drive(
            &plugin,
            &[
                PluginRequest::Handshake(HandshakeRequest {
                    daemon_protocol_version: PROTOCOL_VERSION + 98,
                }),
                parse_request(),
            ],
        );

        assert_eq!(responses.len(), 1, "got {responses:?}");
        match &responses[0] {
            PluginResponse::Error(e) => assert!(
                e.message.contains("incompatible daemon protocol version"),
                "unexpected message: {}",
                e.message
            ),
            other => panic!("expected error response, got {other:?}"),
        }
        assert_eq!(
            plugin.parses.load(Ordering::SeqCst),
            0,
            "no work request may be served after a refused handshake"
        );
    }

    #[test]
    fn closed_stdin_exits_without_writing() {
        let plugin = CountingPlugin::default();
        assert!(drive(&plugin, &[]).is_empty());
    }
}
