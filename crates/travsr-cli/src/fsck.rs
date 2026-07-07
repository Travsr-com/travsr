use anyhow::Context as _;

use crate::repo::find_git_root;

pub fn run(fix: bool, json: bool, force: bool) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let repo_root = find_git_root(&cwd)?;

    let report = travsr_daemon::fsck_repo(&repo_root, fix, force)?;

    if json {
        let out = serde_json::to_string_pretty(&report)
            .context("serializing GcReport to JSON")?;
        println!("{out}");
        return Ok(());
    }

    println!("nodes: {}  edges: {}", report.node_count, report.edge_count);

    if report.ghost_paths.is_empty() {
        if fix {
            println!("graph is clean — no ghost nodes found");
        } else {
            println!("no ghost nodes detected (run with --fix to repair)");
        }
    } else {
        let label = if fix { "deleted" } else { "ghost" };
        println!("{} {} path(s):", label, report.ghost_paths.len());
        for p in &report.ghost_paths {
            println!("  {p}");
        }
    }

    if report.orphan_edges_swept > 0 {
        println!("swept {} orphan edge(s)", report.orphan_edges_swept);
    }

    if report.aborted {
        if let Some(reason) = &report.abort_reason {
            anyhow::bail!("fsck aborted: {reason}");
        }
        anyhow::bail!("fsck aborted");
    }

    Ok(())
}
