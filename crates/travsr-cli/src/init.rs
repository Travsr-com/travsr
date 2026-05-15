// Delegates to travsr_daemon::init_repo (DEBT-010 closed in Sprint 2).
use anyhow::Context as _;

use crate::repo::find_git_root;

pub fn run() -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let repo_root = find_git_root(&cwd)?;
    let stats = travsr_daemon::init_repo(&repo_root)?;
    let db_path = repo_root.join(".travsr/graph.db");
    println!(
        "indexed {} files, {} nodes, {} edges → {}",
        stats.files_indexed,
        stats.nodes_written,
        stats.edges_written,
        db_path.display()
    );

    // DEBT-013 closed: hint users whose repo has no commits yet so
    // `travsr status` showing last_commit: (none) is not confusing.
    let check = travsr_store::SqliteStore::open(&db_path)?;
    if check.get_meta("last_commit")?.is_none() {
        println!(
            "tip: run `git commit` to record a baseline — \
             `travsr status` will show freshness after your first commit"
        );
    }

    Ok(())
}
