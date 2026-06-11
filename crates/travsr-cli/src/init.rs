// Delegates to travsr_daemon::init_repo (DEBT-010 closed in Sprint 2).
use anyhow::Context as _;

use crate::repo::find_git_root;

pub fn run(quiet: bool, json: bool, jobs: Option<usize>) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let repo_root = find_git_root(&cwd)?;

    // Live progress so a long indexing run is not mistaken for a hang (#293).
    // Renders to stderr; the summary below stays on stdout.
    let mut progress = crate::progress::ProgressReporter::new(quiet, json);
    let stats =
        travsr_daemon::init_repo_with_progress(&repo_root, jobs, &mut |ev| progress.update(ev))?;
    let elapsed = progress.elapsed();
    progress.finish();

    let db_path = repo_root.join(".travsr/graph.db");

    if json {
        // Machine-readable summary on stdout for CI; progress went to stderr.
        let summary = serde_json::json!({
            "files_indexed": stats.files_indexed,
            "nodes_written": stats.nodes_written,
            "edges_written": stats.edges_written,
            "total_nodes": stats.total_nodes,
            "total_edges": stats.total_edges,
            "elapsed_s": elapsed.as_secs(),
            "db_path": db_path.display().to_string(),
        });
        println!("{summary}");
        return Ok(());
    }

    crate::progress::print_summary(&stats, elapsed, quiet);

    // Tips are advisory chatter — suppress under --quiet.
    if !quiet {
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
    }

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
