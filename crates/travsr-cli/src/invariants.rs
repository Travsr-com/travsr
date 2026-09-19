//! `travsr invariants` — check declared architectural rules against the graph.
//!
//! The rules live in `architecture-invariants.json` at the repository root, in
//! the repository, reviewed like code. The point is that they are checked on
//! every commit rather than restated in prose that drifts: this project's own
//! CLAUDE.md described a dependency graph that had been missing two real edges.
//!
//! Exits non-zero when a rule is violated, so it works as a CI gate.

use anyhow::{Context, Result};

/// Where the rules live, relative to the repository root.
pub const RULES_FILE: &str = "architecture-invariants.json";

pub fn run(provenance: &str) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let repo_root = crate::repo::find_git_root(&cwd)?;
    let db_path = repo_root.join(".travsr/graph.db");

    let rules_path = repo_root.join(RULES_FILE);
    if !rules_path.exists() {
        // Nothing declared is not a failure: a repository that has written down
        // no rules has none to break.
        println!("no {RULES_FILE} at the repository root, so there is nothing to check.");
        return Ok(());
    }

    let rules = std::fs::read_to_string(&rules_path)
        .with_context(|| format!("reading {}", rules_path.display()))?;
    let store = crate::daemon_client::open_read_store(&db_path)?;

    let report = travsr_mcp::check_architecture_invariants(&store, &rules, provenance);
    println!("{}", report.trim());

    if report.contains("VIOLATIONS FOUND") {
        anyhow::bail!("architecture invariants violated");
    }
    Ok(())
}
