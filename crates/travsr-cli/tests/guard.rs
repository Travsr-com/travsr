//! End-to-end tests for `travsr guard` and its installation (#916).
//!
//! These drive the real binary over a real stdin/stdout pipe against a real
//! index, because that is the only place the contract actually lives: a unit
//! test of the decision function cannot catch a guard that writes its JSON to
//! stderr, exits non-zero, or blocks because the process died before printing.
//!
//! The invariant almost every test here asserts is the same one:
//! **the guard did not block the call.** It is spelled [`assert_allows`]
//! throughout, and it accepts both shapes that mean it: an explicit
//! `permissionDecision: "allow"` and an empty stdout, which the host documents
//! as "no decision; normal permission flow applies". Where the distinction
//! matters the test says so explicitly.

use std::path::Path;
use std::process::Command as StdCommand;

use assert_cmd::Command;
use serde_json::{json, Value};

// ── fixtures ────────────────────────────────────────────────────────────────

fn git(dir: &Path, args: &[&str]) {
    let ok = StdCommand::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok, "git {args:?} failed in {}", dir.display());
}

/// A git repo with one indexed Rust file holding a distinctive symbol, indexed
/// at `HEAD` so the guard's staleness check passes.
///
/// `charge_payment` rather than something like `charge`: the name has to be
/// absent from the test's own noise and long enough to clear the guard's
/// minimum identifier length.
fn indexed_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    git(dir, &["-c", "init.defaultBranch=main", "init", "-q"]);
    git(dir, &["config", "user.email", "t@t.com"]);
    git(dir, &["config", "user.name", "T"]);
    // The fixture writes LF and the developer's global may be `autocrlf=true`,
    // which makes every `git add` warn once per file. Harmless, and it buries
    // the test output it interleaves with.
    git(dir, &["config", "core.autocrlf", "false"]);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src/pay.rs"),
        "pub fn charge_payment(amount: u64) -> u64 {\n    amount\n}\n\
         pub fn refund_payment(amount: u64) -> u64 {\n    charge_payment(amount)\n}\n",
    )
    .unwrap();
    std::fs::write(dir.join("README.md"), "# docs\n\ncharge_payment is here.\n").unwrap();
    std::fs::write(dir.join("Cargo.lock"), "# lock\n").unwrap();
    std::fs::create_dir_all(dir.join("node_modules/dep")).unwrap();
    std::fs::write(dir.join("node_modules/dep/index.js"), "// vendored\n").unwrap();
    // A git-ignored source file: real Rust, never in the index.
    std::fs::write(dir.join(".gitignore"), "/build/\n").unwrap();
    std::fs::create_dir_all(dir.join("build")).unwrap();
    std::fs::write(dir.join("build/generated.rs"), "pub fn generated() {}\n").unwrap();
    // A binary, and a workflow config: neither is code the graph carries.
    std::fs::create_dir_all(dir.join("assets")).unwrap();
    std::fs::write(dir.join("assets/logo.png"), [0x89u8, 0x50, 0x4E, 0x47]).unwrap();
    std::fs::create_dir_all(dir.join(".github/workflows")).unwrap();
    std::fs::write(dir.join(".github/workflows/ci.yml"), "on: push\n").unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-qm", "seed"]);

    travsr(dir)
        .args(["init", "--quiet", "--no-connect"])
        .assert()
        .success();
    tmp
}

fn travsr(dir: &Path) -> Command {
    let mut c = Command::cargo_bin("travsr").unwrap();
    c.env("TRAVSR_DISABLE_REGISTRY", "1")
        // The guard reads `guard.mode` through the normal layering, which
        // includes `~/.travsr/config.toml`. Point HOME at the fixture so a
        // developer's own global config cannot decide a test.
        .env("HOME", dir)
        .env("USERPROFILE", dir)
        .current_dir(dir);
    c
}

fn set_mode(dir: &Path, mode: &str) {
    travsr(dir)
        .args(["config", "set", "guard.mode", mode, "--repo"])
        .assert()
        .success();
}

/// Run the guard against `payload` and return its stdout.
///
/// Every test here except the two that are *about* the deadline runs with a
/// generous one. The guard's real budget is 200ms and it fails open when it is
/// missed, which is correct in production and useless in a suite that runs a
/// dozen full `travsr init` runs at once: a strict-mode `deny` would then
/// depend on how loaded the machine happened to be. The deadline has its own
/// tests, here and in the unit suite; these are about what the guard decides.
fn guard(dir: &Path, payload: Value) -> String {
    guard_env(dir, payload, &[("TRAVSR_GUARD_DEADLINE_MS", "60000")])
}

fn guard_env(dir: &Path, mut payload: Value, env: &[(&str, &str)]) -> String {
    // The host sends an absolute `cwd`. The builders below write "." as a
    // stand-in for "this fixture"; a test that means somewhere else says so.
    if payload.get("cwd") == Some(&json!(".")) {
        payload["cwd"] = json!(dir.to_string_lossy());
    }
    let mut cmd = travsr(dir);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd
        .arg("guard")
        .write_stdin(payload.to_string())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "the guard must always exit 0 (a non-zero exit from a PreToolUse hook \
         is itself a signal); got {:?}, stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("stdout must be UTF-8")
}

/// The decision object, or `None` when the guard emitted nothing.
fn decision(stdout: &str) -> Option<Value> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return None;
    }
    let v: Value = serde_json::from_str(trimmed)
        .unwrap_or_else(|e| panic!("stdout must be one JSON object, got {trimmed:?}: {e}"));
    let specific = v
        .get("hookSpecificOutput")
        .unwrap_or_else(|| panic!("a decision must live under hookSpecificOutput: {v}"));
    assert_eq!(
        specific["hookEventName"], "PreToolUse",
        "the decision must name the event it answers: {v}"
    );
    Some(specific.clone())
}

/// The decision the guard made, if it made one.
///
/// `None` covers both shapes that mean "no decision": nothing on stdout at
/// all, and a `hookSpecificOutput` that carries only `additionalContext`. An
/// earlier version mapped the second to `Some("?")`, which was harmless while
/// every emitted object carried a decision and became wrong the moment the
/// guard stopped emitting `allow`.
fn verdict(stdout: &str) -> Option<String> {
    decision(stdout).and_then(|d| d["permissionDecision"].as_str().map(str::to_string))
}

/// The `additionalContext` the guard attached, if any.
fn context(stdout: &str) -> Option<String> {
    decision(stdout).and_then(|d| d["additionalContext"].as_str().map(str::to_string))
}

/// The property the whole fail-open contract reduces to: the call went through.
#[track_caller]
fn assert_allows(stdout: &str, what: &str) {
    match verdict(stdout).as_deref() {
        None => {}
        other => panic!("{what}: expected the call to go through, got {other:?}; {stdout}"),
    }
}

/// The guard emitted no `permissionDecision`, so the user's own permission
/// rules for that tool still apply.
///
/// The same check as [`assert_allows`] as long as `allow` is never emitted,
/// and deliberately a separate name: that one states the fail-open contract
/// ("the call went through"), this one states the no-auto-approve contract
/// ("and the guard did not spend the user's permission settings to do it").
/// If the two ever come apart, it will be because someone reintroduced
/// `allow`, and the call sites say which property they were relying on.
#[track_caller]
fn assert_no_decision(stdout: &str, what: &str) {
    assert_eq!(
        verdict(stdout),
        None,
        "{what}: the guard must not decide on this call; {stdout}"
    );
}

#[track_caller]
fn assert_denies(stdout: &str, what: &str) {
    assert_eq!(
        verdict(stdout).as_deref(),
        Some("deny"),
        "{what}: expected a deny; {stdout}"
    );
}

fn grep_for(term: &str) -> Value {
    json!({
        "session_id": "s-test",
        "hook_event_name": "PreToolUse",
        "cwd": ".",
        "tool_name": "Grep",
        "tool_input": { "pattern": term }
    })
}

fn bash(command: &str) -> Value {
    json!({
        "session_id": "s-test",
        "hook_event_name": "PreToolUse",
        "cwd": ".",
        "tool_name": "Bash",
        "tool_input": { "command": command }
    })
}

fn read(path: &str) -> Value {
    json!({
        "session_id": "s-test",
        "hook_event_name": "PreToolUse",
        "cwd": ".",
        "tool_name": "Read",
        "tool_input": { "file_path": path }
    })
}

/// A payload with a fresh session, so the strict-mode valve starts closed.
fn with_session(mut p: Value, session: &str) -> Value {
    p["session_id"] = json!(session);
    p
}

// ── the match set ───────────────────────────────────────────────────────────

#[test]
fn every_matched_operation_is_allowed_with_a_redirect_in_advisory_mode() {
    let tmp = indexed_repo();
    let dir = tmp.path();
    set_mode(dir, "advisory");

    let cases: Vec<(&str, Value)> = vec![
        ("Grep", grep_for("charge_payment")),
        (
            "Glob",
            json!({"session_id":"s","hook_event_name":"PreToolUse","cwd":".",
                        "tool_name":"Glob","tool_input":{"pattern":"**/*.rs"}}),
        ),
        ("Read", read("src/pay.rs")),
        ("bash grep", bash("grep -rn charge_payment src/")),
        ("bash rg", bash("rg charge_payment")),
        ("bash find", bash("find . -name '*.rs'")),
        ("bash ag", bash("ag charge_payment")),
        ("bash ack", bash("ack charge_payment")),
        ("bash ls -R", bash("ls -R")),
    ];

    for (what, payload) in cases {
        let out = guard(dir, payload);
        assert_no_decision(&out, what);
        let note =
            context(&out).unwrap_or_else(|| panic!("{what}: advisory must say something; {out}"));
        assert!(
            !note.trim().is_empty(),
            "{what}: an advisory nudge with no redirect teaches nothing"
        );
        assert!(
            note.contains("Travsr") || note.contains("travsr"),
            "{what}: the redirect must name where to go instead; {note}"
        );
    }
}

/// The hole this mode had, and the reason advisory carries no decision at all.
///
/// `redirect_for` returns `None` for exactly the paths the guard cannot vouch
/// for: outside the repository, a lockfile, a `.env`, anything the index does
/// not carry. Emitting `allow` for those would lift the user's own `Read`
/// gating on `~/.ssh/id_rsa` and friends, which is the same failure as
/// auto-approving `grep foo && rm -rf build` on the strength of its first word.
#[test]
fn advisory_never_auto_approves_a_read_it_cannot_vouch_for() {
    let tmp = indexed_repo();
    let dir = tmp.path();
    set_mode(dir, "advisory");

    let outside = if cfg!(windows) {
        "C:\\Windows\\System32\\drivers\\etc\\hosts"
    } else {
        "/etc/hosts"
    };
    let cases: Vec<(&str, Value)> = vec![
        ("a path outside the repository", read(outside)),
        ("a dotenv file", read(".env")),
        ("a lockfile", read("Cargo.lock")),
        ("a markdown file", read("README.md")),
        ("a vendored file", read("node_modules/dep/index.js")),
        ("a file that does not exist", read("src/nope.rs")),
    ];
    for (what, payload) in cases {
        let out = guard(dir, payload);
        assert_no_decision(&out, what);
        assert_eq!(
            context(&out),
            None,
            "{what}: the graph cannot answer for this file, so there is nothing \
             true to nudge toward; {out}"
        );
    }
}

/// The same rule on the search side: advisory teaches, it does not approve.
#[test]
fn advisory_decides_nothing_even_when_it_has_a_redirect() {
    let tmp = indexed_repo();
    let dir = tmp.path();
    set_mode(dir, "advisory");
    let out = guard(dir, with_session(grep_for("charge_payment"), "s-adv"));
    assert_no_decision(&out, "a search the graph can answer");
    assert!(
        context(&out).is_some_and(|c| c.contains("find_references")),
        "the redirect still has to reach the agent; {out}"
    );
}

#[test]
fn an_unrelated_bash_command_is_passed_through_untouched() {
    let tmp = indexed_repo();
    let dir = tmp.path();
    set_mode(dir, "strict");
    for cmd in ["cargo test", "npm run build", "git status", "ls src/"] {
        let out = guard(dir, bash(cmd));
        assert_eq!(
            verdict(&out),
            None,
            "`{cmd}` is outside the match set: the guard must emit no decision \
             at all, so the user's own permission settings still apply; {out}"
        );
    }
}

/// The reason an unmatched `Bash` call gets no decision rather than an explicit
/// allow: `permissionDecision: "allow"` is the host's auto-approve, and the
/// guard has no business spending it on a command it did not understand.
#[test]
fn a_compound_command_is_never_auto_approved() {
    let tmp = indexed_repo();
    let dir = tmp.path();
    set_mode(dir, "advisory");
    for cmd in [
        "grep -rn charge_payment . && rm -rf build",
        "grep charge_payment . | xargs rm",
        "rg charge_payment > /tmp/out",
    ] {
        let out = guard(dir, bash(cmd));
        assert_eq!(
            verdict(&out),
            None,
            "`{cmd}` must not be auto-approved on the strength of its first word \
            ; {out}"
        );
    }
}

#[test]
fn an_unknown_tool_is_passed_through() {
    let tmp = indexed_repo();
    let dir = tmp.path();
    set_mode(dir, "strict");
    for tool in ["Write", "Edit", "WebFetch", "SomeToolFromTheFuture"] {
        let out = guard(
            dir,
            json!({"hook_event_name":"PreToolUse","cwd":".","tool_name":tool,
                   "tool_input":{"file_path":"src/pay.rs"}}),
        );
        assert_allows(&out, tool);
        assert_eq!(verdict(&out), None, "{tool} is not the guard's business");
    }
}

// ── strict mode ─────────────────────────────────────────────────────────────

#[test]
fn strict_denies_a_search_the_graph_can_answer_and_names_the_replacement() {
    let tmp = indexed_repo();
    let dir = tmp.path();
    set_mode(dir, "strict");

    let out = guard(dir, with_session(grep_for("charge_payment"), "s-deny-1"));
    assert_denies(&out, "a grep for an indexed symbol");
    let reason = decision(&out).unwrap()["permissionDecisionReason"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        reason.contains("find_references(symbol=\"charge_payment\")"),
        "the deny has to hand over the exact call, arguments and all; {reason}"
    );
}

#[test]
fn strict_denies_a_whole_file_read_of_an_indexed_file() {
    let tmp = indexed_repo();
    let dir = tmp.path();
    set_mode(dir, "strict");
    let out = guard(dir, with_session(read("src/pay.rs"), "s-read-1"));
    assert_denies(&out, "a whole-file read of an indexed file");
    let reason = decision(&out).unwrap()["permissionDecisionReason"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        reason.contains("get_context") && reason.contains("src/pay.rs"),
        "the deny must name the file and the call that replaces the read; {reason}"
    );
}

/// A ranged read is a follow-up to something the graph already answered.
#[test]
fn strict_allows_a_ranged_read() {
    let tmp = indexed_repo();
    let dir = tmp.path();
    set_mode(dir, "strict");
    let out = guard(
        dir,
        json!({"session_id":"s-range","hook_event_name":"PreToolUse","cwd":".",
               "tool_name":"Read",
               "tool_input":{"file_path":"src/pay.rs","offset":1,"limit":20}}),
    );
    assert_allows(&out, "a ranged read");
}

#[test]
fn strict_allows_everything_the_graph_cannot_answer() {
    let tmp = indexed_repo();
    let dir = tmp.path();
    set_mode(dir, "strict");

    let cases: Vec<(&str, Value)> = vec![
        // A symbol the graph has never heard of.
        ("unknown symbol", grep_for("nonexistent_symbol_xyz")),
        // A regex is a text search, not a structural question.
        ("a regex", grep_for("fn .*charge")),
        ("an alternation", grep_for("charge|refund")),
        // Too short for an exact-name lookup to mean anything.
        ("a two-letter term", grep_for("id")),
        // File discovery: the graph indexes code files only, so it cannot
        // answer a question about the whole tree.
        (
            "a glob",
            json!({"session_id":"s","hook_event_name":"PreToolUse","cwd":".",
                          "tool_name":"Glob","tool_input":{"pattern":"**/*.rs"}}),
        ),
        ("find", bash("find . -name '*.rs'")),
        ("ls -R", bash("ls -R")),
        // Files the graph does not carry.
        ("a markdown read", read("README.md")),
        ("a lockfile read", read("Cargo.lock")),
        ("a vendored read", read("node_modules/dep/index.js")),
        ("a git-ignored read", read("build/generated.rs")),
        ("a binary read", read("assets/logo.png")),
        ("an untracked read", read("src/not_created_yet.rs")),
        ("a config read", read(".github/workflows/ci.yml")),
        ("a read outside the repo", read("/etc/hosts")),
    ];
    for (what, payload) in cases {
        assert_allows(&guard(dir, payload), what);
    }
}

/// Strict mode must never turn normal file access into an outage, so the
/// property is asserted as a whole and not only case by case.
#[test]
fn strict_blocks_the_match_set_and_nothing_else() {
    let tmp = indexed_repo();
    let dir = tmp.path();
    set_mode(dir, "strict");
    for cmd in [
        "cargo build",
        "git diff",
        "cat src/pay.rs",
        "sed -n 1,5p src/pay.rs",
        "python script.py",
    ] {
        assert_allows(&guard(dir, bash(cmd)), cmd);
    }
    // And a Write of the very file a Read of would have been denied.
    assert_allows(
        &guard(
            dir,
            json!({"hook_event_name":"PreToolUse","cwd":".","tool_name":"Write",
                   "tool_input":{"file_path":"src/pay.rs"}}),
        ),
        "a write",
    );
}

// ── the strict-mode release valve ───────────────────────────────────────────

#[test]
fn a_repeated_search_in_the_same_session_is_released() {
    let tmp = indexed_repo();
    let dir = tmp.path();
    set_mode(dir, "strict");
    let payload = with_session(grep_for("charge_payment"), "s-valve-repeat");

    assert_denies(&guard(dir, payload.clone()), "the first search");
    assert_allows(
        &guard(dir, payload.clone()),
        "the second search for the same term in the same session",
    );
    assert_allows(&guard(dir, payload), "and every one after it");
}

/// The scenario the issue describes: the agent asks Travsr, the graph comes
/// back empty, and the follow-up grep has to be allowed.
#[test]
fn a_search_after_a_travsr_query_is_released_immediately() {
    let tmp = indexed_repo();
    let dir = tmp.path();
    set_mode(dir, "strict");

    // The agent queries the graph. The guard sees it go past and allows it.
    let query = json!({
        "session_id": "s-valve-query",
        "hook_event_name": "PreToolUse",
        "cwd": ".",
        "tool_name": "mcp__travsr__get_callers",
        "tool_input": { "symbol": "charge_payment" }
    });
    assert_allows(&guard(dir, query), "a travsr query must never be blocked");

    // The follow-up search is not redirected: the agent has been to the graph.
    assert_allows(
        &guard(
            dir,
            with_session(grep_for("charge_payment"), "s-valve-query"),
        ),
        "the verification search after a travsr query",
    );
}

#[test]
fn a_release_does_not_leak_to_another_session() {
    let tmp = indexed_repo();
    let dir = tmp.path();
    set_mode(dir, "strict");
    let term = grep_for("charge_payment");

    assert_denies(&guard(dir, with_session(term.clone(), "s-a")), "session a");
    assert_allows(
        &guard(dir, with_session(term.clone(), "s-a")),
        "session a again",
    );
    assert_denies(
        &guard(dir, with_session(term, "s-b")),
        "a second session must still get its redirect",
    );
}

#[test]
fn a_release_does_not_leak_to_another_term() {
    let tmp = indexed_repo();
    let dir = tmp.path();
    set_mode(dir, "strict");
    assert_denies(
        &guard(dir, with_session(grep_for("charge_payment"), "s-terms")),
        "the first term",
    );
    assert_denies(
        &guard(dir, with_session(grep_for("refund_payment"), "s-terms")),
        "a different symbol is a different question",
    );
}

/// The valve's state is the guard's own bookkeeping and belongs under
/// `.travsr/`, which `init` already git-ignores.
#[test]
fn valve_state_lives_under_the_ignored_travsr_directory() {
    let tmp = indexed_repo();
    let dir = tmp.path();
    set_mode(dir, "strict");
    guard(dir, with_session(grep_for("charge_payment"), "s-state"));
    assert!(
        dir.join(".travsr/guard-sessions.json").is_file(),
        "the valve must persist somewhere the repo already ignores"
    );
}

// ── fail-open ───────────────────────────────────────────────────────────────

#[test]
fn a_missing_graph_database_allows() {
    let tmp = indexed_repo();
    let dir = tmp.path();
    set_mode(dir, "strict");
    std::fs::remove_file(dir.join(".travsr/graph.db")).unwrap();
    assert_allows(&guard(dir, grep_for("charge_payment")), "no graph.db");
    assert_allows(&guard(dir, read("src/pay.rs")), "no graph.db, on a read");
}

#[test]
fn a_graph_database_that_will_not_open_allows() {
    let tmp = indexed_repo();
    let dir = tmp.path();
    set_mode(dir, "strict");
    std::fs::write(dir.join(".travsr/graph.db"), b"not a sqlite file at all").unwrap();
    assert_allows(
        &guard(dir, grep_for("charge_payment")),
        "a corrupt graph.db",
    );
}

/// A database another process holds exclusively. SQLite's locking is what makes
/// this reachable: an `EXCLUSIVE` transaction blocks even a read-only opener,
/// so the guard's open path has to come back empty-handed rather than wait.
#[test]
fn a_locked_graph_database_allows() {
    let tmp = indexed_repo();
    let dir = tmp.path();
    set_mode(dir, "strict");
    let conn = rusqlite::Connection::open(dir.join(".travsr/graph.db")).unwrap();
    conn.execute_batch("PRAGMA locking_mode=EXCLUSIVE; BEGIN EXCLUSIVE;")
        .expect("taking an exclusive lock");
    let out = guard(dir, grep_for("charge_payment"));
    drop(conn);
    assert_allows(&out, "a locked graph.db");
}

#[test]
fn an_empty_index_allows() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    git(dir, &["-c", "init.defaultBranch=main", "init", "-q"]);
    git(dir, &["config", "user.email", "t@t.com"]);
    git(dir, &["config", "user.name", "T"]);
    git(dir, &["config", "core.autocrlf", "false"]);
    std::fs::write(dir.join("README.md"), "# nothing to index\n").unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-qm", "seed"]);
    travsr(dir)
        .args(["init", "--quiet", "--no-connect"])
        .assert()
        .success();
    set_mode(dir, "strict");
    assert_allows(&guard(dir, grep_for("charge_payment")), "an empty index");
}

/// A checkout that has moved on from the commit the index describes.
///
/// The marker is stamped directly rather than by making a commit. `travsr init`
/// installs a post-commit hook, so committing re-indexes and the tree never
/// goes stale, which is the whole point of that hook, and useless as a fixture
/// for the state it exists to prevent.
#[test]
fn a_stale_index_allows() {
    let tmp = indexed_repo();
    let dir = tmp.path();
    set_mode(dir, "strict");
    // The index is current here, so the deny is the baseline this contrasts with.
    assert_denies(
        &guard(
            dir,
            with_session(grep_for("charge_payment"), "s-stale-base"),
        ),
        "the fresh-index baseline",
    );

    let db = dir.join(".travsr/graph.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    let stamped = conn
        .execute(
            "UPDATE meta SET value = ?1 WHERE key = 'last_commit'",
            ["deadbee"],
        )
        .expect("stamping a commit the checkout is not at");
    assert_eq!(stamped, 1, "init must have recorded a last_commit to move");
    drop(conn);

    assert_allows(
        &guard(dir, with_session(grep_for("charge_payment"), "s-stale")),
        "an index that no longer describes HEAD",
    );
}

/// No git at all: staleness cannot be determined, which is its own fail-open
/// condition rather than a licence to assume the index is current.
#[test]
fn an_index_whose_head_cannot_be_resolved_allows() {
    let tmp = indexed_repo();
    let dir = tmp.path();
    set_mode(dir, "strict");
    std::fs::remove_file(dir.join(".git/HEAD")).unwrap();
    assert_allows(
        &guard(dir, grep_for("charge_payment")),
        "an unreadable HEAD",
    );
}

#[test]
fn a_payload_outside_any_repository_allows() {
    let outside = tempfile::tempdir().unwrap();
    let repo = indexed_repo();
    // `guard.mode` comes from the repo, but `cwd` points somewhere else.
    set_mode(repo.path(), "strict");
    let mut payload = grep_for("charge_payment");
    payload["cwd"] = json!(outside.path().to_string_lossy());
    assert_allows(&guard(repo.path(), payload), "a cwd outside a repository");
}

#[test]
fn the_environment_escape_hatch_allows_everything() {
    let tmp = indexed_repo();
    let dir = tmp.path();
    set_mode(dir, "strict");
    // Without the hatch this is a deny; the contrast is the point.
    assert_denies(
        &guard(
            dir,
            with_session(grep_for("charge_payment"), "s-hatch-base"),
        ),
        "the baseline before the hatch",
    );
    for payload in [grep_for("charge_payment"), read("src/pay.rs")] {
        let out = guard_env(dir, payload, &[("TRAVSR_GUARD", "off")]);
        assert_allows(&out, "TRAVSR_GUARD=off");
        assert_eq!(
            verdict(&out),
            None,
            "the escape hatch is 'get out of my way', not 'auto-approve things' \
            ; {out}"
        );
    }
}

#[test]
fn a_malformed_payload_allows() {
    let tmp = indexed_repo();
    let dir = tmp.path();
    set_mode(dir, "strict");
    for raw in [
        "",
        "   ",
        "not json at all",
        "{\"tool_name\": ",
        "[]",
        "null",
        "{\"tool_name\": 42}",
    ] {
        let out = travsr(dir).arg("guard").write_stdin(raw).output().unwrap();
        assert!(
            out.status.success(),
            "a malformed payload must still exit 0, got {:?}",
            out.status.code()
        );
        assert_allows(
            &String::from_utf8_lossy(&out.stdout),
            &format!("malformed payload {raw:?}"),
        );
    }
}

#[test]
fn a_payload_for_another_hook_event_allows() {
    let tmp = indexed_repo();
    let dir = tmp.path();
    set_mode(dir, "strict");
    let mut payload = grep_for("charge_payment");
    payload["hook_event_name"] = json!("PostToolUse");
    assert_allows(&guard(dir, payload), "a PostToolUse payload");
}

/// The guard reads the index directly, so there is no daemon to be unavailable.
/// Asserted rather than assumed: a future change that reached for the daemon
/// would make every decision depend on a process that is usually not running.
#[test]
fn no_daemon_is_required() {
    let tmp = indexed_repo();
    let dir = tmp.path();
    set_mode(dir, "strict");
    // `daemon.lock` is the daemon's liveness marker on every platform, unlike a
    // socket path, which does not exist on Windows at all and would make this
    // assertion trivially true there.
    assert!(
        !dir.join(".travsr/daemon.lock").exists(),
        "fixture must have no daemon"
    );
    assert_denies(
        &guard(dir, with_session(grep_for("charge_payment"), "s-nodaemon")),
        "the guard must answer with no daemon running",
    );
}

/// The budget the guard documents. Restated rather than imported: the constant
/// lives in the binary crate, which an integration test cannot reach.
const DEADLINE_MS: u64 = 200;

/// The timeout fail-open, end to end.
///
/// The decision is made unreachable by never closing stdin: the guard blocks
/// reading the payload, so nothing but the deadline can end the wait. That is
/// also a real shape, a host that writes the payload and holds the pipe.
///
/// A very small budget does not work here and is how this test first went
/// wrong. `TRAVSR_GUARD_DEADLINE_MS=1` races the decision rather than
/// forbidding it, and on a release build with a warm cache the decision wins:
/// CI came back with the `deny` the baseline below asserts, which is correct
/// behaviour and a useless test. Blocking the read removes the race.
#[test]
fn a_missed_deadline_allows() {
    use std::io::Write as _;
    use std::process::Stdio;

    let tmp = indexed_repo();
    let dir = tmp.path();
    set_mode(dir, "strict");

    // The same payload is a certain `deny` when the guard is given time, which
    // is what makes the allow below attributable to the deadline alone.
    assert_denies(
        &guard(dir, with_session(grep_for("charge_payment"), "s-deadline")),
        "the baseline, given time",
    );

    let payload = with_session(grep_for("charge_payment"), "s-deadline-2").to_string();
    let started = std::time::Instant::now();
    let mut child = StdCommand::new(assert_cmd::cargo::cargo_bin("travsr"))
        .arg("guard")
        .env("TRAVSR_DISABLE_REGISTRY", "1")
        .env("HOME", dir)
        .env("USERPROFILE", dir)
        .env("TRAVSR_GUARD_DEADLINE_MS", DEADLINE_MS.to_string())
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the guard must spawn");

    // Held open deliberately, and not dropped until the child has exited.
    let mut pipe = child.stdin.take().expect("stdin must be piped");
    pipe.write_all(payload.as_bytes())
        .expect("writing the payload");
    pipe.flush().expect("flushing the payload");

    let out = child
        .wait_with_output()
        .expect("the guard must exit on its own");
    drop(pipe);

    assert!(
        out.status.success(),
        "a guard that ran out of time must still exit 0; got {:?}",
        out.status.code()
    );
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_allows(&stdout, "a decision that could not be reached in time");
    assert_eq!(
        verdict(&stdout),
        None,
        "a guard that did not decide must say nothing, not auto-approve; {stdout}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(60),
        "the guard waited {:?} on a read that never finishes; the deadline is \
         the only thing that can end that wait",
        started.elapsed()
    );
}

/// What the deadline actually buys: a guard that does not sit on
/// `SqliteStore::open_read_only`'s five second busy timeout because another
/// process holds the database, turning SQLite's patience into the agent's
/// latency. Runs at the real budget, and states its assertion as a difference
/// from a warm baseline because process startup dominates a debug build.
#[test]
fn a_locked_database_does_not_stall_the_guard() {
    let tmp = indexed_repo();
    let dir = tmp.path();
    set_mode(dir, "strict");
    let real = &[("TRAVSR_GUARD_DEADLINE_MS", DEADLINE_MS.to_string())];
    let real: Vec<(&str, &str)> = real.iter().map(|(k, v)| (*k, v.as_str())).collect();

    let timed = |payload: Value| {
        let started = std::time::Instant::now();
        let out = guard_env(dir, payload, &real);
        (started.elapsed(), out)
    };

    // Warm the binary and the page cache, then take the baseline: everything
    // this test measures other than the lock.
    timed(grep_for("charge_payment"));
    let (baseline, _) = timed(with_session(grep_for("charge_payment"), "s-base"));

    let conn = rusqlite::Connection::open(dir.join(".travsr/graph.db")).unwrap();
    conn.execute_batch("PRAGMA locking_mode=EXCLUSIVE; BEGIN EXCLUSIVE;")
        .expect("taking an exclusive lock");
    let (locked, out) = timed(with_session(grep_for("charge_payment"), "s-locked"));
    drop(conn);

    assert_allows(&out, "a locked database");
    let added = locked.saturating_sub(baseline);
    assert!(
        added < std::time::Duration::from_secs(2),
        "the lock added {added:?} on top of a {baseline:?} baseline. The guard's \
         own work is bounded at {DEADLINE_MS}ms, so anything near SQLite's 5s \
         busy timeout means the deadline is not being enforced"
    );
}

// ── off ─────────────────────────────────────────────────────────────────────

#[test]
fn an_unconfigured_repository_decides_nothing() {
    let tmp = indexed_repo();
    let dir = tmp.path();
    // No `guard.mode` written at all: `init` was run without `--guard`.
    for payload in [
        grep_for("charge_payment"),
        read("src/pay.rs"),
        bash("rg charge_payment"),
    ] {
        let out = guard(dir, payload);
        assert_eq!(
            verdict(&out),
            None,
            "the guard is opt-in; an unconfigured repo must be untouched; {out}"
        );
    }
}

#[test]
fn a_mistyped_mode_is_read_as_off_rather_than_as_blocking() {
    let tmp = indexed_repo();
    let dir = tmp.path();
    // Past the CLI validator, by hand, the way a typo actually arrives.
    std::fs::write(
        dir.join(".travsr/config.toml"),
        "[guard]\nmode = \"strickt\"\n",
    )
    .unwrap();
    assert_allows(&guard(dir, grep_for("charge_payment")), "a mistyped mode");
}

// ── CLI surface ─────────────────────────────────────────────────────────────

#[test]
fn the_guard_flag_rejects_an_unknown_level() {
    let tmp = indexed_repo();
    let out = travsr(tmp.path())
        .args(["connect", "--guard=foo", "--print"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "an unknown level must be an error");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("foo") && stderr.contains("strict"),
        "the error must name what was rejected and what is accepted; {stderr}"
    );
}

#[test]
fn the_guard_flag_is_documented_on_both_commands() {
    for cmd in ["init", "connect"] {
        let tmp = tempfile::tempdir().unwrap();
        let out = travsr(tmp.path()).args([cmd, "--help"]).output().unwrap();
        let help = String::from_utf8_lossy(&out.stdout);
        assert!(
            help.contains("--guard"),
            "`travsr {cmd} --help` must document --guard; {help}"
        );
    }
    let tmp = tempfile::tempdir().unwrap();
    let out = travsr(tmp.path())
        .args(["guard", "--help"])
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        help.contains("stdin") && help.contains("stdout"),
        "`travsr guard --help` must say what it reads and writes; {help}"
    );
}

// ── installation into .claude/settings.json ─────────────────────────────────

fn settings(dir: &Path) -> Value {
    let text = std::fs::read_to_string(dir.join(".claude/settings.json"))
        .expect(".claude/settings.json must exist");
    serde_json::from_str(&text).expect(".claude/settings.json must stay strict JSON")
}

/// Every travsr `PreToolUse` handler currently in the file.
fn guard_handlers(root: &Value) -> Vec<Value> {
    root["hooks"]["PreToolUse"]
        .as_array()
        .map(|groups| {
            groups
                .iter()
                .filter_map(|g| g["hooks"].as_array())
                .flatten()
                .filter(|h| {
                    let cmd = h["command"].as_str().unwrap_or_default();
                    let args = h["args"].as_array().cloned().unwrap_or_default();
                    (cmd.ends_with("travsr") || cmd.ends_with("travsr.exe"))
                        && args.first().and_then(|a| a.as_str()) == Some("guard")
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// A repo Claude Code is detected in, with a `settings.json` the user owns.
fn claude_repo() -> tempfile::TempDir {
    let tmp = indexed_repo();
    let dir = tmp.path();
    std::fs::create_dir_all(dir.join(".claude")).unwrap();
    std::fs::write(
        dir.join(".claude/settings.json"),
        serde_json::to_string_pretty(&json!({
            "theme": "dark",
            "env": { "MY_VAR": "1" },
            "permissions": { "allow": ["Bash(npm test)"] },
            "hooks": {
                "PostToolUse": [
                    { "matcher": "Edit", "hooks": [
                        { "type": "command", "command": "my-formatter" }
                    ] }
                ],
                "PreToolUse": [
                    { "matcher": "Write", "hooks": [
                        { "type": "command", "command": "my-own-check.sh" }
                    ] }
                ]
            }
        }))
        .unwrap(),
    )
    .unwrap();
    tmp
}

fn connect(dir: &Path, args: &[&str]) -> String {
    let out = travsr(dir)
        .arg("connect")
        .args(["--tool", "claude-code"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "connect {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn guard_installs_one_pretooluse_entry_and_preserves_everything_else() {
    let tmp = claude_repo();
    let dir = tmp.path();
    connect(dir, &["--guard"]);

    let root = settings(dir);
    assert_eq!(guard_handlers(&root).len(), 1, "exactly one entry; {root}");

    // Unrelated keys.
    assert_eq!(root["theme"], "dark");
    assert_eq!(root["env"]["MY_VAR"], "1");
    assert_eq!(root["permissions"]["allow"][0], "Bash(npm test)");
    // Unrelated hook events.
    assert_eq!(root["hooks"]["PostToolUse"][0]["matcher"], "Edit");
    assert_eq!(
        root["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
        "my-formatter"
    );
    // And the user's own PreToolUse group, untouched.
    let theirs = root["hooks"]["PreToolUse"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["matcher"] == "Write")
        .expect("the user's own PreToolUse group must survive");
    assert_eq!(theirs["hooks"][0]["command"], "my-own-check.sh");
    assert_eq!(
        theirs["hooks"].as_array().unwrap().len(),
        1,
        "travsr must not have joined the user's group"
    );
}

#[test]
fn the_installed_hook_matches_the_tools_the_guard_inspects() {
    let tmp = claude_repo();
    let dir = tmp.path();
    connect(dir, &["--guard"]);
    let root = settings(dir);
    let group = root["hooks"]["PreToolUse"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| !guard_handlers(&json!({"hooks":{"PreToolUse":[g]}})).is_empty())
        .expect("our group must be there")
        .clone();
    let matcher = group["matcher"].as_str().unwrap();
    for tool in ["Grep", "Glob", "Read", "Bash"] {
        assert!(
            matcher.contains(tool),
            "the guard inspects {tool}, so the matcher has to fire on it; {matcher}"
        );
    }
    // And on the travsr tools, which is what feeds the release valve.
    assert!(
        matcher.contains("get_callers") && matcher.contains("find_references"),
        "the guard must see travsr queries go past or the strict valve cannot \
         open; {matcher}"
    );
    // Exec form, so a Windows path with a space in it needs no quoting.
    let handler = &group["hooks"][0];
    assert_eq!(handler["args"][0], "guard");
    assert_eq!(handler["type"], "command");
}

#[test]
fn repeated_init_and_connect_do_not_duplicate_the_hook() {
    let tmp = claude_repo();
    let dir = tmp.path();

    connect(dir, &["--guard"]);
    connect(dir, &["--guard"]);
    connect(dir, &["--guard=strict"]);
    travsr(dir)
        .args(["init", "--quiet", "--guard=strict"])
        .assert()
        .success();
    travsr(dir)
        .args(["init", "--quiet", "--guard=strict"])
        .assert()
        .success();

    let root = settings(dir);
    assert_eq!(
        guard_handlers(&root).len(),
        1,
        "five runs must leave exactly one entry; {root}"
    );
    assert_eq!(
        root["hooks"]["PreToolUse"].as_array().unwrap().len(),
        2,
        "the user's group plus ours, and no more; {root}"
    );
}

#[test]
fn a_second_identical_run_reports_no_change() {
    let tmp = claude_repo();
    let dir = tmp.path();
    connect(dir, &["--guard"]);
    let again = connect(dir, &["--guard"]);
    assert!(
        again.contains("ok .claude/settings.json"),
        "an unchanged file must be reported as unchanged, not rewritten; {again}"
    );
}

#[test]
fn the_level_is_persisted_and_drives_the_guard() {
    let tmp = claude_repo();
    let dir = tmp.path();

    connect(dir, &["--guard"]);
    let cfg = std::fs::read_to_string(dir.join(".travsr/config.toml")).unwrap();
    assert!(cfg.contains("advisory"), "--guard stores advisory; {cfg}");
    assert_no_decision(
        &guard(dir, with_session(grep_for("charge_payment"), "s-p1")),
        "advisory never blocks",
    );

    connect(dir, &["--guard=strict"]);
    let cfg = std::fs::read_to_string(dir.join(".travsr/config.toml")).unwrap();
    assert!(
        cfg.contains("strict"),
        "--guard=strict stores strict; {cfg}"
    );
    assert_denies(
        &guard(dir, with_session(grep_for("charge_payment"), "s-p2")),
        "strict, after the level changed",
    );
}

#[test]
fn a_plain_init_installs_no_hook_and_leaves_one_alone() {
    let tmp = claude_repo();
    let dir = tmp.path();

    // A bare init must not install enforcement in a repo nobody asked in.
    travsr(dir).args(["init", "--quiet"]).assert().success();
    assert!(
        guard_handlers(&settings(dir)).is_empty(),
        "a silent init must never start denying an agent's tool calls"
    );

    // And once installed, a later bare init must not quietly remove it.
    connect(dir, &["--guard=strict"]);
    travsr(dir).args(["init", "--quiet"]).assert().success();
    assert_eq!(
        guard_handlers(&settings(dir)).len(),
        1,
        "a bare re-init must leave an installed guard standing"
    );
    let cfg = std::fs::read_to_string(dir.join(".travsr/config.toml")).unwrap();
    assert!(
        cfg.contains("strict"),
        "and must not reset the level; {cfg}"
    );
}

#[test]
fn remove_strips_only_the_travsr_hook() {
    let tmp = claude_repo();
    let dir = tmp.path();
    connect(dir, &["--guard=strict"]);
    assert_eq!(guard_handlers(&settings(dir)).len(), 1);

    connect(dir, &["--remove"]);
    let root = settings(dir);
    assert!(
        guard_handlers(&root).is_empty(),
        "ours must be gone; {root}"
    );
    // Everything else, exactly as it was.
    assert_eq!(root["theme"], "dark");
    assert_eq!(root["env"]["MY_VAR"], "1");
    assert_eq!(root["permissions"]["allow"][0], "Bash(npm test)");
    assert_eq!(
        root["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
        "my-formatter"
    );
    let pre = root["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(pre.len(), 1, "only the user's group is left; {root}");
    assert_eq!(pre[0]["hooks"][0]["command"], "my-own-check.sh");
}

#[test]
fn remove_clears_the_stored_level_so_the_guard_goes_quiet() {
    let tmp = claude_repo();
    let dir = tmp.path();
    connect(dir, &["--guard=strict"]);
    connect(dir, &["--remove"]);

    let cfg = std::fs::read_to_string(dir.join(".travsr/config.toml")).unwrap_or_default();
    assert!(
        !cfg.contains("strict"),
        "a setting left behind after the hook is gone is one waiting to \
         surprise whoever reinstalls it; {cfg}"
    );
    assert_eq!(
        verdict(&guard(dir, grep_for("charge_payment"))),
        None,
        "and the guard must decide nothing once removed"
    );
}

#[test]
fn remove_is_idempotent_and_never_deletes_the_file() {
    let tmp = claude_repo();
    let dir = tmp.path();
    connect(dir, &["--guard"]);
    connect(dir, &["--remove"]);
    connect(dir, &["--remove"]);
    connect(dir, &["--remove"]);
    assert!(
        dir.join(".claude/settings.json").is_file(),
        "the file is the user's; removing our hook must not take it with us"
    );
    assert_eq!(settings(dir)["theme"], "dark");
}

/// Removing from a file that never had our hook must not touch it at all.
#[test]
fn remove_over_an_untouched_file_changes_nothing() {
    let tmp = claude_repo();
    let dir = tmp.path();
    let before = std::fs::read_to_string(dir.join(".claude/settings.json")).unwrap();
    connect(dir, &["--remove"]);
    let after = std::fs::read_to_string(dir.join(".claude/settings.json")).unwrap();
    assert_eq!(before, after, "nothing of ours was there to remove");
}

#[test]
fn the_hook_installs_into_a_repo_with_no_settings_file_yet() {
    let tmp = indexed_repo();
    let dir = tmp.path();
    // `.claude/` is the Claude Code detection marker; the settings file is not.
    std::fs::create_dir_all(dir.join(".claude")).unwrap();
    connect(dir, &["--guard"]);
    assert_eq!(guard_handlers(&settings(dir)).len(), 1);
}

/// Same rule as `merge_json_server`: a config that does not parse is the
/// user's, and travsr does not get to replace it with one that does.
#[test]
fn a_malformed_settings_file_is_skipped_not_clobbered() {
    let tmp = indexed_repo();
    let dir = tmp.path();
    std::fs::create_dir_all(dir.join(".claude")).unwrap();
    let broken = "{\n  \"theme\": \"dark\",   // a comment JSON does not allow\n}\n";
    std::fs::write(dir.join(".claude/settings.json"), broken).unwrap();

    let report = connect(dir, &["--guard"]);
    assert_eq!(
        std::fs::read_to_string(dir.join(".claude/settings.json")).unwrap(),
        broken,
        "the file must be left exactly as it was"
    );
    assert!(
        report.contains("skipped .claude/settings.json"),
        "and the skip must be reported, not silent; {report}"
    );
}

/// `.claude/settings.json` is shared and committed. Git-ignoring it would hide
/// the user's own configuration from their repo.
#[test]
fn the_settings_file_is_never_git_ignored() {
    let tmp = claude_repo();
    let dir = tmp.path();
    connect(dir, &["--guard"]);
    let ignored = std::fs::read_to_string(dir.join(".gitignore")).unwrap_or_default();
    assert!(
        !ignored.contains(".claude/settings.json"),
        "a shared, user-owned file must not be ignored; {ignored}"
    );
}

#[test]
fn print_writes_nothing() {
    let tmp = claude_repo();
    let dir = tmp.path();
    let before = std::fs::read_to_string(dir.join(".claude/settings.json")).unwrap();
    let report = connect(dir, &["--guard=strict", "--print"]);
    assert!(
        report.contains(".claude/settings.json"),
        "--print must say what it would do; {report}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(".claude/settings.json")).unwrap(),
        before,
        "--print must touch nothing"
    );
    let cfg = std::fs::read_to_string(dir.join(".travsr/config.toml")).unwrap_or_default();
    assert!(!cfg.contains("strict"), "not even the stored level; {cfg}");
}
