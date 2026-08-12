use std::path::{Path, PathBuf};

use anyhow::Context as _;

const TRAVSR_MARKER_SH: &str = "# installed by travsr \u{2014} do not edit this line";
#[cfg(windows)]
const TRAVSR_MARKER_CMD: &str = "@rem installed by travsr \u{2014} do not edit this line";

/// Hooks Travsr installs, and the git event each one covers.
///
/// #655 M2: `post-commit` alone leaves the graph describing the pre-pull tree
/// after a fast-forward `git pull`, because a fast-forward creates no commit
/// and therefore fires no `post-commit`. `git checkout` between branches has
/// the same shape. Both are ordinary daily operations, so "always fresh" was
/// only true for the subset of tree changes that happen to be commits.
const HOOKS: &[&str] = &["post-commit", "post-merge", "post-checkout"];

/// Absolute path of the binary currently running `travsr`, for embedding in the
/// installed git hooks so every event is reindexed by the SAME binary that ran
/// `travsr init` — not whatever bare `travsr` happens to be first on `PATH`.
///
/// #655 M1: git runs hooks with a minimal environment, and hooks fired from GUI
/// clients and IDEs routinely see a `PATH` without the user's shell additions,
/// so a bare-`travsr` hook failed with `exec: travsr: not found` and the graph
/// silently stopped tracking the repo. On a dogfooding machine the PATH
/// `travsr` is the npm-global Node wrapper (often a stale version) while the
/// daemon runs from a freshly built binary, so a bare name reindexed the fresh
/// graph with the wrong binary either way. `.git/hooks/*` is machine-local and
/// untracked, so an absolute path there is legitimate and never leaks into
/// version control. Re-running `travsr init` re-pins this to the current binary
/// (self-heal after a move/reinstall), and the generated script also falls back
/// to a `PATH` lookup at run time if the recorded path stops being executable.
/// Falls back to the bare name when the path cannot be resolved or is not valid
/// UTF-8, preserving the previous behaviour rather than failing the install.
fn installing_binary() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_owned))
        .unwrap_or_else(|| "travsr".to_string())
}

/// Single-quote a string for safe embedding in a POSIX `sh` command line, so a
/// binary path containing spaces or shell metacharacters is passed verbatim.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Double-quote a string for safe embedding in a Windows `.cmd` command line.
#[cfg(windows)]
fn cmd_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// `post-checkout` receives `<prev-head> <new-head> <branch-flag>`, where the
/// flag is 1 for a branch checkout and 0 for a file checkout. Reindexing on a
/// file checkout would fire on every `git checkout -- <path>`, so the generated
/// hook returns early unless the flag is 1.
fn branch_checkout_guard_sh(hook: &str) -> &'static str {
    if hook == "post-checkout" {
        "[ \"$3\" = \"1\" ] || exit 0\n"
    } else {
        ""
    }
}

fn hook_body(hook: &str, bin: &str) -> String {
    format!(
        r#"#!/bin/sh
# installed by travsr — do not edit this line
{guard}_travsr={exe}
[ -x "$_travsr" ] || _travsr="travsr"
exec "$_travsr" hook-run --from-hook
"#,
        guard = branch_checkout_guard_sh(hook),
        exe = sh_quote(bin),
    )
}

fn chain_hook_body(hook: &str, bin: &str) -> String {
    format!(
        r#"#!/bin/sh
# installed by travsr — do not edit this line
# chains the pre-existing hook that was renamed to {hook}.travsr-pre.bak
_dir="$(cd "$(dirname "$0")" && pwd)"
if [ -x "$_dir/{hook}.travsr-pre.bak" ]; then
  "$_dir/{hook}.travsr-pre.bak" "$@"
fi
{guard}_travsr={exe}
[ -x "$_travsr" ] || _travsr="travsr"
exec "$_travsr" hook-run --from-hook
"#,
        guard = branch_checkout_guard_sh(hook),
        exe = sh_quote(bin),
    )
}

/// `post-checkout` guard for cmd.exe, mirroring [`branch_checkout_guard_sh`].
#[cfg(windows)]
fn branch_checkout_guard_cmd(hook: &str) -> &'static str {
    if hook == "post-checkout" {
        "@if not \"%3\"==\"1\" exit /b 0\r\n"
    } else {
        ""
    }
}

/// Windows `.cmd` hook — CRLF line endings required for cmd.exe.
#[cfg(windows)]
fn cmd_hook_body(hook: &str, bin: &str) -> String {
    format!(
        "@echo off\r\n@rem installed by travsr \u{2014} do not edit this line\r\n{guard}{exe} hook-run --from-hook\r\n",
        guard = branch_checkout_guard_cmd(hook),
        exe = cmd_quote(bin),
    )
}

/// Windows chain `.cmd` hook — calls any pre-existing hook before running travsr.
#[cfg(windows)]
fn cmd_chain_hook_body(hook: &str, bin: &str) -> String {
    format!(
        "@echo off\r\n@rem installed by travsr \u{2014} do not edit this line\r\n@if exist \"%~dp0{hook}.travsr-pre.bak.cmd\" call \"%~dp0{hook}.travsr-pre.bak.cmd\" %*\r\n{guard}{exe} hook-run --from-hook\r\n",
        guard = branch_checkout_guard_cmd(hook),
        exe = cmd_quote(bin),
    )
}

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

/// Install the Travsr git hooks in the repo's hooks directory.
///
/// Installs every hook in [`HOOKS`], not `post-commit` alone: a fast-forward
/// `git pull` creates no commit and a branch checkout creates none either, so
/// commit-only coverage left the graph describing the previous tree (#655 M2).
///
/// For a standard repo the directory is `repo_root/.git/hooks`. For a **linked
/// worktree** `.git` is a gitlink *file*, so hooks live in the shared common
/// dir; the directory is resolved via git in both cases (see
/// [`resolve_hooks_dir`]).
///
/// On all platforms, writes POSIX shell hooks (which work with Git Bash on
/// Windows). On Windows, additionally writes `<hook>.cmd` so that git invoked
/// from plain cmd.exe or PowerShell also triggers them.
///
/// If a hook already exists that was NOT installed by Travsr, it is renamed to
/// `<hook>.travsr-pre.bak` (or `.bak.cmd`) and a chain script is written
/// instead so the existing hook continues to run. The backup name is per hook,
/// so installing three of them cannot make one clobber another's backup.
pub fn install_hook(repo_root: &Path) -> anyhow::Result<()> {
    let hooks_dir = resolve_hooks_dir(repo_root);
    std::fs::create_dir_all(&hooks_dir)
        .with_context(|| format!("creating hooks directory {}", hooks_dir.display()))?;

    // Pin the hooks to the binary that ran `travsr init` (self-heal: every init
    // re-pins to the current binary). See [`installing_binary`].
    let bin = installing_binary();

    for hook in HOOKS {
        install_one(&hooks_dir, hook, &bin)?;
    }

    Ok(())
}

/// Install a single hook, preserving any pre-existing script by backing it up
/// and chaining to it.
fn install_one(hooks_dir: &Path, hook: &str, bin: &str) -> anyhow::Result<()> {
    // ── POSIX shell hook (all platforms) ─────────────────────────────────────
    let hook_path = hooks_dir.join(hook);
    let bak_path = hooks_dir.join(format!("{hook}.travsr-pre.bak"));

    let script = if hook_path.exists() {
        let existing = std::fs::read_to_string(&hook_path)
            .with_context(|| format!("reading existing {hook} hook"))?;
        if existing.contains(TRAVSR_MARKER_SH) {
            hook_body(hook, bin)
        } else if bak_path.exists() {
            // L6: a backup already exists — the user may have manually restored their
            // original hook over ours. Don't silently overwrite the backup.
            tracing::info!(
                "{hook} hook modified since last install and {} already exists — \
                 overwriting hook only (not re-backing up)",
                bak_path.display()
            );
            chain_hook_body(hook, bin)
        } else {
            std::fs::rename(&hook_path, &bak_path)
                .with_context(|| format!("backing up existing {hook} hook"))?;
            tracing::info!("existing {hook} hook backed up to {}", bak_path.display());
            chain_hook_body(hook, bin)
        }
    } else {
        hook_body(hook, bin)
    };

    std::fs::write(&hook_path, script).with_context(|| format!("writing {hook} hook"))?;
    set_executable(&hook_path)?;
    tracing::info!("installed {hook} hook at {}", hook_path.display());

    // ── Windows .cmd hook (Windows only) ─────────────────────────────────────
    #[cfg(windows)]
    {
        let cmd_hook_path = hooks_dir.join(format!("{hook}.cmd"));
        let cmd_bak_path = hooks_dir.join(format!("{hook}.travsr-pre.bak.cmd"));

        let cmd_script = if cmd_hook_path.exists() {
            let existing = std::fs::read_to_string(&cmd_hook_path)
                .with_context(|| format!("reading existing {hook}.cmd hook"))?;
            if existing.contains(TRAVSR_MARKER_CMD) {
                cmd_hook_body(hook, bin)
            } else if cmd_bak_path.exists() {
                // L6 (#507): same guard as the POSIX branch above. The user may
                // have restored their own hook over ours; fs::rename on Windows
                // REPLACES the destination, so re-backing up here would destroy
                // the original backup.
                tracing::info!(
                    "{hook}.cmd hook modified since last install and {} already exists — \
                     overwriting hook only (not re-backing up)",
                    cmd_bak_path.display()
                );
                cmd_chain_hook_body(hook, bin)
            } else {
                std::fs::rename(&cmd_hook_path, &cmd_bak_path)
                    .with_context(|| format!("backing up existing {hook}.cmd hook"))?;
                tracing::info!(
                    "existing {hook}.cmd hook backed up to {}",
                    cmd_bak_path.display()
                );
                cmd_chain_hook_body(hook, bin)
            }
        } else {
            cmd_hook_body(hook, bin)
        };

        std::fs::write(&cmd_hook_path, cmd_script)
            .with_context(|| format!("writing {hook}.cmd hook"))?;
        tracing::info!("installed {hook}.cmd hook at {}", cmd_hook_path.display());
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

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn git(dir: &Path, args: &[&str]) -> bool {
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Issue #586 downstream: in a linked worktree `.git` is a gitlink *file*,
    /// so `<wt>/.git/hooks` does not exist. `install_hook` used to
    /// `create_dir_all(repo_root/.git/hooks)` and fail with
    /// `Not a directory (os error 20)`, leaving `init` half done. Git hooks are
    /// a common resource, so the hook must land in the shared common hooks dir
    /// (`<main>/.git/hooks`) and installing from the worktree must succeed.
    #[test]
    fn install_hook_from_worktree_uses_common_hooks_dir() {
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

        // A standard repo resolves to its own `.git/hooks`.
        assert_eq!(
            resolve_hooks_dir(&main).canonicalize().unwrap(),
            main.join(".git/hooks").canonicalize().unwrap()
        );
        // A worktree resolves to the *shared* common hooks dir, never
        // `<wt>/.git/hooks` (which does not exist).
        assert_eq!(
            resolve_hooks_dir(&wt).canonicalize().unwrap(),
            main.join(".git/hooks").canonicalize().unwrap()
        );

        // The bug: this used to error with `Not a directory (os error 20)`.
        install_hook(&wt).unwrap();
        let hook = main.join(".git/hooks/post-commit");
        assert!(
            hook.exists(),
            "hook must land in the shared common hooks dir"
        );
        assert!(std::fs::read_to_string(&hook)
            .unwrap()
            .contains(TRAVSR_MARKER_SH));
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

    // ── #655 M1/M2: what the hook invokes, and which events it covers ────────

    #[test]
    fn every_git_event_that_moves_the_tree_gets_a_hook() {
        // post-commit alone misses a fast-forward pull, which creates no
        // commit, and a branch checkout. Both change the tree.
        let tmp = make_fake_repo();
        install_hook(tmp.path()).unwrap();
        let hooks = tmp.path().join(".git/hooks");

        for hook in HOOKS {
            assert!(hooks.join(hook).exists(), "{hook} was not installed");
        }
        assert!(
            HOOKS.contains(&"post-merge") && HOOKS.contains(&"post-checkout"),
            "the merge and checkout events are the ones #655 M2 is about"
        );
    }

    #[test]
    fn the_hook_names_an_absolute_binary_rather_than_resolving_from_path() {
        // A bare `travsr` is resolved from PATH, and git runs hooks with a
        // minimal PATH, so the old body failed with `exec: travsr: not found`
        // and the graph silently stopped tracking the repo.
        let tmp = make_fake_repo();
        install_hook(tmp.path()).unwrap();
        let body = std::fs::read_to_string(tmp.path().join(".git/hooks/post-commit")).unwrap();

        let exe = std::env::current_exe().unwrap();
        assert!(
            body.contains(&exe.display().to_string()),
            "hook should invoke the resolved binary, got:\n{body}"
        );
        assert!(
            !body.contains("exec travsr hook-run"),
            "hook should not invoke a bare `travsr`, got:\n{body}"
        );
    }

    #[test]
    fn the_hook_still_works_if_the_recorded_binary_moves() {
        // Pinning an absolute path trades one failure mode for another: the
        // binary can be moved, upgraded, or uninstalled. The script degrades
        // to PATH lookup rather than breaking outright.
        let tmp = make_fake_repo();
        install_hook(tmp.path()).unwrap();
        let body = std::fs::read_to_string(tmp.path().join(".git/hooks/post-commit")).unwrap();

        assert!(
            body.contains(r#"[ -x "$_travsr" ] || _travsr="travsr""#),
            "expected a PATH fallback, got:\n{body}"
        );
    }

    #[test]
    fn post_checkout_ignores_file_checkouts() {
        // post-checkout also fires for `git checkout -- <path>`, where $3 is 0.
        // Reindexing there would fire on an operation that restores a single
        // file, so the hook exits before doing any work.
        let tmp = make_fake_repo();
        install_hook(tmp.path()).unwrap();
        let hooks = tmp.path().join(".git/hooks");

        let checkout = std::fs::read_to_string(hooks.join("post-checkout")).unwrap();
        assert!(
            checkout.contains(r#"[ "$3" = "1" ] || exit 0"#),
            "post-checkout must skip file checkouts, got:\n{checkout}"
        );

        // The guard belongs only to post-checkout; the others take no such args.
        for hook in ["post-commit", "post-merge"] {
            let body = std::fs::read_to_string(hooks.join(hook)).unwrap();
            assert!(
                !body.contains(r#""$3""#),
                "{hook} must not carry the post-checkout guard, got:\n{body}"
            );
        }
    }

    #[test]
    fn a_foreign_hook_is_backed_up_and_chained_under_its_own_name() {
        // The backup name used to be hardcoded to post-commit. With three
        // hooks installed, a shared name would have one hook overwrite
        // another's backup and lose the user's script.
        let tmp = make_fake_repo();
        let hooks = tmp.path().join(".git/hooks");
        std::fs::write(hooks.join("post-merge"), "#!/bin/sh\necho mine\n").unwrap();

        install_hook(tmp.path()).unwrap();

        let bak = hooks.join("post-merge.travsr-pre.bak");
        assert!(bak.exists(), "the pre-existing hook must be preserved");
        assert!(std::fs::read_to_string(&bak).unwrap().contains("echo mine"));

        let body = std::fs::read_to_string(hooks.join("post-merge")).unwrap();
        assert!(
            body.contains("post-merge.travsr-pre.bak"),
            "the chain must call this hook's own backup, got:\n{body}"
        );
        assert!(
            !body.contains("post-commit.travsr-pre.bak"),
            "the chain must not reference another hook's backup, got:\n{body}"
        );
    }
}
