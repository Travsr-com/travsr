// Delegates to travsr_daemon::init_repo (DEBT-010 closed in Sprint 2).
use anyhow::Context as _;

use crate::repo::find_git_root_for_write;
use travsr_plugin_host::phase_b::status::Readiness;

// One parameter per `travsr init` flag, the same shape `graph::run` uses. A
// struct would only move the list somewhere else: clap already owns the
// canonical definition, and a second one here would be a copy to keep in sync.
#[allow(clippy::too_many_arguments)]
pub fn run(
    quiet: bool,
    json: bool,
    jobs: Option<usize>,
    force: bool,
    allow_unsandboxed_lsif: bool,
    no_connect: bool,
    guard: Option<crate::guard::GuardMode>,
) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    // Write command: index the worktree we are standing in, never redirect to
    // the main worktree (issue #586).
    let repo_root = find_git_root_for_write(&cwd)?;

    // Apply the operator opt-in before any indexing begins. This sets a
    // process-global flag consulted by run_ra_lsif via allow_unsandboxed_opt_in().
    travsr_daemon::set_allow_unsandboxed_lsif(allow_unsandboxed_lsif);
    // The same `--allow-unsandboxed` opt-in also permits, for this one run, the
    // Phase B analyzers that cannot run inside Travsr's isolation on Windows
    // (java/scala) to run with the user's own privileges. The persistent
    // `travsr lang allow-unsandboxed` grant is the primary path; this covers a
    // one-shot `travsr init`.
    travsr_plugin_host::resolver::set_allow_unsandboxed(allow_unsandboxed_lsif);
    if allow_unsandboxed_lsif {
        if let Err(e) = crate::lang::grant_unsandboxed_from_init("rust") {
            eprintln!("warning: could not save the Rust setting for later runs: {e:#}");
        }
    }

    // Ctrl-C, or SIGTERM from an editor or an agent's timeout: once indexing
    // is done, hand the rest to the daemon. An interrupted Phase B leaves
    // `last_commit` ahead of `phase_b_commit`, so the daemon picks it up.
    let _ = ctrlc::set_handler({
        let repo_root = repo_root.clone();
        move || {
            let indexed = TRACING_CALLS.load(std::sync::atomic::Ordering::SeqCst);
            let ci = std::env::var_os("CI").is_some();
            let outcome = hand_off_on_interrupt(ci, indexed).then(|| {
                std::env::current_exe().map_or(crate::daemon_client::SpawnOutcome::Failed, |exe| {
                    crate::daemon_client::spawn_background_daemon(&repo_root, &exe, false)
                })
            });
            eprintln!("\n{}", interrupt_note(outcome));
            std::process::exit(130);
        }
    });

    let db_path = repo_root.join(".travsr/graph.db");
    // The key the Phase B trust gate checks, from this worktree rather than cwd,
    // so a linked worktree trusts itself and not the main one.
    let corpus = travsr_daemon::detect_corpus(
        &repo_root
            .canonicalize()
            .unwrap_or_else(|_| repo_root.clone()),
    );

    // Get the language tools this repo needs. Gated on readiness so a re-run
    // with everything in place makes no network call. The default
    // `.travsrignore` is written first, so detection skips the same folders
    // (vendor/, testdata/, ...) indexing will, and sets up nothing for them.
    let travsrignore_scaffolded = travsr_daemon::scaffold_travsrignore(&repo_root).unwrap_or(false);
    let languages = crate::lang::detect_languages_in(&repo_root);
    grant_unsandboxed_where_needed(&languages);
    let skip_downloads = std::env::var_os("TRAVSR_SKIP_DOWNLOAD").is_some();
    let mut offline = false;
    if !skip_downloads {
        let states = readiness_of(&repo_root, &corpus, &languages, &stored_warnings(&db_path));
        let to_set_up = languages_to_set_up(&states);
        if !to_set_up.is_empty() {
            offline = crate::lang::install_selected(&to_set_up, true, true, Some(&corpus));
        }
    }
    let search_ranking = if travsr_mcp::rerank_model_installed() {
        "installed"
    } else if skip_downloads || offline {
        "skipped"
    } else {
        match travsr_mcp::install_rerank_model() {
            Ok(_) => "installed",
            Err(e) => {
                eprintln!("warning: could not get search ranking: {e:#}");
                "failed"
            }
        }
    };
    if offline {
        eprintln!("warning: no network. Run `travsr init` again when online to finish setting up.");
    }

    // Live progress so a long indexing run is not mistaken for a hang (#293).
    // Renders to stderr; the summary below stays on stdout.
    let mut progress = crate::progress::ProgressReporter::new(quiet, json);
    let mut stats =
        travsr_daemon::init_repo_with_progress(&repo_root, jobs, true, force, &mut |ev| {
            if matches!(ev, travsr_daemon::InitProgress::Finalizing) {
                TRACING_CALLS.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            progress.update(ev)
        })?;
    stats.travsrignore_scaffolded |= travsrignore_scaffolded;
    let elapsed = progress.elapsed();
    progress.finish();

    // #893: a `.gitignore` entry cannot un-track a path git already holds, so on
    // a repo that committed `.travsr/` before this existed, `init_repo`'s
    // scaffold changed nothing and `git revert`/`git merge` still refuse to run
    // against the permanently dirty WAL. Always stderr, so it reaches the `--json` path too without landing in
    // the machine-readable summary on stdout. Reported, not auto-fixed: removing
    // it rewrites the user's index.
    if stats.travsr_dir_tracked {
        eprintln!(
            "warning: git tracks files under .travsr/, so ignoring it has no effect. \
             The graph's WAL changes on every read, which keeps the working tree \
             dirty and makes `git revert`/`git merge` refuse to run. \
             `git rm -r --cached .travsr` to untrack it, then commit."
        );
    }

    // Keep the index fresh in the background: file watching, git hooks, and
    // Phase B for later commits. `CI` is the one opt-out, so a CI step or a test
    // never leaves a process behind; a terminal is not required, because agents
    // and editors run init without one.
    use crate::daemon_client::SpawnOutcome;
    let keeping_fresh = if std::env::var_os("CI").is_some() {
        if crate::daemon_client::daemon_lock_held(&repo_root) {
            "running"
        } else {
            "not_started"
        }
    } else {
        // Race-free: spawns only if no daemon holds the lock, so a re-`init` over
        // an already-running daemon never forks a doomed child.
        let exe = std::env::current_exe().context("finding current exe path")?;
        match crate::daemon_client::spawn_background_daemon(&repo_root, &exe, false) {
            SpawnOutcome::AlreadyRunning => "running",
            SpawnOutcome::Started | SpawnOutcome::Starting => "started",
            _ => "not_started",
        }
    };

    if json {
        // Machine-readable summary on stdout for CI; progress went to stderr.
        // #878: a CI consumer reads this field instead of the human summary, so
        // it must not say `complete` over a run whose TypeScript LSIF pass was
        // skipped, or whose analyzer crashed. `travsr status` calls both
        // `partial`; agree with it.
        let phase_b = match &stats.phase_b_report {
            None => "pending",
            Some(r) if !r.lsif_skipped.is_empty() || !r.crashed.is_empty() => "partial",
            Some(_) => "complete",
        };
        let states = readiness_of(&repo_root, &corpus, &languages, &stored_warnings(&db_path));
        let summary = serde_json::json!({
            "files_indexed": stats.files_indexed,
            "nodes_written": stats.nodes_written,
            "edges_written": stats.edges_written,
            "total_nodes": stats.total_nodes,
            "total_edges": stats.total_edges,
            "elapsed_s": elapsed.as_secs(),
            "phase_b": phase_b,
            "db_path": db_path.display().to_string(),
            // UX-023: expose the ghost sweep in JSON too, not just the human summary.
            "ghosts_pruned": stats.ghosts_pruned,
            "ghost_prune_aborted": stats.ghost_prune_aborted,
            "languages": states.iter().map(|(l, r)| language_json(l, r)).collect::<Vec<_>>(),
            "search_ranking": search_ranking,
            "keeping_fresh": keeping_fresh,
        });
        println!("{summary}");
        // stdout carries the machine-readable summary, so the connect report goes
        // to stderr. It must not be dropped: these writes land in tracked,
        // user-authored files, and RFC-026 promises they stay visible.
        maybe_connect(
            &repo_root,
            no_connect,
            guard,
            crate::connect::Report::Stderr,
        );
        return Ok(());
    }

    // A running daemon auto-arms Phase B on startup and indexes semantic call
    // edges in the background, so the summary must not call them commit-gated.
    let states = readiness_of(&repo_root, &corpus, &languages, &stored_warnings(&db_path));
    crate::progress::print_summary(
        &stats,
        elapsed,
        quiet,
        keeping_fresh != "not_started",
        &states,
    );

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
                "tip: run `git commit` to record a baseline, \
                 `travsr status` will show freshness after your first commit"
            );
        }
    }

    maybe_connect(
        &repo_root,
        no_connect,
        guard,
        if quiet {
            crate::connect::Report::Silent
        } else {
            crate::connect::Report::Stdout
        },
    );

    Ok(())
}

/// Detect AI coding tools and wire them to Travsr (RFC-026). Non-fatal: wiring
/// is a convenience, so a failure here must never fail `travsr init`.
fn maybe_connect(
    repo_root: &std::path::Path,
    no_connect: bool,
    guard: Option<crate::guard::GuardMode>,
    report: crate::connect::Report,
) {
    if no_connect {
        return;
    }
    let mut opts = crate::connect::ConnectOpts::auto();
    opts.report = report;
    // #916: `None` unless `--guard` was passed, which is what keeps a plain
    // `travsr init` from installing enforcement nobody asked for.
    opts.guard = guard;
    let _ = crate::connect::run(repo_root, &opts);
}

/// Set once indexing is done and call tracing has started: from then on an
/// interrupted init has work the daemon can finish.
static TRACING_CALLS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether an interrupted init starts the daemon to finish its work.
fn hand_off_on_interrupt(ci: bool, tracing_calls: bool) -> bool {
    !ci && tracing_calls
}

/// What an interrupted init tells the user, given the daemon it tried to hand
/// off to (`None` in CI, where nothing is left running).
fn interrupt_note(outcome: Option<crate::daemon_client::SpawnOutcome>) -> &'static str {
    use crate::daemon_client::SpawnOutcome::{AlreadyRunning, Started, Starting};
    match outcome {
        Some(Started | Starting | AlreadyRunning) => {
            "Tracing calls continues in the background (travsr status)."
        }
        _ => "Stopped. Run `travsr init` again to finish.",
    }
}

/// Languages `travsr init` can set up itself; the rest need the user.
fn languages_to_set_up(states: &[(String, Readiness)]) -> Vec<&str> {
    states
        .iter()
        .filter(|(_, r)| *r == Readiness::SettingUp)
        .map(|(l, _)| l.as_str())
        .collect()
}

pub(crate) fn readiness_of(
    repo_root: &std::path::Path,
    corpus: &str,
    languages: &[String],
    warnings: &str,
) -> Vec<(String, Readiness)> {
    use travsr_plugin_host::phase_b::status::{gather, readiness};
    let lang_toml = travsr_plugin_host::trust::LangToml::from_disk();
    let resolver = travsr_plugin_host::resolver::CatalogResolver::new();
    languages
        .iter()
        .filter_map(|l| {
            let entry = travsr_plugin_host::phase_b::lookup(l)?;
            let cap = gather(entry, repo_root, corpus, &lang_toml, &resolver, warnings);
            Some((l.clone(), readiness(&cap)))
        })
        .collect()
}

/// Readiness of every language in `repo_root`, from the same detection, corpus
/// key and last-run warnings `travsr init` uses, so `status` agrees with it.
pub(crate) fn repo_language_states(repo_root: &std::path::Path) -> Vec<(String, Readiness)> {
    let corpus = travsr_daemon::detect_corpus(
        &repo_root
            .canonicalize()
            .unwrap_or_else(|_| repo_root.to_path_buf()),
    );
    let languages = crate::lang::detect_languages_in(repo_root);
    let warnings = stored_warnings(&repo_root.join(".travsr/graph.db"));
    readiness_of(repo_root, &corpus, &languages, &warnings)
}

/// The last run's `phase_b_warnings`, or empty before the first run.
pub(crate) fn stored_warnings(db_path: &std::path::Path) -> String {
    if !db_path.exists() {
        return String::new();
    }
    travsr_store::SqliteStore::open(db_path)
        .ok()
        .and_then(|s| s.get_meta("phase_b_warnings").ok().flatten())
        .unwrap_or_default()
}

fn language_json(language: &str, r: &Readiness) -> serde_json::Value {
    let mut o = serde_json::json!({ "language": language, "state": r.tag() });
    match r {
        Readiness::NeedsToolchain { needs } => o["needs"] = needs.as_str().into(),
        Readiness::Unsupported { os } => o["os"] = os.as_str().into(),
        _ => {}
    }
    if let Some(fix) = r.fix() {
        o["fix"] = fix.into();
    }
    o
}

/// Record the unsandboxed grants init makes for the user (not printed): Rust
/// where the OS offers no sandbox, and on Windows the languages whose build
/// tools cannot run inside its isolation.
fn grant_unsandboxed_where_needed(languages: &[String]) {
    use travsr_indexer::sandbox::{build_sandboxed_command, SandboxConfig, SandboxStatus};
    let no_sandbox = matches!(
        build_sandboxed_command("true", &[], &SandboxConfig::default()).1,
        SandboxStatus::Unavailable { .. }
    );
    for lang in languages {
        let needed = (lang == "rust" && no_sandbox)
            || (cfg!(windows)
                && travsr_plugin_host::phase_b::lookup(lang)
                    .is_some_and(|e| e.windows_sandbox_unsupported()));
        if needed {
            let _ = crate::lang::grant_unsandboxed_from_init(lang);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_languages_init_can_fix_are_set_up() {
        let states = vec![
            ("go".to_string(), Readiness::SettingUp),
            ("python".to_string(), Readiness::Ready),
            (
                "java".to_string(),
                Readiness::NeedsToolchain {
                    needs: "JDK".into(),
                },
            ),
            (
                "swift".to_string(),
                Readiness::Unsupported { os: "linux".into() },
            ),
            ("ruby".to_string(), Readiness::Failed),
            ("rust".to_string(), Readiness::SettingUp),
        ];
        assert_eq!(languages_to_set_up(&states), ["go", "rust"]);
        assert!(languages_to_set_up(&[]).is_empty());
    }

    #[test]
    fn an_interrupt_only_promises_what_the_daemon_will_do() {
        use crate::daemon_client::SpawnOutcome;
        let continues = "Tracing calls continues in the background (travsr status).";
        let stopped = "Stopped. Run `travsr init` again to finish.";
        for outcome in [
            SpawnOutcome::Started,
            SpawnOutcome::Starting,
            SpawnOutcome::AlreadyRunning,
        ] {
            assert_eq!(interrupt_note(Some(outcome)), continues);
        }
        assert_eq!(interrupt_note(Some(SpawnOutcome::Failed)), stopped);
        // No hand-off attempted: nothing is left running.
        assert_eq!(interrupt_note(None), stopped);
    }

    #[test]
    fn an_interrupt_hands_off_only_finished_indexing_outside_ci() {
        // Before indexing finished the daemon has nothing to continue from.
        assert!(hand_off_on_interrupt(false, true));
        assert!(!hand_off_on_interrupt(false, false));
        assert!(
            !hand_off_on_interrupt(true, true),
            "CI leaves nothing running"
        );
        assert!(!hand_off_on_interrupt(true, false));
    }

    #[test]
    fn a_refused_connection_is_a_network_error() {
        // Port 9 (discard) is closed on a dev machine and CI: connect is refused.
        let err = crate::lang::run_async(async {
            reqwest::Client::new()
                .get("http://127.0.0.1:9/")
                .send()
                .await
                .map(|_| ())
                .map_err(anyhow::Error::from)
        })
        .unwrap_err()
        .context("downloading wrapper binary");
        assert!(crate::lang::is_network_error(&err));
        assert!(!crate::lang::is_network_error(&anyhow::anyhow!(
            "'go' is not installed on your machine"
        )));
    }
}
