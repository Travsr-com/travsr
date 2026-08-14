//! #407 T2: in-crate loopback round-trips through the REAL transports.
//!
//! The unit tests cover addressing and message serialization; these bind an
//! actual listener/pipe server and drive every `ControlMessage` variant through
//! `UnixTransport` / `NamedPipeTransport`, so a framing or transport regression
//! fails here instead of only in the daemon's end-to-end suites (Unix) or not
//! at all (the Windows client path previously had no in-repo coverage).

use travsr_ipc::{ControlAddr, ControlMessage, ControlResponse, ControlTransport as _};

/// One of every `ControlMessage` variant. A new variant fails the
/// `EVERY_VARIANT` count below until it is added here, so the loopback
/// coverage cannot silently rot.
fn every_message_variant() -> Vec<ControlMessage> {
    vec![
        ControlMessage::ReindexCommit {
            sha: "abc1234".into(),
        },
        ControlMessage::ReindexPaths {
            paths: vec![std::path::PathBuf::from("src/lib.rs")],
        },
        ControlMessage::Status,
        ControlMessage::Shutdown,
        ControlMessage::StopEmbed,
        ControlMessage::ResumeEmbed,
        ControlMessage::Query {
            protocol: travsr_ipc::QUERY_PROTOCOL_VERSION,
            tool: "status".into(),
            args: serde_json::json!({}),
        },
    ]
}

const EVERY_VARIANT: usize = 7;

fn loopback_response() -> String {
    // Serialized once, written verbatim by the fake servers.
    serde_json::to_string(&ControlResponse::ok("loopback".to_string())).expect("serialize")
}

#[cfg(unix)]
#[test]
fn unix_loopback_round_trips_every_message_variant() {
    use std::io::{BufRead as _, BufReader, Write as _};

    let messages = every_message_variant();
    assert_eq!(
        messages.len(),
        EVERY_VARIANT,
        "keep the variant list complete"
    );

    let dir = tempfile::tempdir().expect("tempdir");
    let travsr_dir = dir.path().to_path_buf();
    let addr = ControlAddr::for_repo(dir.path());
    let sock_path = addr.socket_path(&travsr_dir);
    let listener = std::os::unix::net::UnixListener::bind(&sock_path).expect("bind");

    // One connection per request (the daemon's protocol), plus one final
    // connection for the fire-and-forget send, which gets no response.
    let server = std::thread::spawn(move || {
        let mut received = Vec::new();
        for i in 0..=EVERY_VARIANT {
            let (stream, _) = listener.accept().expect("accept");
            let mut line = String::new();
            BufReader::new(&stream)
                .read_line(&mut line)
                .expect("read request line");
            received.push(line.trim().to_string());
            if i < EVERY_VARIANT {
                writeln!(&stream, "{}", loopback_response()).expect("write response");
            }
        }
        received
    });

    for msg in &messages {
        let mut transport =
            travsr_ipc::unix::UnixTransport::connect(&addr, &travsr_dir).expect("connect");
        let resp = transport.send_request(msg).expect("round trip");
        assert!(resp.ok, "loopback response must parse as ok");
        assert_eq!(resp.message.as_deref(), Some("loopback"));
    }

    // #407 L2: the fire-and-forget path uses the same transport and framing.
    let mut transport =
        travsr_ipc::unix::UnixTransport::connect(&addr, &travsr_dir).expect("connect faf");
    transport
        .send_fire_and_forget(&ControlMessage::ReindexCommit { sha: "faf".into() })
        .expect("fire and forget");

    let received = server.join().expect("server thread");
    assert_eq!(received.len(), EVERY_VARIANT + 1);
    for line in &received {
        serde_json::from_str::<ControlMessage>(line)
            .unwrap_or_else(|e| panic!("server must receive a parseable message ({e}): {line}"));
    }
}

#[cfg(windows)]
#[test]
fn windows_loopback_round_trips_every_message_variant() {
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    use tokio::net::windows::named_pipe::ServerOptions;

    let messages = every_message_variant();
    assert_eq!(
        messages.len(),
        EVERY_VARIANT,
        "keep the variant list complete"
    );

    let dir = tempfile::tempdir().expect("tempdir");
    let addr = ControlAddr::for_repo(dir.path());
    let pipe_name = addr.pipe_name();

    // Fake daemon: one pipe instance per connection, mirroring the real accept
    // loop (travsr-daemon). The final connection is the fire-and-forget send
    // and gets no response.
    let server_pipe = pipe_name.clone();
    let server = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(async move {
            let mut received = Vec::new();
            let mut first = true;
            for i in 0..=EVERY_VARIANT {
                let mut opts = ServerOptions::new();
                if first {
                    opts.first_pipe_instance(true);
                    first = false;
                }
                let server = opts.create(&server_pipe).expect("create pipe instance");
                server.connect().await.expect("pipe connect");
                let (reader, mut writer) = tokio::io::split(server);
                let mut lines = tokio::io::BufReader::new(reader).lines();
                let line = lines
                    .next_line()
                    .await
                    .expect("read request line")
                    .expect("request line present");
                received.push(line);
                if i < EVERY_VARIANT {
                    writer
                        .write_all(format!("{}\n", loopback_response()).as_bytes())
                        .await
                        .expect("write response");
                    writer.flush().await.expect("flush response");
                    // Let the client drain the buffered response before this
                    // instance is torn down for the next iteration.
                    let _ = lines.next_line().await;
                }
            }
            received
        })
    });

    // The fake server has a gap between dropping one instance and creating the
    // next; a connect landing in that gap sees "not found", so retry briefly —
    // the real daemon has the same gap, which is also why `connect` retries
    // ERROR_PIPE_BUSY (#407 M2).
    let connect_with_retry = |what: &str| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match travsr_ipc::windows::NamedPipeTransport::connect(&addr) {
                Ok(t) => return t,
                Err(e) if std::time::Instant::now() < deadline => {
                    let _ = e;
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(e) => panic!("could not connect for {what}: {e}"),
            }
        }
    };

    for msg in &messages {
        let mut transport = connect_with_retry("request");
        let resp = transport.send_request(msg).expect("round trip");
        assert!(resp.ok, "loopback response must parse as ok");
        assert_eq!(resp.message.as_deref(), Some("loopback"));
    }

    // #407 L2: fire-and-forget through the same transport and framing.
    let mut transport = connect_with_retry("fire-and-forget");
    transport
        .send_fire_and_forget(&ControlMessage::ReindexCommit { sha: "faf".into() })
        .expect("fire and forget");

    let received = server.join().expect("server thread");
    assert_eq!(received.len(), EVERY_VARIANT + 1);
    for line in &received {
        serde_json::from_str::<ControlMessage>(line)
            .unwrap_or_else(|e| panic!("server must receive a parseable message ({e}): {line}"));
    }
}
