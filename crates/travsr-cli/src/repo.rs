use std::path::{Path, PathBuf};

use travsr_store::registry;

/// Resolve the Travsr repo root for `start` — the directory whose `.travsr/`
/// holds the index.
///
/// Resolution order (issue #302 — repo resolution must be git-aware):
/// 1. Walk up to the nearest `.git` entry. For a standard repo (`.git` is a
///    directory) that directory is the root.
/// 2. For a **linked worktree** (`.git` is a gitlink *file*), resolve the main
///    worktree via `git rev-parse --git-common-dir` and return it, so commands
///    find the parent repo's `.travsr/` instead of erroring `not initialized`.
/// 3. If no `.git` is found at all, fall back to the global registry: if `start`
///    lives under a registered repo root, return that root.
pub fn find_git_root(start: &Path) -> anyhow::Result<PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        let git_entry = current.join(".git");
        if git_entry.is_dir() {
            return Ok(current);
        }
        if git_entry.is_file() {
            // Linked worktree: `.git` is a gitlink file pointing at the main
            // repo's git dir. Resolve the main worktree so its `.travsr/` is used.
            return Ok(main_worktree_root(&current).unwrap_or(current));
        }
        if !current.pop() {
            // Not inside a git repository — last resort, consult the registry
            // in case `start` sits under a repo that was initialized elsewhere.
            if let Some(root) = registered_ancestor(start) {
                return Ok(root);
            }
            anyhow::bail!("not inside a git repository — run `git init` first, then `travsr init`");
        }
    }
}

/// Resolve the main-worktree root from inside a linked worktree.
///
/// Returns `None` (caller falls back to the worktree dir) when git is
/// unavailable, too old for `--path-format`, or the common dir does not point
/// at a real main worktree (e.g. a submodule gitlink under `.git/modules`).
fn main_worktree_root(worktree_dir: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(worktree_dir)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let common = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());
    // `--git-common-dir` is `<main>/.git`; its parent is the main worktree.
    let root = common.parent()?.to_path_buf();
    // Guard against submodule gitlinks (`.git/modules/<name>`): only accept a
    // directory that actually looks like a main worktree.
    if root.join(".git").is_dir() {
        Some(root)
    } else {
        None
    }
}

/// Find the deepest registered repo root that is an ancestor of (or equal to)
/// `start`. Registry values are `<root>/.travsr/graph.db` paths.
fn registered_ancestor(start: &Path) -> Option<PathBuf> {
    let repos = registry::all_repos().ok()?;
    let mut best: Option<PathBuf> = None;
    for db_path in repos.values() {
        // Strip `graph.db` then `.travsr` to recover the repo root.
        let Some(root) = db_path.parent().and_then(|p| p.parent()) else {
            continue;
        };
        if start.starts_with(root) {
            // Prefer the deepest (most specific) matching root.
            let deeper = best
                .as_ref()
                .map(|b| root.as_os_str().len() > b.as_os_str().len())
                .unwrap_or(true);
            if deeper {
                best = Some(root.to_path_buf());
            }
        }
    }
    best
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

    #[test]
    fn standard_repo_resolves_to_repo_root() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let root = find_git_root(tmp.path()).unwrap();
        assert_eq!(
            root.canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn subdir_walks_up_to_repo_root() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let sub = tmp.path().join("a/b/c");
        std::fs::create_dir_all(&sub).unwrap();
        let root = find_git_root(&sub).unwrap();
        assert_eq!(
            root.canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap()
        );
    }

    /// Issue #302 acceptance: a linked worktree must resolve to the main
    /// worktree (whose `.travsr/` holds the index), not the worktree dir.
    #[test]
    fn worktree_resolves_to_main_worktree() {
        if !git_available() {
            eprintln!("skipping: git not available");
            return;
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
        // `.git` in a worktree is a gitlink file, not a directory.
        assert!(wt.join(".git").is_file());

        let resolved = find_git_root(&wt).unwrap();
        assert_eq!(
            resolved.canonicalize().unwrap(),
            main.canonicalize().unwrap(),
            "worktree must resolve to the main repo root"
        );

        // Also from a subdirectory of the worktree.
        let wt_sub = wt.join("sub");
        std::fs::create_dir(&wt_sub).unwrap();
        let resolved_sub = find_git_root(&wt_sub).unwrap();
        assert_eq!(
            resolved_sub.canonicalize().unwrap(),
            main.canonicalize().unwrap()
        );
    }
}
