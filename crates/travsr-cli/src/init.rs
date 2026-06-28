// Delegates to travsr_daemon::init_repo (DEBT-010 closed in Sprint 2).
use anyhow::Context as _;
use indicatif::{ProgressBar, ProgressStyle};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::repo::find_git_root;

pub fn run() -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let repo_root = find_git_root(&cwd)?;

    let pb = ProgressBar::new(0);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.cyan} indexing  {bar:20.white/white}  {pos}/{len}  {percent}%   {elapsed}  · eta {eta}",
        )
        .unwrap()
        .progress_chars("█░░")
        .tick_strings(&["◐", "◓", "◑", "◒"]),
    );
    pb.enable_steady_tick(Duration::from_millis(120));

    // Shared total so the callback can set the bar length on first call.
    let total_set = Arc::new(AtomicU64::new(0));
    let pb_cb = pb.clone();
    let total_set_cb = Arc::clone(&total_set);

    let stats = travsr_daemon::init_repo_with_progress(&repo_root, move |done, total| {
        if total_set_cb.load(Ordering::Relaxed) == 0 {
            pb_cb.set_length(total);
            total_set_cb.store(total, Ordering::Relaxed);
        }
        pb_cb.set_position(done);
    })?;

    pb.finish_and_clear();

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
