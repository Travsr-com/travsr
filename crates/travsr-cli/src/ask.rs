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

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum OutputFormat {
    /// Human-readable table (default)
    Table,
    /// Machine-readable JSON — emits the full AskPayload
    Json,
}

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

pub fn run(query_str: &str, format: OutputFormat) -> anyhow::Result<()> {
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
            let mut store = daemon_client::open_read_store(&db_path)?;
            // Best-effort: load HNSW embed hook for cold-path KNN. Falls back to
            // FTS-only if the sidecar binary is absent or the index is not built.
            travsr_daemon::try_inject_embed_hook_readonly(&mut store, &db_path);
            let knn = store.embed_knn_fn();
            let knn_ref = knn
                .as_ref()
                .map(|f| f as &dyn Fn(&str, u32) -> Vec<(travsr_core::NodeId, f32)>);
            query::ask_query(&store, query_str, knn_ref)?
        }
    };

    if matches!(format, OutputFormat::Json) {
        println!("{}", serde_json::to_string(&payload)?);
        return Ok(());
    }

    let display_query = query_str.strip_prefix(':').unwrap_or(query_str).trim();
    if !payload.matched {
        // `ask` is natural-language, graph-grounded retrieval — not a symbol-name
        // lookup — so an abstention means "no confidently relevant code found",
        // not "that symbol does not exist". Word it that way to avoid the misread.
        println!("no grounded match for '{display_query}' in this repo (try rephrasing, or search a symbol name directly)");
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
            path: match r.line {
                Some(l) => format!("{}:{}", r.path, l),
                None => r.path,
            },
            score: format!("{:.3}", r.score),
        })
        .collect();

    let n = rows.len();
    println!("{}", Table::new(rows));
    let embed_note = if payload.embed_used {
        " · [embed-enhanced]"
    } else {
        ""
    };
    // F9: surface the honest confidence label (parity with get_context's header).
    let confidence_note = if payload.confidence.is_empty() {
        String::new()
    } else {
        format!(" · confidence: {}", payload.confidence)
    };
    println!(
        "\n{n} nodes · ~{} tokens{confidence_note}{embed_note}",
        payload.total_tokens
    );
    Ok(())
}
