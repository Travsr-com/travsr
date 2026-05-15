use std::path::Path;

/// Start a file-system watcher on `repo_root` and trigger re-indexing on
/// non-git edits. Full async event loop wires in Sprint 3 when the daemon
/// acquires its Tokio runtime.
pub fn watch(_repo_root: &Path) -> anyhow::Result<()> {
    Ok(())
}
