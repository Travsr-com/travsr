//! `travsr status` — index and graph health summary.
//!
//! Data acquisition is shared with the daemon via `travsr_mcp::query`
//! (#318 O1): a running daemon answers from its warm store; otherwise the
//! store is opened directly (read-only fast path).

use anyhow::Context as _;
use travsr_mcp::query::{self, StatusPayload};

use crate::daemon_client;
use crate::repo::find_git_root;

/// M7: compare `last_commit` vs `phase_b_commit` to describe Phase B freshness.
///
/// #583: equal markers are not sufficient evidence of freshness. A watcher
/// reindex rewrites a file's Phase A nodes and drops that file's `ref/call`
/// edges without moving HEAD, so both markers still agree while `get_callers`
/// and `get_blast_radius` answer from a graph degraded below the committed
/// snapshot. Reporting `complete` there is the actual harm; the edges
/// themselves return on the next commit's Phase B run.
///
/// The dirty flag therefore only changes the verdict inside that one window.
/// Once the markers diverge, `pending` already tells the user a run is coming.
///
/// The wording names the condition, not a remedy, because there is no single
/// correct remedy. The motivating cases (branch switch, `git stash pop`,
/// revert) all restore the file to its committed content, so the working tree
/// ends up equal to HEAD with the flag still set and the `ref/call` edge still
/// missing. Telling the user to commit is a dead end there: there is nothing
/// to stage. Recovery is `travsr init`, or any later commit that fires the
/// hook.
fn phase_b_state(payload: &StatusPayload) -> String {
    match payload.phase_b_commit.as_deref() {
        Some(pb) if !pb.is_empty() && Some(pb) == payload.last_commit.as_deref() => {
            if payload.phase_b_dirty {
                "stale (run travsr init to refresh)".to_string()
            } else {
                // #712: the marker now advances even when a language crashed, so
                // the healthy languages are complete and queryable at HEAD. Name
                // any crashed language rather than reporting a flat "complete"
                // that contradicts the per-language reality and the crash
                // warning printed below.
                let crashed = crashed_langs(payload);
                if crashed.is_empty() {
                    "complete".to_string()
                } else {
                    format!("partial (crashed: {})", crashed.join(", "))
                }
            }
        }
        Some(pb) if !pb.is_empty() => "pending".to_string(),
        _ => "not run".to_string(),
    }
}

/// #712: languages whose Phase B sidecar crashed on the last run, parsed from the
/// `phase_b_warnings` meta (`crashed:<lang>` entries). Used to downgrade the
/// `semantic:` field from `complete` to `partial (crashed: …)` so it agrees with
/// the crash warning and the per-language outcome.
fn crashed_langs(payload: &StatusPayload) -> Vec<String> {
    payload
        .phase_b_warnings
        .as_deref()
        .unwrap_or("")
        .split(',')
        .filter_map(|w| w.strip_prefix("crashed:"))
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// #645 WS-B: the caller's live short HEAD, read at `cwd` (before the worktree
/// redirect in `find_git_root`, so a linked worktree reports its own commit,
/// not the main worktree's). `None` when git is unavailable or the dir is not a
/// repo — the mismatch note then correctly never fires.
fn head_at(cwd: &std::path::Path) -> Option<String> {
    // Bounded: an unbounded `output()` here can never return on Windows when a
    // git child or grandchild inherits the pipe (#717 triage, same mechanism as
    // #503 / #572). A HEAD that does not arrive is the same as no HEAD, which
    // this function already handles.
    // `cwd` goes through as a real path rather than `-C <string>`: a path with
    // bytes that are not valid UTF-8 is legal, and converting it to a string
    // first would mangle it into U+FFFD and lose a repo that exists.
    crate::git_bounded::git_stdout_bounded(Some(cwd), ["rev-parse", "--short", "HEAD"])
}

pub fn run() -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    // `head_at` and `find_git_root` are independent, bounded git queries on the
    // same `cwd` (the latter only shells out in the linked-worktree branch, via
    // `main_worktree_root`). Run concurrently rather than sequentially: with a
    // wedged git, sequential calls each pay their own `GIT_QUERY_TIMEOUT`, so
    // this command could stall for up to 2x the bound instead of 1x.
    let head_handle = {
        let cwd = cwd.clone();
        std::thread::spawn(move || head_at(&cwd))
    };
    let repo_root = find_git_root(&cwd)?;
    let head = head_handle.join().ok().flatten();

    let db_path = repo_root.join(".travsr").join("graph.db");

    if !db_path.exists() {
        anyhow::bail!("not initialized — run `travsr init`");
    }

    let payload: StatusPayload =
        match daemon_client::try_query(&repo_root, "status", serde_json::json!({})) {
            Some(p) => p,
            None => {
                let store = daemon_client::open_read_store(&db_path)
                    .with_context(|| format!("opening graph database at {}", db_path.display()))?;
                query::status_query(&store)?
            }
        };

    let last_commit = payload.last_commit.as_deref().unwrap_or("(none)");
    let phase_b_state = phase_b_state(&payload);
    // RFC-021 P5: reranker state. Old daemons omit the field (serde default
    // empty) — suppress the segment then so mixed CLI/daemon versions stay clean.
    let rerank_segment = if payload.rerank.is_empty() {
        String::new()
    } else {
        format!(" | rerank: {}", payload.rerank)
    };
    println!(
        "nodes: {} | edges: {} | schema: v{} | journal: {} | last_commit: {} | semantic: {}{}",
        payload.nodes,
        payload.edges,
        payload.schema,
        payload.journal,
        last_commit,
        phase_b_state,
        rerank_segment
    );

    // #645 WS-B: the freshness markers only ever compare against each other,
    // never against the repository. Compare the caller's live HEAD (read at cwd,
    // above) to the index's last_commit so a checkout at a different revision —
    // a linked worktree, or a HEAD move the daemon has not yet reconciled — is
    // never answered for silently. cwd-local, so it holds for both the
    // daemon-answered and cold-store payloads.
    if let Some(head) = head.as_deref() {
        let stored = payload.last_commit.as_deref().unwrap_or("");
        if let Some(note) = travsr_mcp::head_index_mismatch_note(head, stored) {
            eprintln!("{note}");
        }
    }

    // RFC-014 #317 re-index policy: surface signature-format skew so the user
    // knows the graph was built with an older format and a re-index is due.
    let sig_v = payload.signature_format_version;
    if sig_v != travsr_core::SIGNATURE_FORMAT_VERSION {
        eprintln!(
            "warning: signature format v{sig_v} != current v{} — graph built with an older format; run `travsr init` to re-index",
            travsr_core::SIGNATURE_FORMAT_VERSION
        );
    }

    // L11: detect FTS/nodes skew — indicates a partial write or corrupt FTS index.
    let fts = payload.fts_nodes;
    if fts > 0 && fts != payload.nodes {
        eprintln!(
            "warning: text search index has {fts} rows but the graph has {} nodes — run `travsr init` to rebuild",
            payload.nodes
        );
    }

    // H3: surface Phase B warnings so the user knows about crashed/mismatched
    // analyzers without having to re-read the init output.
    if let Some(warnings) = &payload.phase_b_warnings {
        if !warnings.is_empty() {
            // Trust is per-repo, not per-language: a single `install` enables
            // every language at once, so collapse the "not enabled here" notices
            // into one line rather than repeating it per language (matches init).
            let untrusted: Vec<&str> = warnings
                .split(',')
                .filter_map(|w| w.strip_prefix("untrusted_corpus:"))
                .collect();
            if !untrusted.is_empty() {
                eprintln!(
                    "warning: semantic analysis is not enabled for this repository yet ({}) — run `travsr lang install <lang>` here to enable",
                    untrusted.join(", ")
                );
            }
            for warn in warnings.split(',') {
                let parts: Vec<&str> = warn.splitn(2, ':').collect();
                match parts.as_slice() {
                    // #712: point at the force path. A plain `travsr init
                    // --semantic` re-runs on top of the existing graph, which a
                    // no-op Phase A can make look like it did nothing; `--force`
                    // purges and rebuilds so the retry is unambiguous.
                    ["crashed", lang] => eprintln!(
                        "warning: semantic analyzer for '{lang}' crashed — fix the tool (e.g. `travsr lang install {lang}`), then re-run `travsr init --semantic --force` to rebuild"
                    ),
                    ["version_mismatch", rest] => {
                        let v: Vec<&str> = rest.splitn(3, ':').collect();
                        if let [lang, expected, got] = v.as_slice() {
                            eprintln!(
                                "warning: '{lang}' sidecar protocol v{got} != expected v{expected} — run `travsr lang install {lang}`"
                            );
                        }
                    }
                    ["needs_approval", lang] => eprintln!(
                        "warning: '{lang}' needs a one-time network approval before it can index — run `travsr lang approve {lang}`"
                    ),
                    // #712: analyzer ran but produced no nodes over the repo's
                    // source files of this language — a silent zero-node result,
                    // not a crash. Point at the tool and a rebuild.
                    // UX-3: the analyzer is installed and active (the zero-node
                    // warning only fires for a language that ran), so telling the
                    // user to reinstall misdirects — reinstalling changes nothing.
                    // The real causes are the sidecar failing to parse/resolve this
                    // repo's sources (e.g. a sandbox-denied read, a missing SDK, or
                    // no buildable project). Point at the sidecar's own diagnostics,
                    // which the host now forwards on stderr.
                    ["zero_nodes", lang] => {
                        eprintln!(
                            "warning: '{lang}' analysis ran but found no symbols, though the repo has '{lang}' sources. The analyzer is installed, so reinstalling will not help — it usually means the analyzer could not read or build this project's sources (a missing SDK or an unbuildable project). Fix the project setup, then re-run `travsr init --semantic --force`"
                        );
                        // #724 Finding 4: the most common cause of a zero-node
                        // Java run on macOS is scip-java's javac shim crashing
                        // under the stock bash 3.2. Surface the actionable fix.
                        if *lang == "java" {
                            if let Some(hint) = crate::progress::macos_java_bash_hint() {
                                eprintln!("  {hint}");
                            }
                        }
                    }
                    // #449: languages present in the repo whose Phase B sidecar
                    // never ran, previously a silent skip that left the user
                    // with "0 references" and no explanation.
                    ["skipped_unregistered", lang] => eprintln!(
                        "warning: '{lang}' sources found but full analysis is not set up. Run `travsr lang install {lang}`"
                    ),
                    // #414 (ADR-017 Rule 3): registered globally but this repo was
                    // never enabled. Collapsed into one combined line above the
                    // loop (trust is per-repo, so one install fixes all of them).
                    ["untrusted_corpus", _] => {}
                    ["skipped_no_analyzer", lang] => eprintln!(
                        "warning: '{lang}' is registered but its analyzer binary is missing. Run `travsr lang install {lang}`"
                    ),
                    // L5a: scip-clang (c/cpp) needs a compile_commands.json at the
                    // repo root — without one it hangs, so it is skipped up front.
                    ["skipped_no_compdb", lang] => eprintln!(
                        "warning: full '{lang}' analysis needs a compile database (compile_commands.json) at the repo root. Generate one (e.g. `bear -- make`, or CMake's CMAKE_EXPORT_COMPILE_COMMANDS)"
                    ),
                    // E6: SCIP definitions that did not unify onto their Phase A
                    // tree-sitter node — their references attribute to an orphaned
                    // duplicate node instead. `rate` is missed/attempted.
                    ["scip_unification_misses", rate] => eprintln!(
                        "warning: {rate} semantic definitions did not match their parsed symbol — some references may resolve to a duplicate. Re-run `travsr init --semantic` if it persists."
                    ),
                    _ => {}
                }
            }
        }
    }

    // M1: warn when Rust's full cross-file edges were skipped for lack of a
    // security sandbox. The remedy is per-OS: only Linux has a sandbox the user
    // can install (bubblewrap); Windows and macOS have none to add here, so the
    // only path to full edges is the trusted-repo opt-in. `cfg!` (not `#[cfg]`)
    // keeps every platform's wording compiled and checked.
    if let Some(reason) = &payload.rust_lsif_degraded {
        if reason == "sandbox_unavailable" {
            let remedy = if cfg!(target_os = "linux") {
                "Install bubblewrap, or re-run `travsr init --allow-unsandboxed-lsif` if you trust this repo."
            } else {
                "Re-run `travsr init --allow-unsandboxed-lsif` if you trust this repo."
            };
            eprintln!(
                "warning: Rust is on basic analysis — full cross-file edges (from rust-analyzer) were skipped because they need a security sandbox that is not available here. {remedy}"
            );
        }
    }

    // WS-2: warn when Dart Phase B ran without resolved dependencies, so a
    // partial cross-package index is never mistaken for a complete one.
    if let Some(pkgs) = payload.dart_deps_unresolved.as_deref() {
        if !pkgs.is_empty() {
            eprintln!(
                "warning: Dart cross-package references are incomplete — these \
                 package(s) were indexed without resolved dependencies: {pkgs}. \
                 Run `dart pub get` in each to enable cross-package references \
                 (intra-package references are unaffected)."
            );
        }
    }

    // RFC-025 §8: sidecar version health (installed vs required vs latest), with
    // the exact remedy. Computed offline; the `latest` note is present only when
    // the local cache is warm. Prints nothing when no sidecar is installed.
    crate::sidecar_health::print_block();

    // #712 F: the embed sidecar can be installed while no backend is active, so
    // the semantic path silently runs without embeddings. Nudge to enable it.
    crate::embed::hint_activate_if_installed(&repo_root);

    Ok(())
}

#[cfg(test)]
mod tests {

    /// #717: `head_at` runs on every `travsr status`, and it used an unbounded
    /// `Command::output()`. These pin the two answers it must give without
    /// hanging for either: a real repo reports a short SHA, a directory that is
    /// not a repo reports nothing.
    #[test]
    fn head_at_reports_a_sha_inside_a_repo_and_none_outside() {
        let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let sha = head_at(here).expect("this crate lives in a git repo");
        assert!(!sha.is_empty(), "a short SHA, not an empty string");
        assert!(!sha.contains('\n'), "arrives trimmed: {sha:?}");
        assert!(
            sha.chars().all(|c| c.is_ascii_hexdigit()),
            "a short SHA is hex: {sha:?}"
        );

        let tmp = tempfile::tempdir().unwrap();
        assert!(
            head_at(tmp.path()).is_none(),
            "outside a repo there is no HEAD, and asking must not hang"
        );
    }

    /// The bound is what stops a wedged git holding the CLI forever, so the
    /// happy path must not be anywhere near it.
    #[test]
    fn head_at_is_far_inside_its_deadline() {
        let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let started = std::time::Instant::now();
        let _ = head_at(here);
        assert!(
            started.elapsed() * 4 < crate::git_bounded::GIT_QUERY_TIMEOUT,
            "a warm rev-parse should finish in a small fraction of the deadline, took {:?}",
            started.elapsed()
        );
    }
    use super::*;

    fn payload(last: &str, phase_b: &str, dirty: bool) -> StatusPayload {
        StatusPayload {
            nodes: 1,
            fts_nodes: 1,
            edges: 0,
            schema: 21,
            journal: "wal".into(),
            last_commit: Some(last.to_string()),
            signature_format_version: travsr_core::SIGNATURE_FORMAT_VERSION,
            phase_b_commit: Some(phase_b.to_string()),
            phase_b_warnings: None,
            rust_lsif_degraded: None,
            rerank: String::new(),
            phase_b_dirty: dirty,
            dart_deps_unresolved: None,
        }
    }

    #[test]
    fn phase_b_reports_complete_when_markers_agree_and_nothing_is_dirty() {
        assert_eq!(phase_b_state(&payload("abc", "abc", false)), "complete");
    }

    #[test]
    fn phase_b_reports_stale_when_a_watcher_reindex_degraded_the_graph() {
        // #583: the exact window this PR exists for. Both markers agree, so the
        // old logic said `complete`, but the file's `ref/call` edges are gone.
        assert_eq!(
            phase_b_state(&payload("abc", "abc", true)),
            "stale (run travsr init to refresh)"
        );
    }

    #[test]
    fn phase_b_still_reports_pending_when_markers_diverge() {
        // A run is already coming, so "commit to refresh" would be wrong
        // advice. The dirty flag must not override this.
        assert_eq!(phase_b_state(&payload("def", "abc", false)), "pending");
        assert_eq!(phase_b_state(&payload("def", "abc", true)), "pending");
    }

    #[test]
    fn phase_b_reports_not_run_before_the_first_run() {
        assert_eq!(phase_b_state(&payload("abc", "", true)), "not run");
    }

    #[test]
    fn phase_b_reports_partial_when_a_language_crashed_but_markers_agree() {
        // #712: the marker advances on a partial run (healthy languages are
        // queryable at HEAD), so the field must name the crashed language rather
        // than claiming a flat "complete".
        let mut p = payload("abc", "abc", false);
        p.phase_b_warnings = Some("crashed:objectivec,skipped_no_analyzer:php".into());
        assert_eq!(phase_b_state(&p), "partial (crashed: objectivec)");
    }

    #[test]
    fn phase_b_partial_names_every_crashed_language() {
        let mut p = payload("abc", "abc", false);
        p.phase_b_warnings = Some("crashed:objectivec,crashed:swift".into());
        assert_eq!(phase_b_state(&p), "partial (crashed: objectivec, swift)");
    }

    #[test]
    fn phase_b_non_crash_warnings_do_not_downgrade_complete() {
        // Skip classes (no analyzer, needs approval, …) are not crashes and must
        // not turn a clean "complete" into "partial".
        let mut p = payload("abc", "abc", false);
        p.phase_b_warnings = Some("skipped_no_analyzer:php,needs_approval:go".into());
        assert_eq!(phase_b_state(&p), "complete");
    }
}
