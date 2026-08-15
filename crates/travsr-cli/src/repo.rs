use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use travsr_store::registry;

/// How a linked worktree's gitlink `.git` file is resolved. This is the only
/// behavioral difference between the read and write repo-root resolvers, split
/// out so the two intents cannot silently drift back together (issues #302/#586).
#[derive(Clone, Copy)]
enum WorktreeMode {
    /// Read path: prefer the worktree's own `.travsr/` if it has one, otherwise
    /// redirect to the main worktree so read commands find the parent repo's
    /// index instead of erroring `not initialized` (issue #302).
    PreferLocalElseMain,
    /// Write path: always stay in the linked worktree so `init`/`fsck`/`embed`/
    /// `daemon`/`hook-run` create and mutate the worktree's own `.travsr/`,
    /// never the main worktree's — silently writing a different checkout's index
    /// is the bug (issue #586).
    StayInWorktree,
}

/// Resolve the Travsr repo root for a **read** command (`ask`, `graph`,
/// `references`, `pattern`, `status`, `explain`, `config`, MCP `serve`, …) — the
/// directory whose `.travsr/` holds the index to query.
///
/// Resolution order (issue #302 — repo resolution must be git-aware):
/// 1. Walk up to the nearest `.git` entry. For a standard repo (`.git` is a
///    directory) that directory is the root.
/// 2. For a **linked worktree** (`.git` is a gitlink *file*): if the worktree
///    has its own `.travsr/graph.db` (created by a write command per issue #586)
///    use it; otherwise resolve the main worktree via
///    `git rev-parse --git-common-dir` so reads find the parent repo's index
///    instead of erroring `not initialized` (issue #302).
/// 3. If no `.git` is found at all, fall back to the global registry: if `start`
///    lives under a registered repo root, return that root.
pub fn find_git_root(start: &Path) -> anyhow::Result<PathBuf> {
    resolve_repo_root(start, WorktreeMode::PreferLocalElseMain)
}

/// Resolve the repo root for a **write** command (`init`, `fsck`, `embed`
/// reindex/switch/init/reconfigure/gc/calibrate, `daemon`, `hook-run`).
///
/// Identical to [`find_git_root`] except a linked worktree always resolves to
/// the worktree's **own** directory, never the main worktree. Write commands
/// mutate the resolved `.travsr/`; redirecting them to the main worktree
/// silently mutates a different checkout at a different commit (issue #586).
pub fn find_git_root_for_write(start: &Path) -> anyhow::Result<PathBuf> {
    resolve_repo_root(start, WorktreeMode::StayInWorktree)
}

fn resolve_repo_root(start: &Path, mode: WorktreeMode) -> anyhow::Result<PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        let git_entry = current.join(".git");
        if git_entry.is_dir() {
            return Ok(current);
        }
        if git_entry.is_file() {
            // Linked worktree: `.git` is a gitlink file pointing at the main
            // repo's git dir.
            return Ok(match mode {
                WorktreeMode::PreferLocalElseMain => {
                    if current.join(".travsr/graph.db").is_file() {
                        current
                    } else {
                        main_worktree_root(&current).unwrap_or(current)
                    }
                }
                WorktreeMode::StayInWorktree => current,
            });
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
    // Bounded (#717 triage): this runs while resolving the repo root, before
    // anything is printed, so a git call that never returns here is a CLI that
    // hangs with no output at all. `None` already means "fall back to the
    // worktree dir", which is the right answer for a query that did not land.
    // `worktree_dir` goes through as a raw `OsStr`, never `to_string_lossy`: a
    // path with non-UTF-8 bytes is legal, and mangling it into U+FFFD would make
    // git fail to resolve a worktree that exists, silently dropping back to the
    // linked worktree as the repo root (which has no index) and reporting "not
    // initialized". That is the #302 regression this function exists to prevent.
    let common = PathBuf::from(crate::git_bounded::git_stdout_bounded(
        None,
        [
            OsStr::new("-C"),
            worktree_dir.as_os_str(),
            OsStr::new("rev-parse"),
            OsStr::new("--path-format=absolute"),
            OsStr::new("--git-common-dir"),
        ],
    )?);
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

    /// Build `<tmp>/main` (a committed repo) and a linked worktree `<tmp>/wt`.
    /// Returns `(tmpdir, main, wt)`. Skips (returns `None`) when git is absent.
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

    /// Issue #586 acceptance: the **write** resolver must stay in the worktree,
    /// never redirect to the main worktree, so `init`/`fsck`/`embed` create and
    /// mutate the worktree's own `.travsr/`. Companion to
    /// [`worktree_resolves_to_main_worktree`] — the read and write intents must
    /// not silently collapse back into one behavior.
    #[test]
    fn worktree_write_resolver_stays_in_worktree() {
        let Some((_tmp, main, wt)) = main_and_worktree() else {
            return;
        };

        // Read resolver (no local index yet) redirects to main — issue #302.
        let read = find_git_root(&wt).unwrap();
        assert_eq!(read.canonicalize().unwrap(), main.canonicalize().unwrap());

        // Write resolver must resolve to the worktree itself — issue #586.
        let write = find_git_root_for_write(&wt).unwrap();
        assert_eq!(
            write.canonicalize().unwrap(),
            wt.canonicalize().unwrap(),
            "write resolver must not redirect a worktree to the main repo root"
        );

        // And from a subdirectory of the worktree.
        let wt_sub = wt.join("sub");
        std::fs::create_dir(&wt_sub).unwrap();
        let write_sub = find_git_root_for_write(&wt_sub).unwrap();
        assert_eq!(
            write_sub.canonicalize().unwrap(),
            wt.canonicalize().unwrap()
        );
    }

    /// Once a worktree has its own index (a write command created
    /// `<wt>/.travsr/graph.db`), the **read** resolver must prefer it over the
    /// main worktree — otherwise the read/write split would leave reads pointed
    /// at a different checkout than the one `init` just indexed (issue #586).
    #[test]
    fn worktree_read_resolver_prefers_local_index() {
        let Some((_tmp, _main, wt)) = main_and_worktree() else {
            return;
        };

        // Simulate the worktree having been `travsr init`-ed under the fix.
        std::fs::create_dir_all(wt.join(".travsr")).unwrap();
        std::fs::write(wt.join(".travsr/graph.db"), b"").unwrap();

        let read = find_git_root(&wt).unwrap();
        assert_eq!(
            read.canonicalize().unwrap(),
            wt.canonicalize().unwrap(),
            "read resolver must use the worktree's own index once it has one"
        );
    }
}
