//! macOS sandbox (sandbox-exec / Seatbelt). Fail-closed per ADR-017 Rule 2.
#[cfg(target_os = "macos")]
use crate::sandbox::linux::ENV_ALLOWLIST;
use crate::sandbox::policy::{SandboxPolicy, SandboxUnavailable};
use std::path::Path;
use std::process::Command;

#[cfg(target_os = "macos")]
pub fn build_sandboxed_command(
    program: &str,
    args: &[&str],
    repo_root: &Path,
    scratch_dir: &Path,
    policy: &SandboxPolicy,
) -> Result<Command, SandboxUnavailable> {
    // For Elevated policy, validate fields first (fail-closed per ADR-017 Rule 2).
    if let SandboxPolicy::Elevated { .. } = policy {
        policy.validate()?;
    }

    let repo = repo_root.to_string_lossy();
    let scratch = scratch_dir.to_string_lossy();

    // ADR-017 Rule 1: deny all by default; allow read only to system paths,
    // repo root (read-only), and scratch (read-write).
    // Network: Standard → deny; Elevated → allow (coarse — sandbox-exec has no
    // per-host filtering; enforce via egress proxy / firewall at the OS level).
    let network_rule = match policy {
        SandboxPolicy::Standard => "(deny network*)".to_string(),
        SandboxPolicy::Elevated {
            permitted_hosts, ..
        } => {
            // sandbox-exec does not support per-hostname rules. We allow network*
            // for the process and rely on OS-level egress controls for the host
            // allowlist. Log a warning so the operator is aware.
            tracing::warn!(
                permitted_hosts = ?permitted_hosts,
                "ADR-017 Elevated policy on macOS: sandbox-exec cannot enforce per-host \
                 network filtering; '(allow network*)' is in effect — enforce permitted \
                 hosts via a local firewall or egress proxy"
            );
            "(allow network*)".to_string()
        }
    };

    let profile = format!(
        r#"(version 1)
(deny default)
{network_rule}
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
        network_rule = network_rule,
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
    _policy: &SandboxPolicy,
) -> Result<Command, SandboxUnavailable> {
    Err(SandboxUnavailable(
        "macOS sandbox not available on this platform".into(),
    ))
}
