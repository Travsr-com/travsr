//! `travsr pattern` — graph-scoped textual search (#299).
//!
//! Thin wrapper over the MCP `find_pattern` tool: a `git grep` confined to a
//! bounded file set (whole repo, a path prefix, or `files-importing(<symbol>)`).

use anyhow::Context as _;

use crate::daemon_client;
use crate::repo::find_git_root;

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum OutputFormat {
    /// Human-readable text (default): `path:line:col: text` lines.
    Text,
    /// JSON envelope: `{ "pattern", "scope", "output" }` for scripting.
    Json,
}

pub fn run(
    pattern: &str,
    scope: Option<String>,
    fixed: bool,
    format: OutputFormat,
) -> anyhow::Result<()> {
    if pattern.trim().is_empty() {
        anyhow::bail!("pattern must not be empty — try: travsr pattern FlagSourceHash");
    }
    let cwd = std::env::current_dir().context("getting current directory")?;
    let repo_root = find_git_root(&cwd)?;
    let db_path = repo_root.join(".travsr/graph.db");
    if !db_path.exists() {
        anyhow::bail!("not initialized — run `travsr init`");
    }

    let store = daemon_client::open_read_store(&db_path)?;
    let output = travsr_mcp::find_pattern(&store, pattern, scope.as_deref(), fixed);

    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::json!({
                    "pattern": pattern,
                    "scope": scope,
                    "fixed": fixed,
                    "output": output,
                })
            );
        }
        OutputFormat::Text => {
            // #517 D4: `output` is always the `<travsr-data>…</travsr-data>`
            // envelope, so `output.trim().is_empty()` was unreachable — a
            // zero-match run produced the 26-byte empty-envelope string, never
            // an empty one. Check the inner body instead.
            if envelope_body(&output).trim().is_empty() {
                println!("no matches for '{pattern}'");
            } else {
                println!("{output}");
            }
        }
    }
    Ok(())
}

/// Extract the content between `<travsr-data>` and `</travsr-data>`, handling
/// both the empty form (`<travsr-data></travsr-data>`) and the populated form
/// (`<travsr-data>\n…\n</travsr-data>`) that `wrap_envelope` produces.
fn envelope_body(output: &str) -> &str {
    output
        .strip_prefix("<travsr-data>")
        .and_then(|s| s.strip_suffix("</travsr-data>"))
        .unwrap_or(output)
}
