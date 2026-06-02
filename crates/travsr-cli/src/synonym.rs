//! `travsr synonym` — manage the per-repo dynamic synonym table (RFC-012 A2 F1).

use anyhow::{Context as _, Result};
use clap::Subcommand;
use travsr_store::SqliteStore;

use crate::repo;

#[derive(Debug, Subcommand)]
pub enum SynonymCommand {
    /// Add a synonym pair (term → alias). Rejected if the table has ≥200 rows.
    Add {
        /// The source term (e.g. "store").
        term: String,
        /// The alias to expand to (e.g. "warehouse").
        alias: String,
    },
    /// Remove a synonym pair. No-op if the pair does not exist.
    Remove {
        /// The source term.
        term: String,
        /// The alias to remove.
        alias: String,
    },
    /// List all active synonym pairs.
    List,
    /// Reset the synonym table to the built-in static defaults.
    Reset,
}

pub fn run(action: SynonymCommand) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let repo_root = repo::find_git_root(&cwd)?;
    let db_path = repo_root.join(".travsr/graph.db");
    anyhow::ensure!(
        db_path.exists(),
        "not initialized — run `travsr init` first"
    );
    let mut store = SqliteStore::open(&db_path).context("opening graph store")?;

    match action {
        SynonymCommand::Add { term, alias } => {
            store.synonym_add(&term, &alias).context("adding synonym")?;
            println!("added: {term} → {alias}");
        }
        SynonymCommand::Remove { term, alias } => {
            store
                .synonym_remove(&term, &alias)
                .context("removing synonym")?;
            println!("removed: {term} → {alias}");
        }
        SynonymCommand::List => {
            let pairs = store.synonym_list().context("listing synonyms")?;
            if pairs.is_empty() {
                println!("(no synonyms)");
            } else {
                for (term, alias) in &pairs {
                    println!("{term} → {alias}");
                }
            }
        }
        SynonymCommand::Reset => {
            store.synonym_reset().context("resetting synonyms")?;
            println!("reset to static defaults");
        }
    }
    Ok(())
}
