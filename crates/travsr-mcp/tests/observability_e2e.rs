//! #636 end-to-end tests for the observability tools over the real MCP stdio
//! transport (both single-repo `travsr mcp --stdio` and multi-repo
//! `travsr mcp --global`).
//!
//! `crates/travsr-mcp/tests/conformance.rs` already covers the happy path for
//! all three tools in stdio mode. This file covers what that file does not:
//! global mode, path traversal / cross-repo-leak (AC5), secret and home-path
//! redaction reaching a real response (X1), the byte cap acting independently
//! of the tail cap (X2), no-mutation through the `open_read_only` path (AC4),
//! staleness against a real advancing git repo (AC2), and the parser/cap edge
//! cases the plan calls out (empty log dir, symlinked log file, zero-line
//! file, tail clamping, unknown level).
//!
//! Global-mode tests always run with `HOME` redirected to a throwaway temp
//! directory, never the real developer machine's `~/.travsr/registry.json`.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use assert_cmd::cargo::cargo_bin;

// ── helpers ──────────────────────────────────────────────────────────────────

fn git_init(dir: &Path) {
    Command::new("git")
        .args(["-c", "init.defaultBranch=main", "init", "-q"])
        .current_dir(dir)
        .status()
        .expect("git init");
}

fn git(args: &[&str], cwd: &Path) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .expect("git command");
    assert!(status.success(), "git {args:?} failed in {cwd:?}");
}

/// Copy the `ts-callers` fixture (controller.ts / service.ts, PaymentService)
/// into `tmp` and run `travsr init` with the given extra env vars.
fn init_test_repo_env(tmp: &Path, env: &[(&str, &str)]) {
    git_init(tmp);
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/ts-callers");
    for entry in std::fs::read_dir(fixtures).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), tmp.join(entry.file_name())).unwrap();
    }
    let mut cmd = Command::new(cargo_bin("travsr"));
    cmd.arg("init")
        .current_dir(tmp)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let status = cmd.status().expect("travsr init");
    assert!(status.success(), "travsr init failed in {tmp:?}");
}

/// Stdio-mode init: never registers into the real developer's registry.
fn init_test_repo(tmp: &Path) {
    init_test_repo_env(tmp, &[("TRAVSR_DISABLE_REGISTRY", "1")]);
}

/// Send `messages` to a fresh `travsr mcp` process (with `extra_args` and
/// `env`), close stdin, collect and parse every non-empty stdout line as JSON.
fn run_mcp_with(
    cwd: &Path,
    extra_args: &[&str],
    env: &[(&str, &str)],
    messages: &[&str],
) -> Vec<serde_json::Value> {
    let mut cmd = Command::new(cargo_bin("travsr"));
    cmd.arg("mcp")
        .args(extra_args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("spawn travsr mcp");

    let mut stdin = child.stdin.take().unwrap();
    for msg in messages {
        writeln!(stdin, "{msg}").unwrap();
    }
    drop(stdin); // EOF → server exits cleanly

    // `wait_with_output` drains stdout/stderr on background threads while
    // waiting, so it cannot deadlock on a full pipe buffer even for the
    // larger (X2 byte-cap) responses in this file.
    let output = child.wait_with_output().expect("travsr mcp did not exit");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|_| panic!("invalid JSON: {l}")))
        .collect()
}

fn run_mcp_stdio(cwd: &Path, messages: &[&str]) -> Vec<serde_json::Value> {
    run_mcp_with(cwd, &["--stdio"], &[], messages)
}

fn run_mcp_global(fake_home: &Path, messages: &[&str]) -> Vec<serde_json::Value> {
    // cwd is irrelevant in global mode (repos come from the registry), but a
    // real directory is required to spawn the process from.
    run_mcp_with(
        fake_home,
        &["--global"],
        &[("HOME", &fake_home.to_string_lossy())],
        messages,
    )
}

const INIT_MSG: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;

fn tools_list_msg() -> &'static str {
    r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#
}

fn call_msg(name: &str, arguments: serde_json::Value) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments }
    })
    .to_string()
}

/// Extract `content[0].text` from a `tools/call` response, asserting it is
/// present and not an RPC error.
fn tool_text(resp: &serde_json::Value) -> String {
    assert!(
        resp.get("error").is_none(),
        "tool call must not be an RPC error, got: {resp}"
    );
    resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("content.text must be a string, got: {resp}"))
        .to_string()
}

/// Parse the JSON payload out of a `<travsr-data>...</travsr-data>` envelope.
fn envelope_json(text: &str) -> serde_json::Value {
    let inner = text
        .strip_prefix("<travsr-data>\n")
        .and_then(|s| s.strip_suffix("\n</travsr-data>"))
        .unwrap_or_else(|| panic!("not a well-formed envelope: {text}"));
    serde_json::from_str(inner).unwrap_or_else(|e| panic!("payload not valid JSON: {e}: {inner}"))
}

fn write_log_line(travsr_dir: &Path, filename: &str, content: &str) {
    std::fs::write(travsr_dir.join(filename), content).unwrap();
}

/// Read the fake-home registry.json written by `travsr init` and return the
/// registry key whose db_path is under `repo_dir`. Avoids guessing whether
/// the registry key is canonicalized.
fn registry_key_for(fake_home: &Path, repo_dir: &Path) -> String {
    let raw = std::fs::read_to_string(fake_home.join(".travsr").join("registry.json"))
        .expect("registry.json must exist after travsr init");
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let repos = json["repos"].as_object().expect("repos object");
    // Registration canonicalizes the repo root before using it as the
    // registry key (travsr-daemon/src/lib.rs), which on Windows resolves an
    // 8.3 short-name alias (e.g. RUNNER~1) to its long form and prepends the
    // `\\?\` extended-length prefix. Canonicalize here too so the comparison
    // matches what was actually stored, not the tempdir's raw short-name path.
    let repo_dir = repo_dir
        .canonicalize()
        .unwrap_or_else(|_| repo_dir.to_path_buf());
    // UX-018: registration now strips that `\\?\` verbatim prefix before storing
    // the key and db path, so normalize the comparison target the same way — the
    // canonicalized path still carries the prefix and would otherwise not match.
    let repo_dir_str = repo_dir.to_string_lossy();
    let repo_dir_norm = travsr_store::registry::strip_verbatim_prefix(&repo_dir_str);
    // #454: a row is either the legacy bare db-path string or `{ db_path, ... }`.
    let db_path = |v: &serde_json::Value| -> Option<String> {
        v.as_str()
            .or_else(|| v.get("db_path")?.as_str())
            .map(str::to_owned)
    };
    repos
        .iter()
        .find(|(_, v)| {
            db_path(v)
                .map(|s| s.contains(repo_dir_norm.trim_end_matches('/')))
                .unwrap_or(false)
        })
        .map(|(k, _)| k.clone())
        .unwrap_or_else(|| panic!("no registry entry found for {repo_dir:?}, got: {json}"))
}

// ── AC1: tools/list in both modes, valid schemas ────────────────────────────

#[test]
fn ac1_stdio_tools_list_has_all_three_with_additional_properties_false() {
    let tmp = tempfile::tempdir().unwrap();
    init_test_repo(tmp.path());

    let responses = run_mcp_stdio(tmp.path(), &[INIT_MSG, tools_list_msg()]);
    let tools = responses[1]["result"]["tools"].as_array().unwrap();
    for name in ["get_index_status", "get_daemon_logs", "get_graph_health"] {
        let tool = tools
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("stdio tools/list missing {name}"));
        assert_eq!(
            tool["inputSchema"]["additionalProperties"], false,
            "{name} stdio schema must set additionalProperties: false"
        );
        assert!(
            tool["inputSchema"]["properties"]["repo"].is_null(),
            "{name} stdio schema must not expose 'repo'"
        );
    }
}

#[test]
fn ac1_global_tools_list_has_all_three_with_additional_properties_false_and_repo() {
    let fake_home = tempfile::tempdir().unwrap();

    let responses = run_mcp_global(fake_home.path(), &[INIT_MSG, tools_list_msg()]);
    let tools = responses[1]["result"]["tools"].as_array().unwrap();
    for name in ["get_index_status", "get_daemon_logs", "get_graph_health"] {
        let tool = tools
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("global tools/list missing {name}"));
        assert_eq!(
            tool["inputSchema"]["additionalProperties"], false,
            "{name} global schema must set additionalProperties: false"
        );
        assert!(
            tool["inputSchema"]["properties"]["repo"].is_object(),
            "{name} global schema must expose 'repo'"
        );
        assert_eq!(
            tool["inputSchema"]["required"].as_array().unwrap().len(),
            0,
            "{name} must have no required fields"
        );
    }
}

// ── AC5a: path traversal in `repo` (global mode) ────────────────────────────

#[test]
fn ac5a_path_traversal_repo_arg_rejected_identically_across_all_three_tools() {
    let fake_home = tempfile::tempdir().unwrap(); // no repos registered at all

    for payload in ["../../etc", "/etc", "%2e%2e%2f"] {
        for (tool, args) in [
            ("get_index_status", serde_json::json!({ "repo": payload })),
            (
                "get_daemon_logs",
                serde_json::json!({ "repo": payload, "tail": 5 }),
            ),
            ("get_graph_health", serde_json::json!({ "repo": payload })),
        ] {
            let responses = run_mcp_global(fake_home.path(), &[INIT_MSG, &call_msg(tool, args)]);
            let text = tool_text(&responses[1]);
            let payload_json = envelope_json(&text);
            let err = payload_json["error"].as_str().unwrap_or_else(|| {
                panic!("{tool}({payload:?}) must return an error payload, got: {payload_json}")
            });
            assert!(
                err.starts_with("unknown repo"),
                "{tool}({payload:?}) must use the anti-probe 'unknown repo' text, got: {err}"
            );
            assert!(
                !text.contains("/etc") && !text.contains(".."),
                "{tool}({payload:?}) must not echo the traversal payload back: {text}"
            );
        }
    }
}

// ── AC5b: cross-repo leak (global mode, two registered repos) ──────────────

fn setup_two_repos_with_markers(
    fake_home: &Path,
) -> (String, String, tempfile::TempDir, tempfile::TempDir) {
    let tmp_a = tempfile::tempdir().unwrap();
    let tmp_b = tempfile::tempdir().unwrap();
    init_test_repo_env(tmp_a.path(), &[("HOME", &fake_home.to_string_lossy())]);
    init_test_repo_env(tmp_b.path(), &[("HOME", &fake_home.to_string_lossy())]);

    let travsr_a = tmp_a.path().join(".travsr");
    let travsr_b = tmp_b.path().join(".travsr");
    write_log_line(
        &travsr_a,
        "daemon.log.2026-01-01",
        "2026-01-01T00:00:00Z INFO mod: MARKER_REPO_A_SECRET_LINE\n",
    );
    write_log_line(
        &travsr_b,
        "daemon.log.2026-01-01",
        "2026-01-01T00:00:00Z INFO mod: MARKER_REPO_B_SECRET_LINE\n",
    );

    let key_a = registry_key_for(fake_home, tmp_a.path());
    let key_b = registry_key_for(fake_home, tmp_b.path());
    // Return the TempDir guards themselves so the caller keeps the
    // directories alive (and auto-cleaned on drop) for the test's duration.
    (key_a, key_b, tmp_a, tmp_b)
}

/// Regression test (was a bug reproduction): a registry key is always the
/// absolute repo-root path (`travsr_store::registry::register` /
/// `travsr-daemon/src/lib.rs`'s `registry_key = repo_root.to_string_lossy()`).
/// `resolve_single_repo` now validates `repo_arg` with the narrower
/// `validate_mcp_repo_key_arg` (byte-length cap + null-byte rejection only,
/// see its doc comment in `sanitize.rs` for why that is safe here
/// specifically) instead of the shared `validate_mcp_arg`, so a real,
/// absolute registry key can match. This is #636-scoped only: every other
/// global-mode tool's `repo` argument (`collect_global`, `tools.rs`) still
/// has the wider systemic issue, filed separately.
#[test]
fn ac5c_explicit_repo_arg_matches_a_real_registry_key() {
    let fake_home = tempfile::tempdir().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    init_test_repo_env(tmp.path(), &[("HOME", &fake_home.path().to_string_lossy())]);
    let key = registry_key_for(fake_home.path(), tmp.path());
    assert!(
        Path::new(&key).is_absolute(),
        "sanity: registry keys are absolute paths on this platform, got: {key}"
    );

    let responses = run_mcp_global(
        fake_home.path(),
        &[
            INIT_MSG,
            &call_msg("get_index_status", serde_json::json!({ "repo": key })),
        ],
    );
    let payload = envelope_json(&tool_text(&responses[1]));
    assert!(
        payload.get("error").is_none(),
        "a real, live registry key must resolve successfully when passed as `repo`, \
         got the anti-probe 'unknown repo' error instead (repo disambiguation is \
         unusable end-to-end for every global-mode tool, not just #636's): {payload}"
    );
}

#[test]
fn ac5b_get_daemon_logs_named_repo_never_returns_the_other_repos_marker() {
    let fake_home = tempfile::tempdir().unwrap();
    let (key_a, key_b, path_a, path_b) = setup_two_repos_with_markers(fake_home.path());

    let responses = run_mcp_global(
        fake_home.path(),
        &[
            INIT_MSG,
            &call_msg(
                "get_daemon_logs",
                serde_json::json!({ "repo": key_a, "tail": 10 }),
            ),
        ],
    );
    let text = tool_text(&responses[1]);
    assert!(
        text.contains("MARKER_REPO_A_SECRET_LINE"),
        "repo A's own logs must be visible when named by its real registry \
         key (see ac5c_explicit_repo_arg_matches_a_real_registry_key): {text}"
    );
    assert!(
        !text.contains("MARKER_REPO_B_SECRET_LINE"),
        "repo A's call must never leak repo B's marker: {text}"
    );

    let _ = key_b; // only used to prove the registry has two live entries
    let _ = (path_a, path_b); // keep tempdirs alive for the duration of the test
}

#[test]
fn ac5b_get_daemon_logs_omitted_repo_with_two_registered_refuses_and_leaks_neither() {
    let fake_home = tempfile::tempdir().unwrap();
    let (_key_a, _key_b, path_a, path_b) = setup_two_repos_with_markers(fake_home.path());

    let responses = run_mcp_global(
        fake_home.path(),
        &[
            INIT_MSG,
            &call_msg("get_daemon_logs", serde_json::json!({ "tail": 10 })),
        ],
    );
    let text = tool_text(&responses[1]);
    let payload = envelope_json(&text);
    let err = payload["error"]
        .as_str()
        .unwrap_or_else(|| panic!("omitted repo with 2 registered must error, got: {payload}"));
    assert!(
        err.starts_with("ambiguous"),
        "must be the ambiguity error, got: {err}"
    );
    assert!(
        !text.contains("MARKER_REPO_A_SECRET_LINE") && !text.contains("MARKER_REPO_B_SECRET_LINE"),
        "ambiguous refusal must leak neither repo's logs: {text}"
    );
    let _ = (path_a, path_b);
}

#[test]
fn ac5b_get_index_status_and_get_graph_health_also_refuse_when_ambiguous() {
    let fake_home = tempfile::tempdir().unwrap();
    let (_key_a, _key_b, path_a, path_b) = setup_two_repos_with_markers(fake_home.path());

    for tool in ["get_index_status", "get_graph_health"] {
        let responses = run_mcp_global(
            fake_home.path(),
            &[INIT_MSG, &call_msg(tool, serde_json::json!({}))],
        );
        let text = tool_text(&responses[1]);
        let payload = envelope_json(&text);
        let err = payload["error"].as_str().unwrap_or_else(|| {
            panic!("{tool} with 2 registered repos and no `repo` arg must error, got: {payload}")
        });
        assert!(
            err.starts_with("ambiguous"),
            "{tool} must be the ambiguity error, got: {err}"
        );
    }
    let _ = (path_a, path_b);
}

// ── X1: secret / home-path redaction reaching a real response ──────────────

#[test]
fn x1_get_daemon_logs_redacts_bearer_token_and_home_path_in_real_response() {
    let tmp = tempfile::tempdir().unwrap();
    init_test_repo(tmp.path());
    let travsr_dir = tmp.path().join(".travsr");
    // "token=abc123XYZ" is deliberately NOT in this line, see the dedicated
    // (failing) reproduction below for that case.
    write_log_line(
        &travsr_dir,
        "daemon.log.2026-01-01",
        "2026-01-01T00:00:00Z ERROR mod: failed for repo=/home/qa_secret_user/proj \
         Authorization: Bearer sekrit.token.value\n",
    );

    let responses = run_mcp_stdio(
        tmp.path(),
        &[
            INIT_MSG,
            &call_msg("get_daemon_logs", serde_json::json!({ "tail": 10 })),
        ],
    );
    let text = tool_text(&responses[1]);
    assert!(
        !text.contains("/home/qa_secret_user"),
        "home path must not reach the response: {text}"
    );
    assert!(
        !text.contains("sekrit.token.value"),
        "bearer token must not reach the response: {text}"
    );
    // The line itself must still have been returned (redacted, not dropped).
    let payload = envelope_json(&text);
    assert_eq!(payload["entries"].as_array().unwrap().len(), 1);
}

/// Regression test (was a bug reproduction): `token=`/`secret=`/`password=`-
/// shaped trailing fields are exactly what `split_message_and_fields` pulls
/// OUT of the message into the structured `fields{}` map (this is what
/// `tracing`'s own field formatting produces). Once split out, the bare VALUE
/// ("abc123XYZ") no longer carries the key name ("token") for
/// `redact_key_value_pairs`'s literal `name=value` match to find.
/// `build_log_entry` now checks each field's own key against
/// `sanitize::is_sensitive_key` directly and redacts by key, independent of
/// the value's shape, closing that gap.
#[test]
fn x1_key_value_secret_in_a_parsed_tracing_field_is_redacted() {
    let tmp = tempfile::tempdir().unwrap();
    init_test_repo(tmp.path());
    let travsr_dir = tmp.path().join(".travsr");
    write_log_line(
        &travsr_dir,
        "daemon.log.2026-01-01",
        "2026-01-01T00:00:00Z ERROR mod: auth failed token=abc123XYZ\n",
    );

    let responses = run_mcp_stdio(
        tmp.path(),
        &[
            INIT_MSG,
            &call_msg("get_daemon_logs", serde_json::json!({ "tail": 10 })),
        ],
    );
    let text = tool_text(&responses[1]);
    assert!(
        !text.contains("abc123XYZ"),
        "X1 requires key=value secrets to be redacted from the real response, \
         even after the parser splits `token=abc123XYZ` into fields{{\"token\": \
         \"abc123XYZ\"}}: {text}"
    );
}

#[test]
fn x1_get_daemon_logs_redacts_github_and_aws_style_tokens() {
    let tmp = tempfile::tempdir().unwrap();
    init_test_repo(tmp.path());
    let travsr_dir = tmp.path().join(".travsr");
    write_log_line(
        &travsr_dir,
        "daemon.log.2026-01-01",
        "2026-01-01T00:00:00Z ERROR mod: leaked ghp_ABCDEFGHIJ0123456789 and AKIAABCDEFGHIJKLMNOP\n",
    );

    let responses = run_mcp_stdio(
        tmp.path(),
        &[
            INIT_MSG,
            &call_msg("get_daemon_logs", serde_json::json!({ "tail": 10 })),
        ],
    );
    let text = tool_text(&responses[1]);
    assert!(!text.contains("ghp_ABCDEFGHIJ0123456789"), "got: {text}");
    assert!(!text.contains("AKIAABCDEFGHIJKLMNOP"), "got: {text}");
}

// ── X2: byte cap acts independently of the tail cap ─────────────────────────

#[test]
fn x2_byte_cap_truncates_before_the_tail_cap_is_reached() {
    let tmp = tempfile::tempdir().unwrap();
    init_test_repo(tmp.path());
    let travsr_dir = tmp.path().join(".travsr");

    // 80 lines, each with a ~400-byte message, deliberately UNDER the
    // per-field 512-byte cap so no single entry gets clipped there, isolating
    // the *total* byte cap as the only thing that can truncate here. 80
    // entries at ~490 bytes serialized each is ~39 KB raw, over the 32 KiB
    // total cap, while `tail=80` requests exactly as many lines as exist (so
    // the *tail* cap alone would never trigger truncation: 80 lines exist,
    // 80 are requested). Any truncation observed here must come from the
    // byte cap, proving X2 is independent of the tail cap (#636).
    let filler = "x".repeat(400);
    let mut lines = String::new();
    for i in 0..80 {
        lines.push_str(&format!(
            "2026-01-01T00:{:02}:{:02}Z INFO mod: line {i} {filler}\n",
            i / 60,
            i % 60
        ));
    }
    write_log_line(&travsr_dir, "daemon.log.2026-01-01", &lines);

    let responses = run_mcp_stdio(
        tmp.path(),
        &[
            INIT_MSG,
            &call_msg("get_daemon_logs", serde_json::json!({ "tail": 80 })),
        ],
    );
    let text = tool_text(&responses[1]);
    let payload = envelope_json(&text);
    assert_eq!(
        payload["truncated"], true,
        "byte cap must trigger truncated=true even though tail (80) == lines available (80): {payload}"
    );
    let returned = payload["returned"].as_u64().unwrap();
    assert!(
        returned < 80,
        "byte cap must return fewer than the requested/available 80 lines, got {returned}"
    );
    // The overall response must stay well clear of the raw ~39 KB unbounded size.
    assert!(
        text.len() < 45_000,
        "response must be bounded by the byte cap, got {} bytes",
        text.len()
    );
}

// ── AC4: no-mutation through the real `open_read_only` path (global mode) ──

#[test]
fn ac4_get_graph_health_global_never_mutates_the_db_file_on_disk() {
    let fake_home = tempfile::tempdir().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    init_test_repo_env(tmp.path(), &[("HOME", &fake_home.path().to_string_lossy())]);
    let db_path = tmp.path().join(".travsr").join("graph.db");

    let before = std::fs::metadata(&db_path).unwrap();

    // `repo` is deliberately omitted: this registry has exactly one live
    // entry, so `resolve_single_repo`'s sole-live-entry fallback applies. An
    // explicit `repo=<registry key>` would work too now (see
    // ac5c_explicit_repo_arg_matches_a_real_registry_key), but the omitted-
    // repo fallback is the more direct path for what this test proves (no
    // mutation through the real `open_read_only` path).
    let responses = run_mcp_global(
        fake_home.path(),
        &[
            INIT_MSG,
            &call_msg("get_graph_health", serde_json::json!({})),
        ],
    );
    let text = tool_text(&responses[1]);
    let payload = envelope_json(&text);
    assert!(
        payload.get("error").is_none(),
        "get_graph_health_global must succeed against a freshly-inited repo (proves the \
         open_read_only + query_only=ON path did not hard-fail on an attempted write): {payload}"
    );
    assert!(payload["healthy"].is_boolean(), "got: {payload}");

    let after = std::fs::metadata(&db_path).unwrap();
    assert_eq!(
        before.len(),
        after.len(),
        "db file size must be unchanged after a read-only get_graph_health call"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        assert_eq!(
            before.mtime(),
            after.mtime(),
            "db file mtime must be unchanged after a read-only get_graph_health call"
        );
    }
}

#[test]
fn ac4_healthy_clean_repo_has_no_recommendation_field() {
    let tmp = tempfile::tempdir().unwrap();
    git_init(tmp.path());
    // A single self-contained file with no cross-file calls. The `ts-callers`
    // fixture used elsewhere in this file (controller.ts calling
    // service.ts's PaymentService.charge) reproducibly reports 2 orphan
    // edges from a stock `travsr init` with no edits at all, a pre-existing
    // condition of that fixture/indexer pairing, independent of #636 (see
    // this file's bug report). Use a fixture with no such pre-existing
    // integrity noise so this test actually proves the healthy-report shape.
    std::fs::write(
        tmp.path().join("single.ts"),
        "export function add(a: number, b: number): number {\n  return a + b;\n}\n",
    )
    .unwrap();
    let status = Command::new(cargo_bin("travsr"))
        .arg("init")
        .current_dir(tmp.path())
        .env("TRAVSR_DISABLE_REGISTRY", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("travsr init");
    assert!(status.success());

    let responses = run_mcp_stdio(
        tmp.path(),
        &[
            INIT_MSG,
            &call_msg("get_graph_health", serde_json::json!({})),
        ],
    );
    let text = tool_text(&responses[1]);
    let payload = envelope_json(&text);
    assert_eq!(payload["healthy"], true, "got: {payload}");
    assert!(
        payload.get("recommendation").is_none(),
        "healthy report must omit recommendation, got: {payload}"
    );
    assert_eq!(payload["orphan_edges"], 0);
    assert_eq!(payload["ghost_paths"]["count"], 0);
    assert_eq!(payload["lexical_index_parity"]["ok"], true);
}

#[test]
fn ac4_ghost_path_makes_repo_unhealthy_and_recommends_fsck_over_real_db() {
    let tmp = tempfile::tempdir().unwrap();
    init_test_repo(tmp.path());
    // Delete a file that was indexed, without reindexing. The DB still
    // thinks it's tracked → ghost path, over a real (not in-memory) db.
    std::fs::remove_file(tmp.path().join("service.ts")).unwrap();

    let responses = run_mcp_stdio(
        tmp.path(),
        &[
            INIT_MSG,
            &call_msg("get_graph_health", serde_json::json!({})),
        ],
    );
    let text = tool_text(&responses[1]);
    let payload = envelope_json(&text);
    assert_eq!(payload["healthy"], false, "got: {payload}");
    assert!(
        payload["ghost_paths"]["count"].as_u64().unwrap() >= 1,
        "got: {payload}"
    );
    assert!(
        payload["recommendation"]
            .as_str()
            .unwrap()
            .contains("fsck --fix"),
        "got: {payload}"
    );

    // Never mutates: calling it again must report the identical ghost, not
    // an auto-fixed (count=0) result.
    let responses2 = run_mcp_stdio(
        tmp.path(),
        &[
            INIT_MSG,
            &call_msg("get_graph_health", serde_json::json!({})),
        ],
    );
    let payload2 = envelope_json(&tool_text(&responses2[1]));
    assert_eq!(
        payload["ghost_paths"]["count"], payload2["ghost_paths"]["count"],
        "repeated calls must not change the ghost count (no --fix sweep ever runs)"
    );
}

// ── AC2: staleness against a real advancing git repo ────────────────────────

#[test]
fn ac2_behind_by_nonzero_after_real_commits_past_the_indexed_head() {
    let tmp = tempfile::tempdir().unwrap();
    git_init(tmp.path());
    git(&["config", "user.email", "test@example.com"], tmp.path());
    git(&["config", "user.name", "Test"], tmp.path());
    std::fs::write(
        tmp.path().join("a.ts"),
        "export function seedFn() { return 1; }\n",
    )
    .unwrap();
    git(&["add", "."], tmp.path());
    git(&["commit", "-q", "-m", "init"], tmp.path());
    let status = Command::new(cargo_bin("travsr"))
        .arg("init")
        .current_dir(tmp.path())
        .env("TRAVSR_DISABLE_REGISTRY", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("travsr init");
    assert!(status.success());

    // `travsr init` installs a `post-commit` hook that auto-reindexes on every
    // local commit (CLAUDE.md principle 2: "always fresh"). Remove it so the
    // next two commits land WITHOUT a reindex, reproducing the real-world
    // staleness scenario this field exists to detect: hooks are local-only
    // (never transmitted by `git clone`/`git pull`), so a checkout that
    // received commits by another means (fresh clone, CI checkout, a commit
    // made before `travsr init` first ran) is exactly this state.
    std::fs::remove_file(tmp.path().join(".git/hooks/post-commit")).unwrap();

    // Advance HEAD twice without reindexing.
    for (name, content) in [("b.txt", "two"), ("c.txt", "three")] {
        std::fs::write(tmp.path().join(name), content).unwrap();
        git(&["add", "."], tmp.path());
        git(&["commit", "-q", "-m", name], tmp.path());
    }

    let responses = run_mcp_stdio(
        tmp.path(),
        &[
            INIT_MSG,
            &call_msg("get_index_status", serde_json::json!({})),
        ],
    );
    let payload = envelope_json(&tool_text(&responses[1]));
    assert_eq!(payload["staleness"]["behind_by"], 2, "got: {payload}");
    assert_eq!(payload["staleness"]["is_stale"], true, "got: {payload}");
    assert_ne!(
        payload["indexed_commit"], payload["head_commit"],
        "got: {payload}"
    );
    assert!(
        payload["counts"]["nodes"].as_u64().unwrap() > 0,
        "got: {payload}"
    );
}

#[test]
fn ac2_behind_by_zero_and_not_stale_immediately_after_init() {
    let tmp = tempfile::tempdir().unwrap();
    git_init(tmp.path());
    git(&["config", "user.email", "test@example.com"], tmp.path());
    git(&["config", "user.name", "Test"], tmp.path());
    std::fs::write(
        tmp.path().join("a.ts"),
        "export function seedFn() { return 1; }\n",
    )
    .unwrap();
    git(&["add", "."], tmp.path());
    git(&["commit", "-q", "-m", "init"], tmp.path());
    let status = Command::new(cargo_bin("travsr"))
        .arg("init")
        .current_dir(tmp.path())
        .env("TRAVSR_DISABLE_REGISTRY", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("travsr init");
    assert!(status.success());

    let responses = run_mcp_stdio(
        tmp.path(),
        &[
            INIT_MSG,
            &call_msg("get_index_status", serde_json::json!({})),
        ],
    );
    let payload = envelope_json(&tool_text(&responses[1]));
    assert_eq!(payload["staleness"]["behind_by"], 0, "got: {payload}");
    assert_eq!(payload["staleness"]["is_stale"], false, "got: {payload}");
    assert_eq!(
        payload["indexed_commit"], payload["head_commit"],
        "got: {payload}"
    );
}

// ── Negative / edge cases the plan calls out ────────────────────────────────

#[test]
fn edge_empty_log_dir_returns_empty_entries_gracefully() {
    let tmp = tempfile::tempdir().unwrap();
    init_test_repo(tmp.path());
    // .travsr exists (created by init) but has no daemon.log* files.

    let responses = run_mcp_stdio(
        tmp.path(),
        &[
            INIT_MSG,
            &call_msg("get_daemon_logs", serde_json::json!({})),
        ],
    );
    let payload = envelope_json(&tool_text(&responses[1]));
    assert_eq!(
        payload["entries"].as_array().unwrap().len(),
        0,
        "got: {payload}"
    );
    assert_eq!(payload["returned"], 0, "got: {payload}");
    assert_eq!(payload["truncated"], false, "got: {payload}");
    // `source` lists the files that actually contributed entries (#636
    // review), so a known repo root with zero daemon.log* files reads `[]`.
    // It is an array in every branch, including "repo root unknown" (#636
    // round-2 review:
    // `observability::tests::daemon_logs_source_is_an_empty_array_when_repo_root_unknown`).
    assert_eq!(payload["source"], serde_json::json!([]), "got: {payload}");
    assert_eq!(payload["daemon_running"], false, "got: {payload}");
}

#[test]
fn edge_zero_line_log_file_does_not_crash() {
    let tmp = tempfile::tempdir().unwrap();
    init_test_repo(tmp.path());
    let travsr_dir = tmp.path().join(".travsr");
    write_log_line(&travsr_dir, "daemon.log.2026-01-01", "");

    let responses = run_mcp_stdio(
        tmp.path(),
        &[
            INIT_MSG,
            &call_msg("get_daemon_logs", serde_json::json!({})),
        ],
    );
    let payload = envelope_json(&tool_text(&responses[1]));
    assert_eq!(
        payload["entries"].as_array().unwrap().len(),
        0,
        "got: {payload}"
    );
}

#[cfg(unix)]
#[test]
fn edge_symlinked_daemon_log_is_rejected_not_followed() {
    let tmp = tempfile::tempdir().unwrap();
    init_test_repo(tmp.path());
    let travsr_dir = tmp.path().join(".travsr");

    // A real file OUTSIDE .travsr with sensitive-looking content...
    let outside = tmp.path().join("outside_real_log.txt");
    std::fs::write(
        &outside,
        "2026-01-01T00:00:00Z ERROR mod: SYMLINK_ESCAPE_MARKER\n",
    )
    .unwrap();
    // ...symlinked INTO .travsr under a name that matches the daemon.log* glob.
    std::os::unix::fs::symlink(&outside, travsr_dir.join("daemon.log.2026-01-01")).unwrap();

    let responses = run_mcp_stdio(
        tmp.path(),
        &[
            INIT_MSG,
            &call_msg("get_daemon_logs", serde_json::json!({})),
        ],
    );
    let text = tool_text(&responses[1]);
    assert!(
        !text.contains("SYMLINK_ESCAPE_MARKER"),
        "a symlinked daemon.log must never be followed/read: {text}"
    );
    let payload = envelope_json(&text);
    assert_eq!(
        payload["entries"].as_array().unwrap().len(),
        0,
        "got: {payload}"
    );
}

#[test]
fn edge_tail_zero_clamps_to_one_not_zero_or_panic() {
    let tmp = tempfile::tempdir().unwrap();
    init_test_repo(tmp.path());
    let travsr_dir = tmp.path().join(".travsr");
    write_log_line(
        &travsr_dir,
        "daemon.log.2026-01-01",
        "2026-01-01T00:00:00Z INFO mod: only line\n",
    );

    // tail=0 bypasses the declared schema minimum (a client is not obligated
    // to honor it); the server must still clamp rather than panic or return 0.
    let responses = run_mcp_stdio(
        tmp.path(),
        &[
            INIT_MSG,
            &call_msg("get_daemon_logs", serde_json::json!({ "tail": 0 })),
        ],
    );
    let payload = envelope_json(&tool_text(&responses[1]));
    assert_eq!(payload["returned"], 1, "got: {payload}");
}

#[test]
fn edge_tail_above_max_clamps_to_max_not_panic() {
    let tmp = tempfile::tempdir().unwrap();
    init_test_repo(tmp.path());
    let travsr_dir = tmp.path().join(".travsr");
    write_log_line(
        &travsr_dir,
        "daemon.log.2026-01-01",
        "2026-01-01T00:00:00Z INFO mod: only line\n",
    );

    let responses = run_mcp_stdio(
        tmp.path(),
        &[
            INIT_MSG,
            &call_msg("get_daemon_logs", serde_json::json!({ "tail": 99_999 })),
        ],
    );
    let payload = envelope_json(&tool_text(&responses[1]));
    assert!(
        payload["returned"].as_u64().unwrap() <= 500,
        "tail must clamp to MAX_TAIL=500, got: {payload}"
    );
}

#[test]
fn edge_unknown_level_value_defaults_to_info_not_all_or_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    init_test_repo(tmp.path());
    let travsr_dir = tmp.path().join(".travsr");
    let lines = "2026-01-01T00:00:00Z ERROR mod: err line\n\
                 2026-01-01T00:00:01Z INFO mod: info line\n\
                 2026-01-01T00:00:02Z DEBUG mod: debug line\n";
    write_log_line(&travsr_dir, "daemon.log.2026-01-01", lines);

    let responses = run_mcp_stdio(
        tmp.path(),
        &[
            INIT_MSG,
            &call_msg(
                "get_daemon_logs",
                serde_json::json!({ "tail": 10, "level": "verbose" }),
            ),
        ],
    );
    let payload = envelope_json(&tool_text(&responses[1]));
    let levels: Vec<String> = payload["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["level"].as_str().unwrap().to_string())
        .collect();
    assert!(levels.contains(&"ERROR".to_string()), "got: {levels:?}");
    assert!(levels.contains(&"INFO".to_string()), "got: {levels:?}");
    assert!(
        !levels.contains(&"DEBUG".to_string()),
        "unknown level value must default to info's severity (DEBUG excluded), got: {levels:?}"
    );
}

#[test]
fn edge_daemon_logs_and_index_status_never_create_daemon_lock() {
    let tmp = tempfile::tempdir().unwrap();
    init_test_repo(tmp.path());

    run_mcp_stdio(
        tmp.path(),
        &[
            INIT_MSG,
            &call_msg("get_daemon_logs", serde_json::json!({})),
            &call_msg("get_index_status", serde_json::json!({})),
            &call_msg("get_graph_health", serde_json::json!({})),
        ],
    );
    assert!(
        !tmp.path().join(".travsr").join("daemon.lock").exists(),
        "read-only observability tools must never create .travsr/daemon.lock"
    );
}

// ── get_graph_health cost/behaviour on a larger repo ────────────────────────

#[test]
fn get_graph_health_completes_promptly_and_stays_correct_on_a_larger_repo() {
    let tmp = tempfile::tempdir().unwrap();
    git_init(tmp.path());
    git(&["config", "user.email", "test@example.com"], tmp.path());
    git(&["config", "user.name", "Test"], tmp.path());

    // ~300 small TS files, each a trivially indexable exported function.
    const FILE_COUNT: usize = 300;
    for i in 0..FILE_COUNT {
        std::fs::write(
            tmp.path().join(format!("f{i}.ts")),
            format!("export function fn{i}() {{ return {i}; }}\n"),
        )
        .unwrap();
    }
    git(&["add", "."], tmp.path());
    git(&["commit", "-q", "-m", "many files"], tmp.path());

    let status = Command::new(cargo_bin("travsr"))
        .arg("init")
        .current_dir(tmp.path())
        .env("TRAVSR_DISABLE_REGISTRY", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("travsr init");
    assert!(status.success());

    // Delete a handful of files without reindexing to prove ghost detection
    // stays correct at this file count too, not just at fixture scale.
    for i in 0..7 {
        std::fs::remove_file(tmp.path().join(format!("f{i}.ts"))).unwrap();
    }

    let start = std::time::Instant::now();
    let responses = run_mcp_stdio(
        tmp.path(),
        &[
            INIT_MSG,
            &call_msg("get_graph_health", serde_json::json!({})),
        ],
    );
    let elapsed = start.elapsed();

    let payload = envelope_json(&tool_text(&responses[1]));
    assert_eq!(payload["healthy"], false, "got: {payload}");
    assert_eq!(
        payload["ghost_paths"]["count"], 7,
        "O(F) ghost scan must still be exact at {FILE_COUNT} tracked files: {payload}"
    );
    assert_eq!(
        payload["ghost_paths"]["sample"].as_array().unwrap().len(),
        7,
        "sample stays uncapped below the 20-item ceiling: {payload}"
    );
    assert!(
        elapsed < Duration::from_secs(15),
        "get_graph_health over {FILE_COUNT} tracked files took {elapsed:?}, \
         investigate the O(F) stat loop for a regression"
    );
}
