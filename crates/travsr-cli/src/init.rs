// Delegates to travsr_daemon::init_repo (DEBT-010 closed in Sprint 2).
use anyhow::Context as _;

use crate::repo::find_git_root_for_write;

pub fn run(
    quiet: bool,
    json: bool,
    jobs: Option<usize>,
    semantic: bool,
    force: bool,
    allow_unsandboxed_lsif: bool,
) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    // Write command: index the worktree we are standing in, never redirect to
    // the main worktree (issue #586).
    let repo_root = find_git_root_for_write(&cwd)?;

    // Apply the operator opt-in before any indexing begins. This sets a
    // process-global flag consulted by run_ra_lsif via allow_unsandboxed_opt_in().
    travsr_daemon::set_allow_unsandboxed_lsif(allow_unsandboxed_lsif);

    // Live progress so a long indexing run is not mistaken for a hang (#293).
    // Renders to stderr; the summary below stays on stdout.
    let mut progress = crate::progress::ProgressReporter::new(quiet, json);
    let stats =
        travsr_daemon::init_repo_with_progress(&repo_root, jobs, semantic, force, &mut |ev| {
            progress.update(ev)
        })?;
    let elapsed = progress.elapsed();
    progress.finish();

    let db_path = repo_root.join(".travsr/graph.db");

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

    // Always start the daemon after init in interactive terminals so file
    // watching, git hooks, Phase B, and post-Phase-B embedding all work
    // without a manual `travsr daemon start`.
    // Guard with is_terminal so we never spawn a background process in CI,
    // piped contexts, or integration tests (where it would race the DB lock).
    use crate::daemon_client::SpawnOutcome;
    use std::io::IsTerminal as _;
    // Whether a daemon is (or is coming) up after init. This decides the Phase B
    // summary wording: a running daemon auto-arms Phase B on startup and indexes
    // semantic call edges in the background, so "commit-gated" would be wrong.
    let daemon_running = if std::io::stdout().is_terminal() {
        // Race-free: spawns only if no daemon holds the lock, so a re-`init` over
        // an already-running daemon never forks a doomed child.
        let exe = std::env::current_exe().context("finding current exe path")?;
        matches!(
            crate::daemon_client::spawn_background_daemon(&repo_root, &exe),
            SpawnOutcome::Started | SpawnOutcome::Starting | SpawnOutcome::AlreadyRunning
        )
    } else {
        // Non-interactive (CI / piped): we never spawn, but a daemon started
        // earlier may already be running and will pick up the pending Phase B.
        crate::daemon_client::daemon_lock_held(&repo_root)
    };

    crate::progress::print_summary(&stats, elapsed, quiet, daemon_running);

    // UX-007: a no-op re-run (nothing changed) should not reprint the setup
    // nudges — they are advice for a fresh index, not chatter for every `init`.
    let no_op = stats.nodes_written == 0 && stats.edges_written == 0;

    // Tips are advisory chatter — suppress under --quiet and on no-op re-runs.
    if !quiet && !no_op {
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
        hint_embed_missing();
    }

    Ok(())
}

/// Print a tip when no embed backend is active so users know about semantic search.
fn hint_embed_missing() {
    if travsr_plugin_host::active_backend_id().is_none() {
        println!(
            "tip: semantic search is not set up — run `travsr embed init` for natural-language queries"
        );
    }
}

/// After indexing, scan for supported languages and name the exact
/// per-language install command if any are present but not yet registered.
/// On a TTY, offer to run the interactive `travsr lang detect` flow inline
/// (#449: call/reference indexing for these languages needs the sidecar, and
/// the generic tip was too easy to miss).
fn hint_lang_detect(repo_root: &std::path::Path) -> anyhow::Result<()> {
    use std::io::IsTerminal as _;

    // UX-001/UX-013: only nudge for languages that genuinely still need setup —
    // built-in languages (rust/typescript/python/dart) are excluded, so we never
    // tell a user their already-working semantic support is "not set up".
    let unregistered = crate::lang::languages_needing_setup(repo_root);
    if unregistered.is_empty() {
        return Ok(());
    }

    // UX-007: one line, not one `travsr lang install <lang>` line per language.
    // `lang install` takes a single language, so for multiple we point at
    // `lang detect` (the interactive multi-install flow); for one, name it.
    if let [only] = unregistered.as_slice() {
        println!(
            "tip: {only} found in this repo but semantic (call/reference) indexing is not set up — run: travsr lang install {only}"
        );
    } else {
        println!(
            "tip: {} found in this repo but semantic (call/reference) indexing is not set up — run: travsr lang detect",
            unregistered.join(", ")
        );
    }

    if std::io::stdin().is_terminal() {
        use std::io::Write as _;
        print!("Set up now? [y/N]: ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if answer.trim().eq_ignore_ascii_case("y") {
            return crate::lang::run(crate::lang::LangCommand::Detect);
        }
    }

    Ok(())
}
