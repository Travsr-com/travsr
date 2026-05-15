use anyhow::Context as _;
use travsr_store::SqliteStore;

use crate::repo::find_git_root;

pub fn run() -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let repo_root = find_git_root(&cwd)?;

    let db_path = repo_root.join(".travsr").join("graph.db");

    if !db_path.exists() {
        anyhow::bail!("not initialized — run `travsr init`");
    }

    let store = SqliteStore::open(&db_path)
        .with_context(|| format!("opening graph database at {}", db_path.display()))?;

    let nodes = store.node_count()?;
    let edges = store.edge_count()?;
    let journal = store.journal_mode()?;

    println!("nodes: {nodes} | edges: {edges} | schema: v1 | journal: {journal}");

    Ok(())
}
