use anyhow::Context as _;
use travsr_store::{SqliteStore, StoreMigratable};

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
    let schema = store.schema_version()?;
    let journal = store.journal_mode()?;
    let last_commit = store
        .get_meta("last_commit")?
        .unwrap_or_else(|| "(none)".to_string());

    println!(
        "nodes: {nodes} | edges: {edges} | schema: v{schema} | journal: {journal} | last_commit: {last_commit}"
    );

    // RFC-014 #317 re-index policy: surface signature-format skew so the user
    // knows the graph was built with an older format and a re-index is due.
    let sig_v = store.get_signature_format_version()?;
    if sig_v != travsr_core::SIGNATURE_FORMAT_VERSION {
        println!(
            "⚠ signature format v{sig_v} ≠ current v{} — graph built with an older format; run `travsr init` to re-index",
            travsr_core::SIGNATURE_FORMAT_VERSION
        );
    }

    Ok(())
}
