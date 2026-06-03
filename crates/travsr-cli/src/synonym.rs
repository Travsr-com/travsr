//! `travsr synonym` — manage the per-repo dynamic synonym table (RFC-012 A2 F1).

use anyhow::{Context as _, Result};
use clap::Subcommand;
use travsr_store::SqliteStore;

use crate::repo;

#[derive(Debug, Subcommand)]
pub enum SynonymCommand {
    /// Add one or more aliases for a term. Rejected if the table would exceed 200 rows.
    ///
    /// Example: travsr synonym add payment billing invoice transaction
    Add {
        /// The source term (e.g. "payment").
        term: String,
        /// One or more aliases to expand to.
        #[arg(required = true)]
        aliases: Vec<String>,
    },
    /// Declare exactly the set of aliases for a term (replaces any existing aliases).
    ///
    /// Example: travsr synonym set payment billing invoice transaction charge
    Set {
        /// The source term.
        term: String,
        /// The complete replacement alias list.
        #[arg(required = true)]
        aliases: Vec<String>,
    },
    /// Remove one alias, or all aliases for a term.
    ///
    /// Examples:
    ///   travsr synonym remove payment invoice   # remove one alias
    ///   travsr synonym remove payment           # remove all aliases for "payment"
    Remove {
        /// The source term.
        term: String,
        /// The specific alias to remove. Omit to remove all aliases for the term.
        alias: Option<String>,
    },
    /// List active synonym pairs, optionally filtered to one term.
    ///
    /// Examples:
    ///   travsr synonym list             # all pairs
    ///   travsr synonym list payment     # only payment's aliases
    List {
        /// Show only aliases for this term. Omit to show all pairs.
        term: Option<String>,
    },
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
        SynonymCommand::Add { term, aliases } => {
            for alias in &aliases {
                store
                    .synonym_add(&term, alias)
                    .with_context(|| format!("adding synonym {term} → {alias}"))?;
            }
            println!("added: {} → {}", term, aliases.join(", "));
        }
        SynonymCommand::Set { term, aliases } => {
            store
                .synonym_remove_term(&term)
                .with_context(|| format!("clearing existing aliases for {term}"))?;
            for alias in &aliases {
                store
                    .synonym_add(&term, alias)
                    .with_context(|| format!("setting synonym {term} → {alias}"))?;
            }
            println!("set: {} → {}", term, aliases.join(", "));
        }
        SynonymCommand::Remove {
            term,
            alias: Some(alias),
        } => {
            store
                .synonym_remove(&term, &alias)
                .context("removing synonym")?;
            println!("removed: {term} → {alias}");
        }
        SynonymCommand::Remove { term, alias: None } => {
            store
                .synonym_remove_term(&term)
                .context("removing all aliases for term")?;
            println!("removed all aliases for: {term}");
        }
        SynonymCommand::List { term: Some(term) } => {
            let pairs = store.synonym_list().context("listing synonyms")?;
            let filtered: Vec<_> = pairs.iter().filter(|(t, _)| t == &term).collect();
            if filtered.is_empty() {
                println!("(no synonyms for {term})");
            } else {
                for (t, alias) in &filtered {
                    println!("{t} → {alias}");
                }
            }
        }
        SynonymCommand::List { term: None } => {
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
