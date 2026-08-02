//! Integration tests for the file watcher.
//!
//! These tests spin up a real inotify/kqueue watcher via the `notify` crate
//! and verify that the correct `WatchEvent` variants are emitted for file
//! creations and that skip rules are applied correctly.

use std::time::Instant;
use tokio::sync::mpsc;
use travsr_daemon::watcher::{spawn, WatchEvent};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_detects_ts_file_creation() {
    let tmp = tempfile::tempdir().unwrap();
    let (tx, mut rx) = mpsc::channel(16);
    let _handle = spawn(tmp.path(), tx, Instant::now()).unwrap();

    // Give the watcher time to initialise and register the kqueue/inotify watch.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    std::fs::write(tmp.path().join("app.ts"), "export class App {}").unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if let Some(WatchEvent::Upsert(p)) = rx.recv().await {
                if p.ends_with("app.ts") {
                    return;
                }
            }
        }
    })
    .await
    .expect("watcher must emit Upsert for app.ts within 3 s");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_ignores_target_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let (tx, mut rx) = mpsc::channel(16);
    let _handle = spawn(tmp.path(), tx, Instant::now()).unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let target = tmp.path().join("target/debug");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("lib.rs"), "fn main() {}").unwrap();

    // No event should arrive for target/ contents within 1.5 s.
    let got_event = tokio::time::timeout(std::time::Duration::from_millis(1500), rx.recv()).await;
    assert!(
        got_event.is_err(),
        "watcher must ignore target/ directory, but got: {got_event:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_ignores_non_source_files() {
    let tmp = tempfile::tempdir().unwrap();
    let (tx, mut rx) = mpsc::channel(16);
    let _handle = spawn(tmp.path(), tx, Instant::now()).unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Write an ignored file first, then a source file.
    // NB: the ignored fixture is deliberately not `README.md` — #376 Phase 1
    // made markdown an indexed language, so a `.md` write is now a legitimate
    // Upsert. `notes.txt` has no `Language::from_extension` arm, which is the
    // property this test is actually about.
    std::fs::write(tmp.path().join("notes.txt"), "scratch").unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    std::fs::write(tmp.path().join("app.ts"), "export {}").unwrap();

    let ev = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            match rx.recv().await {
                Some(WatchEvent::Upsert(p)) if p.ends_with("app.ts") => return p,
                Some(WatchEvent::Upsert(p)) if p.ends_with("notes.txt") => {
                    panic!(
                        "watcher must not emit events for notes.txt, got: {}",
                        p.display()
                    );
                }
                _ => {} // other events (dir creation etc.) — keep draining
            }
        }
    })
    .await
    .expect("must receive Upsert for app.ts within 3 s");

    assert!(
        ev.ends_with("app.ts"),
        "expected app.ts, got: {}",
        ev.display()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_ignores_vscode_test_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let (tx, mut rx) = mpsc::channel(16);
    let _handle = spawn(tmp.path(), tx, Instant::now()).unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let vscode_test = tmp.path().join(".vscode-test");
    std::fs::create_dir_all(&vscode_test).unwrap();
    std::fs::write(vscode_test.join("runner.ts"), "export {}").unwrap();

    // No event should arrive for .vscode-test/ contents.
    let got_event = tokio::time::timeout(std::time::Duration::from_millis(1500), rx.recv()).await;
    assert!(
        got_event.is_err(),
        "watcher must ignore .vscode-test/, but got: {got_event:?}"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_ignores_unix_socket_files() {
    use std::os::unix::net::UnixListener;

    let tmp = tempfile::tempdir().unwrap();
    let (tx, mut rx) = mpsc::channel(16);
    let _handle = spawn(tmp.path(), tx, Instant::now()).unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Bind a Unix socket inside the watched directory — should be filtered out.
    let sock_path = tmp.path().join("test.sock");
    let _listener = UnixListener::bind(&sock_path).unwrap();

    // No event should arrive for the socket file.
    let got_event = tokio::time::timeout(std::time::Duration::from_millis(1500), rx.recv()).await;
    assert!(
        got_event.is_err(),
        "watcher must ignore socket files, but got: {got_event:?}"
    );
}
