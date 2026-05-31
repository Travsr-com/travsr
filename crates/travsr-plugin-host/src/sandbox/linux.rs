//! Linux sandbox (bubblewrap + optional Landlock). Fail-closed per ADR-017 Rule 2.
use crate::sandbox::policy::SandboxUnavailable;
use std::path::Path;
use std::process::Command;

/// Permitted env vars (ADR-017 Rule 1). TMPDIR set by caller to scratch dir.
pub const ENV_ALLOWLIST: &[&str] = &["PATH", "LANG", "LC_ALL"];

#[cfg(target_os = "linux")]
pub fn build_sandboxed_command(
    program: &str,
    args: &[&str],
    repo_root: &Path,
    scratch_dir: &Path,
) -> Result<Command, SandboxUnavailable> {
    if !bwrap_available() {
        return Err(SandboxUnavailable(
            "bwrap not on PATH — install bubblewrap or this plugin stays disabled".into(),
        ));
    }
    let repo = repo_root.to_string_lossy();
    let scratch = scratch_dir.to_string_lossy();

    let mut cmd = Command::new("bwrap");
    cmd.args([
        "--unshare-net",
        "--unshare-pid",
        "--unshare-uts",
        "--unshare-ipc",
    ]);
    for path in ["/usr", "/bin", "/sbin", "/lib", "/lib64"] {
        cmd.args(["--ro-bind-try", path, path]);
    }
    for path in [
        "/etc/alternatives",
        "/etc/localtime",
        "/etc/ld.so.cache",
        "/etc/ld.so.conf",
    ] {
        cmd.args(["--ro-bind-try", path, path]);
    }
    cmd.args(["--proc", "/proc", "--dev", "/dev"]);
    // Create the scratch path in the sandbox overlay, then bind-mount the host
    // scratch dir rw over it.  We first ensure /tmp exists in the overlay with
    // --tmpfs so that deeper paths like /tmp/tmp.XYZabc can be created by the
    // subsequent --dir.  This avoids the ro-vs-rw conflict from
    // --ro-bind-try /tmp + --bind scratch and also works on bwrap versions
    // that require a parent mount point before --dir can create a child path.
    cmd.args(["--tmpfs", "/tmp"]);
    cmd.args(["--dir", scratch.as_ref()]);
    cmd.args(["--bind", scratch.as_ref(), scratch.as_ref()]); // scratch: rw
    cmd.args(["--ro-bind", repo.as_ref(), repo.as_ref()]); // repo: ro
    cmd.args(["--die-with-parent", "--"]);
    // Resource caps (ADR-017 Rule 1): 4 GiB virtual memory + 300s CPU via ulimit.
    let quoted_args = args
        .iter()
        .map(|a| format!("'{}'", a.replace('\'', r"'\''")))
        .collect::<Vec<_>>()
        .join(" ");
    let inner = format!(
        "ulimit -v 4194304 2>/dev/null; ulimit -t 300 2>/dev/null; exec '{}' {}",
        program.replace('\'', r"'\''"),
        quoted_args
    );
    cmd.args(["sh", "-c", &inner]);
    cmd.env_clear();
    for key in ENV_ALLOWLIST {
        if let Ok(val) = std::env::var(key) {
            cmd.env(key, val);
        }
    }
    cmd.env("TMPDIR", scratch.as_ref());
    Ok(cmd)
}

#[cfg(target_os = "linux")]
fn bwrap_available() -> bool {
    Command::new("bwrap")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

#[cfg(not(target_os = "linux"))]
pub fn build_sandboxed_command(
    _p: &str,
    _a: &[&str],
    _r: &Path,
    _s: &Path,
) -> Result<Command, SandboxUnavailable> {
    Err(SandboxUnavailable(
        "Linux sandbox not available on this platform".into(),
    ))
}
