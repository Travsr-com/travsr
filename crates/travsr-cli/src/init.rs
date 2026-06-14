// Delegates to travsr_daemon::init_repo (DEBT-010 closed in Sprint 2).
use anyhow::Context as _;

use crate::repo::find_git_root;

pub fn run(quiet: bool, json: bool, jobs: Option<usize>, semantic: bool) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let repo_root = find_git_root(&cwd)?;

    // Live progress so a long indexing run is not mistaken for a hang (#293).
    // Renders to stderr; the summary below stays on stdout.
    let mut progress = crate::progress::ProgressReporter::new(quiet, json);
    let stats = travsr_daemon::init_repo_with_progress(&repo_root, jobs, semantic, &mut |ev| {
        progress.update(ev)
    })?;
    let elapsed = progress.elapsed();
    progress.finish();

    let db_path = repo_root.join(".travsr/graph.db");

    // On the deferred path (phase_b_report == None), spawn the daemon so Phase B
    // runs in the background immediately without requiring the user to remember
    // `travsr daemon start`. If the daemon is already running, the spawn is a
    // no-op — the running daemon will auto-arm via phase_b_tick within 5 s.
    if stats.phase_b_report.is_none() {
        if let Err(e) = spawn_background_daemon(&repo_root) {
            // Non-fatal: daemon spawn failure must not fail `travsr init`.
            // The user can run `travsr daemon start` manually.
            tracing::warn!("could not auto-start daemon for background Phase B: {e:#}");
            if !quiet {
                eprintln!(
                    "warning: could not start background daemon — \
                     run `travsr daemon start` to complete semantic indexing"
                );
            }
        }
    }

    if json {
        // Machine-readable summary on stdout for CI; progress went to stderr.
        let phase_b = if stats.phase_b_report.is_some() {
            "complete"
        } else {
            "pending"
        };
        let summary = serde_json::json!({
            "files_indexed": stats.files_indexed,
            "nodes_written": stats.nodes_written,
            "edges_written": stats.edges_written,
            "total_nodes": stats.total_nodes,
            "total_edges": stats.total_edges,
            "elapsed_s": elapsed.as_secs(),
            "phase_b": phase_b,
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

/// Spawn the travsr daemon as a detached background process so Phase B runs
/// without the user having to run `travsr daemon start` manually.
///
/// If the daemon is already running (control socket responds) the spawn is
/// skipped — the running daemon will auto-arm its Phase B scheduler within one
/// `phase_b_tick` interval (≤ 5 s).
fn spawn_background_daemon(repo_root: &std::path::Path) -> anyhow::Result<()> {
    // Guard: skip spawn when a daemon is already responding on the socket.
    // The running daemon will auto-arm its Phase B scheduler within 5 s via
    // the phase_b_tick handler.
    if super::daemon_client::send_daemon_command(repo_root, &travsr_ipc::ControlMessage::Status)
        .is_ok()
    {
        tracing::debug!("daemon already running — skipping auto-spawn for Phase B");
        return Ok(());
    }

    let exe = std::env::current_exe().context("finding current exe path")?;
    std::process::Command::new(&exe)
        .args(["daemon", "start", "--foreground"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("spawning background daemon")?;

    tracing::info!("background daemon spawned for Phase B indexing");
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
