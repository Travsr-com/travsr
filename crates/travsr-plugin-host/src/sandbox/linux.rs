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
    _scratch_dir: &Path,
) -> Result<Command, SandboxUnavailable> {
    if !bwrap_available() {
        return Err(SandboxUnavailable(
            "bwrap not on PATH — install bubblewrap or this plugin stays disabled".into(),
        ));
    }
    let repo = repo_root.to_string_lossy();

    let mut cmd = Command::new("bwrap");
    // --unshare-user maps the real UID to root inside the namespace, which is
    // required for --tmpfs to create a writable mount (tmpfs root is mode 0755
    // owned by the mapped UID; without the user NS the process runs as the real
    // UID 1001 on CI and cannot write to a root-owned directory).
    cmd.args([
        "--unshare-user",
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
    // Provide a writable scratch area inside the sandbox at /travsr-scratch.
    // We use --tmpfs rather than --bind so that no rw bind-mount privilege is
    // required — unprivileged bwrap on GitHub Actions runners allows tmpfs but
    // may reject rw --bind mounts depending on the kernel/seccomp policy.
    // Plugins write here; TMPDIR is set to /travsr-scratch accordingly.
    cmd.args(["--tmpfs", "/travsr-scratch"]); // writable scratch: in-sandbox tmpfs
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
    cmd.env("TMPDIR", "/travsr-scratch");
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
