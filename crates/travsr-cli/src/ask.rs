use anyhow::Context as _;
use tabled::{Table, Tabled};
use travsr_retrieval::{context_candidates, knapsack, token_cost};
use travsr_store::{SqliteStore, Store};

use crate::repo::find_git_root;

/// Token budget for `travsr ask`. Matches the MCP default.
const ASK_TOKEN_BUDGET: usize = 4096;

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

pub fn run(query: &str) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let repo_root = find_git_root(&cwd)?;
    let db_path = repo_root.join(".travsr/graph.db");

    if !db_path.exists() {
        anyhow::bail!("not initialized — run `travsr init`");
    }

    // Strip a leading `:` so VS Code graph-panel queries (which prefix with `:`
    // to bypass the colon-delimited command syntax) pass through cleanly.
    let query = query.strip_prefix(':').unwrap_or(query).trim();

    let store = SqliteStore::open(&db_path)?;
    let matches = store.search_nodes_fuzzy(query)?;

    if matches.is_empty() {
        println!("no symbols matching '{query}'");
        return Ok(());
    }

    // Prefer structural seeds over file nodes for richer PPR traversal.
    let preferred = ["class", "function", "method", "file"];
    let seed_node = preferred
        .iter()
        .find_map(|k| matches.iter().find(|n| n.kind == *k))
        .unwrap_or(&matches[0]);
    let seed = seed_node.id;

    // PPR from seed → ranked (NodeId, score) pairs.
    let ppr_scores = travsr_retrieval::ppr(&store, &[seed], context_candidates())
        .context("PPR traversal failed")?;

    if ppr_scores.is_empty() {
        println!("no graph results for '{query}'");
        return Ok(());
    }

    // Batch-fetch nodes and join with scores.
    let node_ids: Vec<_> = ppr_scores.iter().map(|(id, _)| *id).collect();
    let score_map: std::collections::HashMap<_, f32> = ppr_scores.into_iter().collect();
    let nodes = store.get_nodes(&node_ids).context("fetching nodes")?;
    let items: Vec<_> = nodes
        .into_iter()
        .filter_map(|n| score_map.get(&n.id).map(|&s| (n, s)))
        .collect();

    // Knapsack selection within token budget.
    let selected = knapsack(items, ASK_TOKEN_BUDGET);
    let total_tokens: usize = selected.iter().map(token_cost).sum();

    let rows: Vec<Row> = selected
        .into_iter()
        .map(|n| {
            let score = score_map.get(&n.id).copied().unwrap_or(0.0);
            Row {
                kind: n.kind,
                signature: n.vname.signature,
                path: n.vname.path,
                score: format!("{score:.3}"),
            }
        })
        .collect();

    let n = rows.len();
    println!("{}", Table::new(rows));
    println!("\n{n} nodes · ~{total_tokens} tokens");
    Ok(())
}
