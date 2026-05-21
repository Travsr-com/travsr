use std::path::Path;

use anyhow::Context as _;

const TRAVSR_MARKER: &str = "# installed by travsr — do not edit this line";

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

/// Install the Travsr `post-commit` hook in `repo_root/.git/hooks/`.
///
/// If a hook already exists that was NOT installed by Travsr, it is renamed to
/// `post-commit.travsr-pre.bak` and a chain script is written instead so the
/// existing hook continues to run.
pub fn install_hook(repo_root: &Path) -> anyhow::Result<()> {
    let hooks_dir = repo_root.join(".git/hooks");
    std::fs::create_dir_all(&hooks_dir).context("creating .git/hooks directory")?;

    let hook_path = hooks_dir.join("post-commit");
    let bak_path = hooks_dir.join("post-commit.travsr-pre.bak");

    let script = if hook_path.exists() {
        let existing = std::fs::read_to_string(&hook_path).context("reading existing hook")?;
        if existing.contains(TRAVSR_MARKER) {
            // Already ours — overwrite with fresh copy.
            HOOK_BODY
        } else {
            // Foreign hook — back it up and chain.
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
