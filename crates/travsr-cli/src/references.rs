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
    /// Structured JSON (`symbol`, `resolved_to`, `references[]`, …) for scripting.
    Json,
}

/// #661 WS-D: the caller's live short HEAD, read at `cwd` (before the worktree
/// redirect in `find_git_root`, so a linked worktree reports its own commit).
/// `None` when git is unavailable or the dir is not a repo — the mismatch note
/// then correctly never fires. Mirrors `status::head_at`.
fn head_at(cwd: &std::path::Path) -> Option<String> {
    // Bounded: an unbounded `output()` here can never return on Windows when a
    // git child or grandchild inherits the pipe (#717 triage, same mechanism as
    // #503 / #572). A HEAD that does not arrive is the same as no HEAD, which
    // this function already handles.
    // `cwd` goes through as a real path rather than `-C <string>`: a path with
    // bytes that are not valid UTF-8 is legal, and converting it to a string
    // first would mangle it into U+FFFD and lose a repo that exists.
    crate::git_bounded::git_stdout_bounded(Some(cwd), ["rev-parse", "--short", "HEAD"])
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
    //
    // Run concurrently with `find_git_root` below: both are independent,
    // bounded git queries on `cwd` (the latter only shells out in the
    // linked-worktree branch, via `main_worktree_root`). Sequentially, a wedged
    // git would let this command stall for up to 2x `GIT_QUERY_TIMEOUT` instead
    // of 1x. Mirrors `status::run`.
    let head_handle = {
        let cwd = cwd.clone();
        std::thread::spawn(move || head_at(&cwd))
    };
    let repo_root = find_git_root(&cwd)?;
    let head = head_handle.join().ok().flatten();
    let db_path = repo_root.join(".travsr/graph.db");
    if !db_path.exists() {
        anyhow::bail!("not initialized — run `travsr init`");
    }

    daemon_client::warn_if_call_graph_degraded(&db_path);
    let store = daemon_client::open_read_store(&db_path)?;

    match format {
        OutputFormat::Json => {
            // Structured result: `symbol`, `resolved_to`, and a real `references`
            // array — not the human-readable envelope body wrapped in a string.
            let structured =
                travsr_mcp::find_references_structured(&store, symbol, path.as_deref());
            println!("{}", serde_json::to_string(&structured)?);
        }
        OutputFormat::Text => {
            // `find_references` returns the model-facing `<travsr-data>` envelope.
            // That is noise in CLI output — `ask` and `graph` already strip it — so
            // present the inner body to the user.
            let output = travsr_mcp::find_references(&store, symbol, path.as_deref());
            let body = envelope_body(&output);
            if body.trim().is_empty() {
                // An empty body means the name resolved to no definition at all —
                // distinct from a resolved symbol with zero recorded uses, which
                // prints its own `resolved: … 0 reference(s)` line.
                println!("Symbol '{symbol}' was not found.");
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
