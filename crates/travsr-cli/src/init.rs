// Sprint 2: delegates to travsr_daemon::init_repo (DEBT-010 closed by SWE in S2-1).
use anyhow::Context as _;

use crate::repo::find_git_root;

pub fn run() -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let repo_root = find_git_root(&cwd)?;
    let stats = travsr_daemon::init_repo(&repo_root)?;
    println!(
        "indexed {} files, {} nodes, {} edges → {}",
        stats.files_indexed,
        stats.nodes_written,
        stats.edges_written,
        repo_root.join(".travsr/graph.db").display()
    );
    Ok(())
}
