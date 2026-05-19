use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use assert_cmd::cargo::cargo_bin;

// ── helpers ──────────────────────────────────────────────────────────────────

fn git_init(dir: &Path) {
    Command::new("git")
        .args(["-c", "init.defaultBranch=main", "init", "-q"])
        .current_dir(dir)
        .status()
        .expect("git init");
}

fn init_test_repo(tmp: &Path) {
    git_init(tmp);
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/ts-callers");
    for entry in std::fs::read_dir(fixtures).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), tmp.join(entry.file_name())).unwrap();
    }
    Command::new(cargo_bin("travsr"))
        .arg("init")
        .current_dir(tmp)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("travsr init");
}

/// Send `messages` to a fresh `travsr mcp --stdio` process, close stdin,
/// collect and parse every non-empty stdout line as JSON.
fn run_mcp(cwd: &Path, messages: &[&str]) -> Vec<serde_json::Value> {
    let mut child = Command::new(cargo_bin("travsr"))
        .args(["mcp", "--stdio"])
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null()) // keep test output clean; logs go to stderr anyway
        .spawn()
        .expect("spawn travsr mcp");

    let mut stdin = child.stdin.take().unwrap();
    for msg in messages {
        writeln!(stdin, "{msg}").unwrap();
    }
    drop(stdin); // EOF → server exits cleanly

    let output = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|_| panic!("invalid JSON: {l}")))
        .collect()
}

const INIT_MSG: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;

// ── tests ─────────────────────────────────────────────────────────────────────

#[test]
fn mcp_initialize_returns_correct_protocol_version() {
    let tmp = tempfile::tempdir().unwrap();
    init_test_repo(tmp.path());

    let responses = run_mcp(tmp.path(), &[INIT_MSG]);
    assert_eq!(responses.len(), 1);
    let result = &responses[0]["result"];
    assert_eq!(result["protocolVersion"], "2024-11-05");
    assert_eq!(result["serverInfo"]["name"], "travsr");
}

#[test]
fn mcp_tools_list_returns_two_tools() {
    let tmp = tempfile::tempdir().unwrap();
    init_test_repo(tmp.path());

    let responses = run_mcp(
        tmp.path(),
        &[
            INIT_MSG,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
        ],
    );
    assert_eq!(responses.len(), 2);
    let tools = &responses[1]["result"]["tools"];
    let names: Vec<&str> = tools
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(
        names.contains(&"get_dependencies"),
        "must list get_dependencies"
    );
    assert!(names.contains(&"get_callers"), "must list get_callers");
}

#[test]
fn mcp_notifications_initialized_gets_no_response() {
    let tmp = tempfile::tempdir().unwrap();
    init_test_repo(tmp.path());

    // A notification has no "id" — must produce zero response lines.
    // Followed by a real request so we can verify the server is still alive.
    let responses = run_mcp(
        tmp.path(),
        &[
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            INIT_MSG,
        ],
    );
    assert_eq!(
        responses.len(),
        1,
        "notification must not produce a response; only the initialize request should"
    );
    assert_eq!(responses[0]["id"], 1);
}

#[test]
fn mcp_get_dependencies_returns_text_content() {
    let tmp = tempfile::tempdir().unwrap();
    init_test_repo(tmp.path());

    // controller.ts imports from "./service"
    let responses = run_mcp(
        tmp.path(),
        &[
            INIT_MSG,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"get_dependencies","arguments":{"file":"controller.ts"}}}"#,
        ],
    );
    assert_eq!(responses.len(), 2);
    let content = &responses[1]["result"]["content"][0];
    assert_eq!(content["type"], "text", "content must be text");
}

#[test]
fn mcp_get_callers_returns_text_content() {
    let tmp = tempfile::tempdir().unwrap();
    init_test_repo(tmp.path());

    // "charge" is the method name on PaymentService — its class node has an incoming edge
    let responses = run_mcp(
        tmp.path(),
        &[
            INIT_MSG,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"get_callers","arguments":{"symbol":"charge"}}}"#,
        ],
    );
    assert_eq!(responses.len(), 2);
    let content = &responses[1]["result"]["content"][0];
    assert_eq!(content["type"], "text");
    let text = content["text"].as_str().unwrap_or("");
    assert!(
        !text.is_empty(),
        "PaymentService has an edge to charge — result must be non-empty"
    );
}

#[test]
fn mcp_unknown_method_returns_error_code() {
    let tmp = tempfile::tempdir().unwrap();
    init_test_repo(tmp.path());

    let responses = run_mcp(
        tmp.path(),
        &[r#"{"jsonrpc":"2.0","id":99,"method":"nonexistent/method","params":{}}"#],
    );
    assert_eq!(responses.len(), 1);
    assert_eq!(
        responses[0]["error"]["code"], -32601,
        "unknown method must return JSON-RPC -32601"
    );
}

#[test]
fn mcp_empty_result_is_not_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    init_test_repo(tmp.path());

    // "zzz_nonexistent" will match nothing — must return result, not error
    let responses = run_mcp(
        tmp.path(),
        &[
            INIT_MSG,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"get_callers","arguments":{"symbol":"zzz_nonexistent"}}}"#,
        ],
    );
    assert_eq!(responses.len(), 2);
    let resp = &responses[1];
    assert!(
        resp.get("result").is_some(),
        "empty lookup must use 'result', not 'error'"
    );
    assert!(
        resp.get("error").is_none(),
        "must not return error for empty results"
    );
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("non-empty");
    // SEC-001: empty results are now wrapped in the sanitized envelope.
    // The envelope is the minimum non-empty response; the LLM sees it as
    // "no data found" rather than an error.
    assert_eq!(
        text, "<travsr-data></travsr-data>",
        "content text must be the empty envelope for no-match query"
    );
}
