//! `travsr refs` — enumerate every use site (`path:line`) of a symbol (#299).
//!
//! Thin wrapper over the same occurrence-store read the MCP `find_references`
//! tool performs. Opens the repo's store read-only and prints the result.

use anyhow::Context as _;

use crate::daemon_client;
use crate::repo::find_git_root;

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum OutputFormat {
    /// Human-readable text (default): a resolved header + `path:line` lines.
    Text,
    /// JSON envelope: `{ "symbol", "path", "output" }` for scripting.
    Json,
}

/// #661 WS-D: the caller's live short HEAD, read at `cwd` (before the worktree
/// redirect in `find_git_root`, so a linked worktree reports its own commit).
/// `None` when git is unavailable or the dir is not a repo — the mismatch note
/// then correctly never fires. Mirrors `status::head_at`.
fn head_at(cwd: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["-C", &cwd.to_string_lossy(), "rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let head = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!head.is_empty()).then_some(head)
}

/// Extract the content between `<travsr-data>` and `</travsr-data>`. Mirrors
/// `pattern::envelope_body`; falls back to the whole string if unwrapped.
fn envelope_body(output: &str) -> &str {
    output
        .strip_prefix("<travsr-data>")
        .and_then(|s| s.strip_suffix("</travsr-data>"))
        .map(|s| s.trim_matches('\n'))
        .unwrap_or(output)
}

pub fn run(symbol: &str, path: Option<String>, format: OutputFormat) -> anyhow::Result<()> {
    if symbol.trim().is_empty() {
        anyhow::bail!("symbol must not be empty — try: travsr refs PaymentService");
    }
    let cwd = std::env::current_dir().context("getting current directory")?;
    // #661 WS-D: read HEAD at cwd before the worktree redirect so a drifted
    // checkout is compared against the served index below.
    let head = head_at(&cwd);
    let repo_root = find_git_root(&cwd)?;
    let db_path = repo_root.join(".travsr/graph.db");
    if !db_path.exists() {
        anyhow::bail!("not initialized — run `travsr init`");
    }

    daemon_client::warn_if_call_graph_degraded(&db_path);
    let store = daemon_client::open_read_store(&db_path)?;
    let output = travsr_mcp::find_references(&store, symbol, path.as_deref());
    // U4: `find_references` returns the model-facing `<travsr-data>` envelope.
    // That is noise in CLI output — `ask` and `graph` already strip it — so
    // present the inner body to the user in both text and JSON here.
    let body = envelope_body(&output);

    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::json!({
                    "symbol": symbol,
                    "path": path,
                    "output": body,
                })
            );
        }
        OutputFormat::Text => {
            if body.trim().is_empty() {
                println!("no references found for '{symbol}'");
            } else {
                println!("{body}");
            }
        }
    }

    // #661 WS-D: warn (on stderr, keeping stdout/JSON clean) when the served
    // index describes a different commit than the caller's checkout, so a
    // confident `path:line` list on a drifted worktree is never taken at face
    // value. cwd-local classifier, identical wording to `travsr status` and the
    // MCP tools (`travsr_mcp::head_index_mismatch_note`).
    if let Some(head) = head.as_deref() {
        let stored = store
            .get_meta("last_commit")
            .ok()
            .flatten()
            .unwrap_or_default();
        if let Some(note) = travsr_mcp::head_index_mismatch_note(head, &stored) {
            eprintln!("{note}");
        }
    }
    Ok(())
}
