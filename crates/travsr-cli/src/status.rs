//! `travsr status` — index and graph health summary.
//!
//! Data acquisition is shared with the daemon via `travsr_mcp::query`
//! (#318 O1): a running daemon answers from its warm store; otherwise the
//! store is opened directly (read-only fast path).

use anyhow::Context as _;
use travsr_mcp::query::{self, StatusPayload};

use crate::daemon_client;
use crate::repo::find_git_root;

pub fn run() -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let repo_root = find_git_root(&cwd)?;

    let db_path = repo_root.join(".travsr").join("graph.db");

    if !db_path.exists() {
        anyhow::bail!("not initialized — run `travsr init`");
    }

    let payload: StatusPayload =
        match daemon_client::try_query(&repo_root, "status", serde_json::json!({})) {
            Some(p) => p,
            None => {
                let store = daemon_client::open_read_store(&db_path)
                    .with_context(|| format!("opening graph database at {}", db_path.display()))?;
                query::status_query(&store)?
            }
        };

    let last_commit = payload.last_commit.unwrap_or_else(|| "(none)".to_string());
    println!(
        "nodes: {} | edges: {} | schema: v{} | journal: {} | last_commit: {last_commit}",
        payload.nodes, payload.edges, payload.schema, payload.journal
    );

    // RFC-014 #317 re-index policy: surface signature-format skew so the user
    // knows the graph was built with an older format and a re-index is due.
    let sig_v = payload.signature_format_version;
    if sig_v != travsr_core::SIGNATURE_FORMAT_VERSION {
        eprintln!(
            "⚠ signature format v{sig_v} ≠ current v{} — graph built with an older format; run `travsr init` to re-index",
            travsr_core::SIGNATURE_FORMAT_VERSION
        );
    }

    Ok(())
}
