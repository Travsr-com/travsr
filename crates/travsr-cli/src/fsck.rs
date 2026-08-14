use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::repo::{find_git_root, find_git_root_for_write};

/// #660 WS-C: resolve which index `fsck` operates on, enforcing the read-vs-repair
/// split so the command can diagnose the *exact* index the read path serves.
///
/// Reads resolve via `find_git_root` (PreferLocalElseMain): from a linked
/// worktree without its own `.travsr/` they answer from the main worktree's
/// index. The old `fsck` used `find_git_root_for_write` (StayInWorktree), which
/// resolves to the absent worktree `.travsr/` and errors `not initialized` — so
/// the user could not fsck the very index their queries came from.
///
/// - **Report mode** (`fix == false`): always the served index (`read_root`).
///   Read-only ⇒ zero #586 conflict.
/// - **`--fix`**: keeps the #586 write-safety split. When the served index
///   belongs to another checkout (a worktree served by the main index), refuse
///   and name it — never a silent redirect into a different worktree's store.
fn resolve_fsck_root(cwd: &Path, fix: bool) -> anyhow::Result<PathBuf> {
    let read_root = find_git_root(cwd)?;
    if fix {
        let write_root = find_git_root_for_write(cwd)?;
        if read_root != write_root {
            anyhow::bail!(
                "this worktree's queries are served by the index at {}; run \
                 `travsr fsck --fix` from there. A linked worktree must not \
                 mutate another checkout's index (#586). `travsr fsck` (report \
                 only) inspects that index from here.",
                read_root.display()
            );
        }
    }
    Ok(read_root)
}

pub fn run(fix: bool, json: bool, force: bool) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let repo_root = resolve_fsck_root(&cwd, fix)?;

    let report = travsr_daemon::fsck_repo(&repo_root, fix, force)?;

    if json {
        let out = serde_json::to_string_pretty(&report).context("serializing GcReport to JSON")?;
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

    if fix {
        if report.orphan_edges_swept > 0 {
            println!("swept {} orphan edge(s)", report.orphan_edges_swept);
        }
        if report.self_ref_call_edges_swept > 0 {
            println!(
                "swept {} self-referential ref/call edge(s) (#650)",
                report.self_ref_call_edges_swept
            );
        }
    } else {
        if report.orphan_edges_detected > 0 {
            println!(
                "{} orphan edge(s) with a missing endpoint (run with --fix to sweep)",
                report.orphan_edges_detected
            );
        }
        if report.self_ref_call_edges_detected > 0 {
            println!(
                "{} self-referential ref/call edge(s) (run with --fix to sweep) (#650)",
                report.self_ref_call_edges_detected
            );
        }
    }

    if let Some(issue) = &report.lexical_index_parity_issue {
        println!("warning: {issue}");
    }

    if report.aborted {
        if let Some(reason) = &report.abort_reason {
            anyhow::bail!("fsck aborted: {reason}");
        }
        anyhow::bail!("fsck aborted");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Build `<tmp>/main` (committed) + a linked worktree `<tmp>/wt`. Mirrors
    /// `repo::tests::main_and_worktree` (that helper is private to its module).
    fn main_and_worktree() -> Option<(tempfile::TempDir, PathBuf, PathBuf)> {
        if !git_available() {
            eprintln!("skipping: git not available");
            return None;
        }
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("main");
        std::fs::create_dir(&main).unwrap();
        assert!(git(&main, &["init", "-q"]));
        assert!(git(&main, &["config", "user.email", "t@t.t"]));
        assert!(git(&main, &["config", "user.name", "t"]));
        std::fs::write(main.join("f.txt"), "x").unwrap();
        assert!(git(&main, &["add", "."]));
        assert!(git(&main, &["commit", "-qm", "init"]));
        let wt = tmp.path().join("wt");
        assert!(git(&main, &["worktree", "add", "-q", wt.to_str().unwrap()]));
        assert!(wt.join(".git").is_file());
        Some((tmp, main, wt))
    }

    /// Report mode from a worktree without its own `.travsr/` targets the main
    /// worktree's served index — the exact index reads answer from — instead of
    /// the old `not initialized` error (#660).
    #[test]
    fn fsck_report_from_worktree_targets_main_index() {
        let Some((_tmp, main, wt)) = main_and_worktree() else {
            return;
        };
        let root = resolve_fsck_root(&wt, false).unwrap();
        assert_eq!(
            root.canonicalize().unwrap(),
            main.canonicalize().unwrap(),
            "report mode must inspect the served (main) index from the worktree"
        );
    }

    /// `--fix` from a worktree served by the main index is refused (never a
    /// silent cross-checkout mutation, #586) and the message names the main
    /// root so the user can repair from there.
    #[test]
    fn fsck_fix_from_worktree_refuses_and_names_main() {
        let Some((_tmp, _main, wt)) = main_and_worktree() else {
            return;
        };
        // The message embeds `find_git_root(&wt).display()` (the served main
        // root). Compare against that exact value, not the test's own `main`
        // PathBuf: on Windows git renders `--git-common-dir` with forward
        // slashes (`C:/…/main`) while the native PathBuf uses backslashes
        // (`C:\…\main`), so a `main.display()` substring check fails there even
        // though the refusal is correct.
        let served = find_git_root(&wt).unwrap();
        let err = resolve_fsck_root(&wt, true).unwrap_err().to_string();
        assert!(
            err.contains(&served.display().to_string()),
            "refusal must name the served main root: {err}"
        );
        assert!(
            err.contains("#586"),
            "refusal must cite the invariant: {err}"
        );
    }

    /// A worktree that has its own `.travsr/` (was `travsr init`-ed in place)
    /// still repairs in place — guards against over-refusing the legitimate case.
    #[test]
    fn fsck_fix_in_own_worktree_still_resolves_locally() {
        let Some((_tmp, _main, wt)) = main_and_worktree() else {
            return;
        };
        std::fs::create_dir_all(wt.join(".travsr")).unwrap();
        std::fs::write(wt.join(".travsr/graph.db"), b"").unwrap();
        let root = resolve_fsck_root(&wt, true).unwrap();
        assert_eq!(
            root.canonicalize().unwrap(),
            wt.canonicalize().unwrap(),
            "a worktree with its own index must repair itself, not be refused"
        );
    }

    /// A standard repo (`.git` is a directory) resolves the same root for both
    /// intents, so `--fix` is never refused.
    #[test]
    fn fsck_fix_in_standard_repo_is_not_refused() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let root = resolve_fsck_root(tmp.path(), true).unwrap();
        assert_eq!(
            root.canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap()
        );
    }
}
