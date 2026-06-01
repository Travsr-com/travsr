// Delegates to travsr_daemon::init_repo (DEBT-010 closed in Sprint 2).
use anyhow::Context as _;

use crate::repo::find_git_root;

pub fn run() -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let repo_root = find_git_root(&cwd)?;
    let stats = travsr_daemon::init_repo(&repo_root)?;
    let db_path = repo_root.join(".travsr/graph.db");
    if stats.nodes_written == 0 && stats.edges_written == 0 {
        println!(
            "graph up to date: {} nodes, {} edges → {}",
            stats.total_nodes,
            stats.total_edges,
            db_path.display()
        );
    } else {
        println!(
            "indexed {} files, +{} nodes, +{} edges → {}",
            stats.files_indexed,
            stats.nodes_written,
            stats.edges_written,
            db_path.display()
        );
    }

    // DEBT-013 closed: hint users whose repo has no commits yet so
    // `travsr status` showing last_commit: (none) is not confusing.
    let check = travsr_store::SqliteStore::open(&db_path)?;
    if check.get_meta("last_commit")?.is_none() {
        println!(
            "tip: run `git commit` to record a baseline — \
             `travsr status` will show freshness after your first commit"
        );
    }

    // Non-fatal: detection errors must not fail `travsr init`.
    let _ = hint_lang_detect(&repo_root);

    Ok(())
}

/// After indexing, scan for supported languages and suggest `travsr lang detect`
/// if any are present but not yet registered.
fn hint_lang_detect(repo_root: &std::path::Path) -> anyhow::Result<()> {
    let detected = crate::lang::detect_languages_in(repo_root);
    if detected.is_empty() {
        return Ok(());
    }

    let config = crate::lang::load_lang_config();
    let unregistered: Vec<_> = detected
        .iter()
        .filter(|l| config.as_ref().map(|c| !c.is_registered(l)).unwrap_or(true))
        .cloned()
        .collect();

    if !unregistered.is_empty() {
        println!(
            "tip: detected {} in this repo — run `travsr lang detect` to set up semantic indexing",
            unregistered.join(", ")
        );
    }

    Ok(())
}
