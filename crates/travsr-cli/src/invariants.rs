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
    // An unknown filter matches no edge, and no edges means every dependency
    // rule "holds": `travsr invariants --provenance ratifed` turned a gate that
    // was failing on two real violations into exit 0. The same reasoning the
    // rules file states for a renamed component applies to a mistyped filter —
    // a guard must not be switchable off by accident — so the typo is rejected
    // rather than answered.
    if !travsr_mcp::PROVENANCE_FILTERS.contains(&provenance) {
        anyhow::bail!(
            "unknown --provenance '{provenance}'. Use one of: {}. \
             An unrecognised filter matches no edges, which would report every \
             rule as holding over an empty graph.",
            travsr_mcp::PROVENANCE_FILTERS
                .iter()
                .map(|p| if p.is_empty() { "'' (all)" } else { p })
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

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

    let report = travsr_mcp::check_architecture_invariants(&store, &rules, provenance)
        .map_err(|e| anyhow::anyhow!("{}: {e}", rules_path.display()))?;
    println!("{}", report.text.trim());

    if report.violations > 0 {
        anyhow::bail!("architecture invariants violated");
    }
    Ok(())
}
