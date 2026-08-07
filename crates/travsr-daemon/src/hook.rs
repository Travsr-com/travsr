use std::path::{Path, PathBuf};

use anyhow::Context as _;

const TRAVSR_MARKER_SH: &str = "# installed by travsr \u{2014} do not edit this line";
#[cfg(windows)]
const TRAVSR_MARKER_CMD: &str = "@rem installed by travsr \u{2014} do not edit this line";

const HOOK_BODY: &str = r#"#!/bin/sh
# installed by travsr — do not edit this line
exec travsr hook-run --from-hook
"#;

const CHAIN_HOOK_BODY: &str = r#"#!/bin/sh
# installed by travsr — do not edit this line
# chains the pre-existing hook that was renamed to post-commit.travsr-pre.bak
_dir="$(cd "$(dirname "$0")" && pwd)"
if [ -x "$_dir/post-commit.travsr-pre.bak" ]; then
  "$_dir/post-commit.travsr-pre.bak"
fi
exec travsr hook-run --from-hook
"#;

/// Windows `.cmd` hook — CRLF line endings required for cmd.exe.
#[cfg(windows)]
const CMD_HOOK_BODY: &str =
    "@echo off\r\n@rem installed by travsr \u{2014} do not edit this line\r\ntravsr hook-run --from-hook\r\n";

/// Windows chain `.cmd` hook — calls any pre-existing hook before running travsr.
#[cfg(windows)]
const CMD_CHAIN_HOOK_BODY: &str =
    "@echo off\r\n@rem installed by travsr \u{2014} do not edit this line\r\n@if exist \"%~dp0post-commit.travsr-pre.bak.cmd\" call \"%~dp0post-commit.travsr-pre.bak.cmd\"\r\ntravsr hook-run --from-hook\r\n";

/// Install the Travsr `post-commit` hook in the repo's git hooks directory.
///
/// For a standard repo this is `repo_root/.git/hooks`. For a **linked worktree**
/// `.git` is a gitlink *file*, so hooks live in the shared common dir; the
/// directory is resolved via git in both cases (see [`resolve_hooks_dir`]).
///
/// On all platforms, writes a POSIX shell `post-commit` (works with Git Bash
/// on Windows). On Windows, additionally writes `post-commit.cmd` so that git
/// invoked from plain cmd.exe or PowerShell also triggers the hook.
///
/// If a hook already exists that was NOT installed by Travsr, it is renamed to
/// `post-commit.travsr-pre.bak` (or `.bak.cmd`) and a chain script is written
/// instead so the existing hook continues to run.
/// Resolve the git hooks directory for `repo_root`.
///
/// Git hooks are a *common* resource shared across a repo's worktrees, stored
/// under the main git dir. For a standard repo that is `<repo_root>/.git/hooks`;
/// for a linked worktree (`.git` is a gitlink *file*, so `.git/hooks` does not
/// exist) it is `<main>/.git/hooks`. `git rev-parse --git-path hooks` returns
/// the right directory in both cases — relative (`.git/hooks`) for a standard
/// repo, absolute for a worktree — which we resolve against `repo_root`
/// (joining an absolute path replaces, a relative path appends). Falls back to
/// `<repo_root>/.git/hooks` when git is unavailable (issue #586).
fn resolve_hooks_dir(repo_root: &Path) -> PathBuf {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["rev-parse", "--git-path", "hooks"])
        .output();
    if let Ok(out) = out {
        if out.status.success() {
            if let Ok(s) = String::from_utf8(out.stdout) {
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    return repo_root.join(trimmed);
                }
            }
        }
    }
    repo_root.join(".git/hooks")
}

pub fn install_hook(repo_root: &Path) -> anyhow::Result<()> {
    let hooks_dir = resolve_hooks_dir(repo_root);
    std::fs::create_dir_all(&hooks_dir)
        .with_context(|| format!("creating hooks directory {}", hooks_dir.display()))?;

    // ── POSIX shell hook (all platforms) ─────────────────────────────────────
    let hook_path = hooks_dir.join("post-commit");
    let bak_path = hooks_dir.join("post-commit.travsr-pre.bak");

    let script = if hook_path.exists() {
        let existing = std::fs::read_to_string(&hook_path).context("reading existing hook")?;
        if existing.contains(TRAVSR_MARKER_SH) {
            HOOK_BODY
        } else if bak_path.exists() {
            // L6: a backup already exists — the user may have manually restored their
            // original hook over ours. Don't silently overwrite the backup.
            tracing::info!(
                "post-commit hook modified since last install and {} already exists — \
                 overwriting hook only (not re-backing up)",
                bak_path.display()
            );
            CHAIN_HOOK_BODY
        } else {
            std::fs::rename(&hook_path, &bak_path).context("backing up existing hook")?;
            tracing::info!(
                "existing post-commit hook backed up to {}",
                bak_path.display()
            );
            CHAIN_HOOK_BODY
        }
    } else {
        HOOK_BODY
    };

    std::fs::write(&hook_path, script).context("writing post-commit hook")?;
    set_executable(&hook_path)?;
    tracing::info!("installed post-commit hook at {}", hook_path.display());

    // ── Windows .cmd hook (Windows only) ─────────────────────────────────────
    #[cfg(windows)]
    {
        let cmd_hook_path = hooks_dir.join("post-commit.cmd");
        let cmd_bak_path = hooks_dir.join("post-commit.travsr-pre.bak.cmd");

        let cmd_script = if cmd_hook_path.exists() {
            let existing =
                std::fs::read_to_string(&cmd_hook_path).context("reading existing cmd hook")?;
            if existing.contains(TRAVSR_MARKER_CMD) {
                CMD_HOOK_BODY
            } else {
                std::fs::rename(&cmd_hook_path, &cmd_bak_path)
                    .context("backing up existing cmd hook")?;
                tracing::info!(
                    "existing post-commit.cmd hook backed up to {}",
                    cmd_bak_path.display()
                );
                CMD_CHAIN_HOOK_BODY
            }
        } else {
            CMD_HOOK_BODY
        };

        std::fs::write(&cmd_hook_path, cmd_script).context("writing post-commit.cmd hook")?;
        tracing::info!(
            "installed post-commit.cmd hook at {}",
            cmd_hook_path.display()
        );
    }

    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .context("reading hook permissions")?
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).context("setting hook permissions")?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

/// Return the absolute paths of files changed in the current HEAD commit by
/// invoking `git diff-tree` via [`std::process::Command`]. Never goes through
/// a shell, so filenames containing spaces, semicolons, or shell metacharacters
/// are handled safely.
///
/// `--diff-merges=first-parent` is required so merge commits report the files
/// changed on the primary parent line. `git diff-tree HEAD` alone emits nothing
/// on merge commits, and `--first-parent` alone is a history-walking flag that
/// does not affect diff output. Requires git 2.31+.
pub fn changed_files_from_git(repo_root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let output = std::process::Command::new("git")
        .args([
            "diff-tree",
            "--root",
            "--no-commit-id",
            "-r",
            "--name-only",
            "--diff-merges=first-parent",
            "HEAD",
        ])
        .current_dir(repo_root)
        .output()
        .context("running git diff-tree in hook-run")?;

    if !output.status.success() {
        return Ok(Vec::new());
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|p| repo_root.join(p))
        .collect())
}

/// Every file git tracks in `repo_root`, as absolute paths.
///
/// Used as the ReindexCommit fallback (#405): when [`changed_files_from_git`]
/// cannot resolve the commit's diff, reindexing the full tracked set keeps the
/// graph correct. `reindex_files` is hash-delta gated, so on an already-fresh
/// graph this pass only re-hashes files and writes nothing. Errors when git
/// itself cannot be spawned, so the caller can refuse to stamp freshness.
pub fn tracked_files_from_git(repo_root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let output = std::process::Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(repo_root)
        .output()
        .context("running git ls-files in reindex fallback")?;

    if !output.status.success() {
        anyhow::bail!(
            "git ls-files exited with status {}",
            output.status.code().unwrap_or(-1)
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .split('\0')
        .filter(|l| !l.is_empty())
        .map(|p| repo_root.join(p))
        .collect())
}

/// Attempt to fire-and-forget a reindex request to the running daemon.
///
/// Returns `true` if the daemon received the message; `false` if no daemon is
/// running or the write fails. The hook never reads a response — the daemon
/// processes the reindex asynchronously after the hook exits.
///
/// Cross-platform: Unix uses a domain socket; Windows uses a Named Pipe
/// (`\\.\pipe\travsr-<hex>`). Returns `false` on unsupported platforms.
///
/// # Panics
/// Never — hook must never panic.
pub fn try_dispatch_to_daemon(repo_root: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::time::Duration;
        use travsr_ipc::{ControlAddr, ControlMessage};

        let sha = std::process::Command::new("git")
            .args([
                "-C",
                &repo_root.to_string_lossy(),
                "rev-parse",
                "--short",
                "HEAD",
            ])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();

        let Ok(line) = serde_json::to_string(&ControlMessage::ReindexCommit { sha }) else {
            return false;
        };

        let addr = ControlAddr::for_repo(repo_root);
        let travsr_dir = repo_root.join(".travsr");
        let sock_path = addr.socket_path(&travsr_dir);
        if !sock_path.exists() {
            return false;
        }
        let Ok(mut conn) = std::os::unix::net::UnixStream::connect(&sock_path) else {
            return false;
        };
        let _ = conn.set_write_timeout(Some(Duration::from_millis(50)));
        writeln!(conn, "{line}").is_ok()
    }
    #[cfg(windows)]
    {
        use travsr_ipc::windows::NamedPipeTransport;
        use travsr_ipc::{ControlAddr, ControlMessage, ControlTransport as _};

        let sha = std::process::Command::new("git")
            .args([
                "-C",
                &repo_root.to_string_lossy(),
                "rev-parse",
                "--short",
                "HEAD",
            ])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();

        let addr = ControlAddr::for_repo(repo_root);
        let Ok(mut transport) = NamedPipeTransport::connect(&addr) else {
            return false;
        };
        transport
            .send_request(&ControlMessage::ReindexCommit { sha })
            .is_ok()
    }

    #[cfg(all(not(unix), not(windows)))]
    {
        let _ = repo_root;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_fake_repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git/hooks")).unwrap();
        tmp
    }

    #[cfg(windows)]
    #[test]
    fn install_creates_both_scripts() {
        let tmp = make_fake_repo();
        install_hook(tmp.path()).unwrap();
        let hooks = tmp.path().join(".git/hooks");

        let sh = std::fs::read_to_string(hooks.join("post-commit")).unwrap();
        assert!(sh.contains(TRAVSR_MARKER_SH));

        let cmd = std::fs::read_to_string(hooks.join("post-commit.cmd")).unwrap();
        assert!(cmd.contains(TRAVSR_MARKER_CMD));
    }

    #[cfg(unix)]
    #[test]
    fn install_creates_only_sh() {
        let tmp = make_fake_repo();
        install_hook(tmp.path()).unwrap();
        let hooks = tmp.path().join(".git/hooks");

        assert!(hooks.join("post-commit").exists());
        assert!(!hooks.join("post-commit.cmd").exists());
    }

    #[cfg(windows)]
    #[test]
    fn foreign_cmd_hook_is_backed_up() {
        let tmp = make_fake_repo();
        let hooks = tmp.path().join(".git/hooks");

        std::fs::write(
            hooks.join("post-commit.cmd"),
            "@echo off\r\necho foreign\r\n",
        )
        .unwrap();

        install_hook(tmp.path()).unwrap();

        assert!(hooks.join("post-commit.travsr-pre.bak.cmd").exists());
        let body = std::fs::read_to_string(hooks.join("post-commit.cmd")).unwrap();
        assert!(body.contains(TRAVSR_MARKER_CMD));
        assert!(body.contains("post-commit.travsr-pre.bak.cmd"));
    }

    #[cfg(windows)]
    #[test]
    fn second_install_detects_ownership() {
        let tmp = make_fake_repo();
        install_hook(tmp.path()).unwrap();
        install_hook(tmp.path()).unwrap();

        let hooks = tmp.path().join(".git/hooks");
        assert!(!hooks.join("post-commit.travsr-pre.bak.cmd").exists());
        let body = std::fs::read_to_string(hooks.join("post-commit.cmd")).unwrap();
        assert!(body.contains(TRAVSR_MARKER_CMD));
    }

    #[cfg(windows)]
    #[test]
    fn chain_cmd_hook_invokes_backed_up_hook() {
        let tmp = make_fake_repo();
        let hooks = tmp.path().join(".git/hooks");
        let sentinel = hooks.join("sentinel.txt");

        // Foreign hook: creates a sentinel file in the same directory.
        std::fs::write(
            hooks.join("post-commit.cmd"),
            "@echo off\r\necho triggered > \"%~dp0sentinel.txt\"\r\n",
        )
        .unwrap();

        install_hook(tmp.path()).unwrap();

        // Run the chain script. `travsr hook-run` may fail (not in PATH) but
        // the backed-up hook must run first and create the sentinel.
        let _ = std::process::Command::new("cmd.exe")
            .args(["/c", hooks.join("post-commit.cmd").to_str().unwrap()])
            .status();

        assert!(
            sentinel.exists(),
            "backed-up cmd hook should have created sentinel.txt"
        );
    }
}
