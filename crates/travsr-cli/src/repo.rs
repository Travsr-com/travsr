use std::path::{Path, PathBuf};

/// Walk up the directory tree from `start` to find the nearest `.git` entry.
/// Supports both standard repos and worktrees (where `.git` is a file).
pub fn find_git_root(start: &Path) -> anyhow::Result<PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        if current.join(".git").exists() {
            return Ok(current);
        }
        if !current.pop() {
            anyhow::bail!("not inside a git repository — run `git init` first, then `travsr init`");
        }
    }
}
