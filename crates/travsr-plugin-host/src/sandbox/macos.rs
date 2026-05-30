//! macOS sandbox (sandbox-exec / Seatbelt). Fail-closed per ADR-017 Rule 2.
#[cfg(target_os = "macos")]
use crate::sandbox::linux::ENV_ALLOWLIST;
use crate::sandbox::policy::SandboxUnavailable;
use std::path::Path;
use std::process::Command;

#[cfg(target_os = "macos")]
pub fn build_sandboxed_command(
    program: &str,
    args: &[&str],
    repo_root: &Path,
    scratch_dir: &Path,
) -> Result<Command, SandboxUnavailable> {
    let repo = repo_root.to_string_lossy();
    let scratch = scratch_dir.to_string_lossy();
    // ADR-017 Rule 1: deny all by default; allow read only to system paths,
    // repo root (read-only), and scratch (read-write). No network access.
    let profile = format!(
        r#"(version 1)
(deny default)
(deny network*)
(allow process-fork process-exec* signal)
(allow file-read*
    (subpath "/usr")
    (subpath "/bin")
    (subpath "/sbin")
    (subpath "/lib")
    (subpath "/Library/Developer")
    (subpath "/System")
    (subpath "/private/etc")
    (subpath "{repo}")
    (subpath "{scratch}"))
(allow file-write* (subpath "{scratch}"))
(allow mach-lookup
    (global-name "com.apple.dyld")
    (global-name "com.apple.logd")
    (global-name "com.apple.system.logger"))
(allow sysctl-read)
"#,
        repo = repo,
        scratch = scratch,
    );
    let mut cmd = Command::new("sandbox-exec");
    cmd.args(["-p", &profile, "--"]).arg(program).args(args);
    cmd.env_clear();
    for key in ENV_ALLOWLIST {
        if let Ok(val) = std::env::var(key) {
            cmd.env(key, val);
        }
    }
    cmd.env("TMPDIR", scratch.as_ref());
    Ok(cmd)
}

#[cfg(not(target_os = "macos"))]
pub fn build_sandboxed_command(
    _p: &str,
    _a: &[&str],
    _r: &Path,
    _s: &Path,
) -> Result<Command, SandboxUnavailable> {
    Err(SandboxUnavailable(
        "macOS sandbox not available on this platform".into(),
    ))
}
