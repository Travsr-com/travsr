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

pub fn run(symbol: &str, path: Option<String>, format: OutputFormat) -> anyhow::Result<()> {
    if symbol.trim().is_empty() {
        anyhow::bail!("symbol must not be empty — try: travsr refs PaymentService");
    }
    let cwd = std::env::current_dir().context("getting current directory")?;
    let repo_root = find_git_root(&cwd)?;
    let db_path = repo_root.join(".travsr/graph.db");
    if !db_path.exists() {
        anyhow::bail!("not initialized — run `travsr init`");
    }

    let store = daemon_client::open_read_store(&db_path)?;
    let output = travsr_mcp::find_references(&store, symbol, path.as_deref());

    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::json!({
                    "symbol": symbol,
                    "path": path,
                    "output": output,
                })
            );
        }
        OutputFormat::Text => {
            if output.trim().is_empty() {
                println!("no references found for '{symbol}'");
            } else {
                println!("{output}");
            }
        }
    }
    Ok(())
}
