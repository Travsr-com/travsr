//! `travsr ask` — PPR-ranked, knapsack-budgeted symbol context.
//!
//! Data acquisition is shared with the daemon via `travsr_mcp::query`
//! (#318 O1): a running daemon answers from its warm store; otherwise the
//! store is opened directly (read-only fast path).

use anyhow::Context as _;
use tabled::{Table, Tabled};
use travsr_mcp::query::{self, AskPayload};

use crate::daemon_client;
use crate::repo::find_git_root;

#[derive(Tabled)]
struct Row {
    #[tabled(rename = "Kind")]
    kind: String,
    #[tabled(rename = "Signature")]
    signature: String,
    #[tabled(rename = "Path")]
    path: String,
    #[tabled(rename = "Score")]
    score: String,
}

pub fn run(query_str: &str) -> anyhow::Result<()> {
    if query_str.trim().is_empty() {
        anyhow::bail!("search query must not be empty — try: travsr ask \"PaymentService\"");
    }
    let cwd = std::env::current_dir().context("getting current directory")?;
    let repo_root = find_git_root(&cwd)?;
    let db_path = repo_root.join(".travsr/graph.db");

    if !db_path.exists() {
        anyhow::bail!("not initialized — run `travsr init`");
    }

    let payload: AskPayload = match daemon_client::try_query(
        &repo_root,
        "ask",
        serde_json::json!({ "query": query_str }),
    ) {
        Some(p) => p,
        None => {
            let store = daemon_client::open_read_store(&db_path)?;
            query::ask_query(&store, query_str)?
        }
    };

    // Match the pre-#318 messages exactly (query echoed after `:`-stripping).
    let display_query = query_str.strip_prefix(':').unwrap_or(query_str).trim();
    if !payload.matched {
        println!("no symbols matching '{display_query}'");
        return Ok(());
    }
    if payload.no_results {
        println!("no graph results for '{display_query}'");
        return Ok(());
    }

    let rows: Vec<Row> = payload
        .rows
        .into_iter()
        .map(|r| Row {
            kind: r.kind,
            signature: r.signature,
            path: r.path,
            score: format!("{:.3}", r.score),
        })
        .collect();

    let n = rows.len();
    println!("{}", Table::new(rows));
    println!("\n{n} nodes · ~{} tokens", payload.total_tokens);
    Ok(())
}
