// BFS-based symbol lookup. PPR ranking arrives in Phase 2.
use anyhow::Context as _;
use tabled::{Table, Tabled};
use travsr_store::SqliteStore;

use crate::repo::find_git_root;

#[derive(Tabled)]
struct Row {
    #[tabled(rename = "Kind")]
    kind: String,
    #[tabled(rename = "Signature")]
    signature: String,
    #[tabled(rename = "Path")]
    path: String,
}

pub fn run(query: &str) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let repo_root = find_git_root(&cwd)?;
    let db_path = repo_root.join(".travsr/graph.db");

    if !db_path.exists() {
        anyhow::bail!("not initialized — run `travsr init`");
    }

    let store = SqliteStore::open(&db_path)?;
    let matches = store.search_nodes_by_name(query)?;

    if matches.is_empty() {
        println!("no symbols matching '{query}'");
        return Ok(());
    }

    let seed = matches[0].id;
    let results = travsr_retrieval::bfs(&store, seed, 3, 4096)?;

    if results.is_empty() {
        println!("no graph results for '{query}'");
        return Ok(());
    }

    let rows: Vec<Row> = results
        .into_iter()
        .map(|n| Row {
            kind: n.kind,
            signature: n.vname.signature,
            path: n.vname.path,
        })
        .collect();

    println!("{}", Table::new(rows));
    Ok(())
}
