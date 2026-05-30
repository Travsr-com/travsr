//! Linux sandbox (bubblewrap + optional Landlock). Fail-closed per ADR-017 Rule 2.
use std::path::Path;
use std::process::Command;
use crate::sandbox::policy::SandboxUnavailable;

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
    cmd.args(["--unshare-net", "--unshare-pid", "--unshare-uts", "--unshare-ipc"]);
    for path in ["/usr", "/bin", "/sbin", "/lib", "/lib64"] {
        cmd.args(["--ro-bind-try", path, path]);
    }
    for path in ["/etc/alternatives", "/etc/localtime", "/etc/ld.so.cache", "/etc/ld.so.conf"] {
        cmd.args(["--ro-bind-try", path, path]);
    }
    cmd.args(["--proc", "/proc", "--dev", "/dev"]);
    cmd.args(["--bind", scratch.as_ref(), scratch.as_ref()]);   // scratch: rw
    cmd.args(["--ro-bind", repo.as_ref(), repo.as_ref()]);      // repo: ro
    cmd.args(["--die-with-parent", "--"]);
    cmd.arg(program).args(args);
    cmd.env_clear();
    for key in ENV_ALLOWLIST {
        if let Ok(val) = std::env::var(key) { cmd.env(key, val); }
    }
    cmd.env("TMPDIR", scratch.as_ref());
    Ok(cmd)
}

#[cfg(target_os = "linux")]
fn bwrap_available() -> bool {
    Command::new("bwrap").arg("--version")
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
        .status().is_ok()
}

#[cfg(not(target_os = "linux"))]
pub fn build_sandboxed_command(
    _p: &str, _a: &[&str], _r: &Path, _s: &Path,
) -> Result<Command, SandboxUnavailable> {
    Err(SandboxUnavailable("Linux sandbox not available on this platform".into()))
}
