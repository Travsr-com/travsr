// L6: the model-switch lifecycle end-to-end. A model switch is exactly what
// produces orphan vectors (#376 lifecycle plan L2), so the natural test for
// reclamation (`embed gc`, W3) is the thing that creates the garbage:
// index a repo, embed it under model A, switch/embed under model B, confirm
// retrieval works under B, then `embed gc --apply` and confirm A's rows and
// HNSW files are gone while B's are untouched.
//
// Downloads two real embedding backends and runs real reindex passes, so
// this is `#[ignore]`d by default — run explicitly via
// `cargo test -p travsr-cli --test embed_lifecycle_e2e -- --ignored`, which
// is what `.github/workflows/embed-lifecycle-e2e.yml` (manual dispatch +
// weekly cron) does. Do not wire this into the default `cargo test` sweep:
// a full double-embed on every PR is the exact cost W7 measures against.
//
// Deliberately reuses the real `~/.travsr/bin`/`~/.travsr/models` cache
// (no fake $HOME) so a local run doesn't re-download ~1 GB every time and a
// CI run benefits from the same cache across runs. The one thing that must
// not leak from that choice: `cmd_init` unconditionally writes the
// machine-global `~/.travsr/embed.toml` active backend alongside the repo
// config (embed.rs's `cmd_init`), so `embed init --backend B` here would
// otherwise silently flip the *host machine's* default model — which then
// shows up nowhere near this test, as a confusing "embed model_id mismatch"
// warning and degraded (FTS-only) retrieval on a completely unrelated repo
// with its own already-running daemon. `GlobalConfigGuard` saves and
// restores that one file around the test run.

use std::path::Path;
use std::process::Command as StdCommand;

use assert_cmd::Command;

/// Saves `~/.travsr/embed.toml` (the machine-global active-backend config)
/// on construction and restores its exact prior content (or removes it, if
/// it didn't exist) on drop — including on a mid-test panic. See the module
/// doc comment for why this file needs protecting at all.
struct GlobalConfigGuard {
    path: std::path::PathBuf,
    prior: Option<String>,
}
impl GlobalConfigGuard {
    fn capture() -> Self {
        let path = dirs::home_dir().unwrap().join(".travsr").join("embed.toml");
        let prior = std::fs::read_to_string(&path).ok();
        Self { path, prior }
    }
}
impl Drop for GlobalConfigGuard {
    fn drop(&mut self) {
        match &self.prior {
            Some(content) => {
                let _ = std::fs::write(&self.path, content);
            }
            None => {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

const MODEL_A: &str = "bge-small-en-v1.5"; // 384-dim, cheapest installed backend
const MODEL_B: &str = "bge-base-en-v1.5"; // next-cheapest — the point is the
                                          // lifecycle transition, not accuracy

fn git_init(dir: &Path) {
    StdCommand::new("git")
        .args(["-c", "init.defaultBranch=main", "init", "-q"])
        .current_dir(dir)
        .status()
        .expect("git init failed");
    StdCommand::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(dir)
        .status()
        .expect("git config email failed");
    StdCommand::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(dir)
        .status()
        .expect("git config name failed");
}

/// A handful of files with clearly distinct, unambiguous symbols — enough
/// that a semantic query for one has exactly one sane top-1 answer,
/// regardless of which embedding model is active.
fn write_fixture(dir: &Path) {
    std::fs::write(
        dir.join("billing.ts"),
        "export function calculateMonthlyInterestRate(principal: number, annualRate: number): number {\n\
         \x20 return principal * (annualRate / 12);\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("notifications.ts"),
        "export function sendWelcomeEmailToNewUser(email: string): void {\n\
         \x20 console.log(`welcome ${email}`);\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("config.ts"),
        "export function parseConfigurationFileFromDisk(path: string): object {\n\
         \x20 return JSON.parse(path);\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("auth.ts"),
        "export function validateUserPasswordAgainstPolicy(password: string): boolean {\n\
         \x20 return password.length >= 12;\n}\n",
    )
    .unwrap();
}

/// Minimal synchronous MCP client: spawns `travsr mcp` in the foreground,
/// completes the initialize handshake, calls `get_context`, and returns the
/// tool's text content. Foreground and fully owned by this function (killed
/// on return) — no detached process, no daemon lifecycle to manage.
fn mcp_get_context(repo: &Path, query: &str) -> String {
    use std::io::{BufRead as _, BufReader, Write as _};

    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("travsr"))
        .arg("mcp")
        .current_dir(repo)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn travsr mcp");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    let send = |stdin: &mut std::process::ChildStdin, msg: &serde_json::Value| {
        writeln!(stdin, "{msg}").unwrap();
    };
    let recv = |stdout: &mut BufReader<std::process::ChildStdout>| -> serde_json::Value {
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        serde_json::from_str(line.trim()).unwrap_or_else(|e| panic!("bad JSON line {line:?}: {e}"))
    };

    send(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05", "capabilities": {},
                "clientInfo": {"name": "embed-lifecycle-e2e", "version": "1"}
            }
        }),
    );
    recv(&mut stdout);
    send(
        &mut stdin,
        &serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    );
    send(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "get_context", "arguments": {"query": query}}
        }),
    );
    let resp = recv(&mut stdout);

    let _ = child.kill();
    let _ = child.wait();

    resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no text content in get_context response: {resp}"))
        .to_string()
}

fn embed_db_path(repo: &Path) -> std::path::PathBuf {
    repo.join(".travsr/embed.db")
}

fn model_row_counts(embed_db: &Path) -> std::collections::BTreeMap<String, i64> {
    let conn = rusqlite::Connection::open(embed_db).unwrap();
    let mut stmt = conn
        .prepare("SELECT model_id, COUNT(*) FROM node_embeddings GROUP BY model_id")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

#[test]
#[ignore = "downloads real models and runs real reindex passes; see module docs"]
fn model_switch_lifecycle_end_to_end() {
    let _global_config_guard = GlobalConfigGuard::capture();
    let repo = tempfile::tempdir().unwrap();
    git_init(repo.path());
    write_fixture(repo.path());

    Command::cargo_bin("travsr")
        .unwrap()
        .current_dir(repo.path())
        .arg("init")
        .assert()
        .success();

    // ── A: install + embed the first model ──────────────────────────────
    Command::cargo_bin("travsr")
        .unwrap()
        .current_dir(repo.path())
        .args(["embed", "init", "--backend", MODEL_A])
        .assert()
        .success();

    let counts_a = model_row_counts(&embed_db_path(repo.path()));
    assert_eq!(
        counts_a.get(MODEL_A).copied().unwrap_or(0),
        4,
        "all 4 fixture functions must be embedded under model A"
    );

    // ── B: install + embed the second model — the switch itself. A's rows
    // stay in embed.db (orphaned, not deleted) exactly as #481/L2 describe. ──
    Command::cargo_bin("travsr")
        .unwrap()
        .current_dir(repo.path())
        .args(["embed", "init", "--backend", MODEL_B])
        .assert()
        .success();

    let repo_config = std::fs::read_to_string(repo.path().join(".travsr/embed.toml")).unwrap();
    assert!(
        repo_config.contains(MODEL_B),
        "repo config must now name model B, got: {repo_config}"
    );

    let counts_b = model_row_counts(&embed_db_path(repo.path()));
    assert_eq!(
        counts_b.get(MODEL_B).copied().unwrap_or(0),
        4,
        "all 4 fixture functions must be embedded under model B"
    );
    assert_eq!(
        counts_b.get(MODEL_A).copied().unwrap_or(0),
        4,
        "model A's rows must survive the switch, orphaned not deleted"
    );
    assert!(
        repo.path()
            .join(format!(".travsr/{MODEL_B}.hnsw.usearch"))
            .exists(),
        "model B's HNSW index must exist after its embed pass"
    );

    // ── Retrieval under B: an unambiguous query must resolve to the right
    // function. Not asserting hit@1 parity with A — different models have
    // different score distributions; this asserts the pipeline works under
    // B at all, on a query with exactly one sane answer.
    //
    // Semantic (embed-backed) retrieval requires the embed KNN hook, which is
    // only wired up by a live daemon or an `mcp` server process — `ask`'s
    // non-daemon read-only fallback is lexical-only. Rather than a detached
    // background `daemon start` (unreliable to tie down in a short-lived test:
    // nothing here needs it to survive past this function, and a foreground
    // child avoids that entirely), spawn `travsr mcp` in the foreground and
    // talk to it directly, the same way `bench/run-phase2-gate.mjs` does. ──
    let context = mcp_get_context(repo.path(), "compute the monthly interest rate on a loan");
    assert!(
        context.contains("billing.ts"),
        "get_context for an unambiguous NL query must surface the interest-rate \
         function under model B, got: {context}"
    );

    // ── gc: reclaim model A now that B is proven working ──────────────────
    Command::cargo_bin("travsr")
        .unwrap()
        .current_dir(repo.path())
        .args(["embed", "gc", "--apply"])
        .assert()
        .success();

    let counts_after_gc = model_row_counts(&embed_db_path(repo.path()));
    assert_eq!(
        counts_after_gc.get(MODEL_A),
        None,
        "model A's rows must be gone after gc --apply"
    );
    assert_eq!(
        counts_after_gc.get(MODEL_B).copied().unwrap_or(0),
        4,
        "model B's rows must survive gc (it is the active model)"
    );
    assert!(
        !repo
            .path()
            .join(format!(".travsr/{MODEL_A}.hnsw.usearch"))
            .exists(),
        "model A's HNSW index must be removed by gc"
    );
    assert!(
        repo.path()
            .join(format!(".travsr/{MODEL_B}.hnsw.usearch"))
            .exists(),
        "model B's HNSW index must survive gc"
    );
}
