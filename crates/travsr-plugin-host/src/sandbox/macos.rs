//! macOS sandbox (sandbox-exec / Seatbelt). Fail-closed per ADR-017 Rule 2.
#[cfg(target_os = "macos")]
use crate::sandbox::linux::ENV_ALLOWLIST;
use crate::sandbox::policy::{SandboxPolicy, SandboxUnavailable};
use crate::sandbox::SandboxedSpawn;
use std::path::Path;
#[cfg(target_os = "macos")]
use std::process::Command;

#[cfg(target_os = "macos")]
pub fn build_sandboxed_command(
    program: &str,
    args: &[&str],
    repo_root: &Path,
    scratch_dir: &Path,
    policy: &SandboxPolicy,
    language: &str,
) -> Result<SandboxedSpawn, SandboxUnavailable> {
    // For Elevated policy, validate fields first (fail-closed per ADR-017 Rule 2).
    if let SandboxPolicy::Elevated { .. } = policy {
        policy.validate()?;
    }

    // Per-language toolchain grants: a build-tool analyzer (scip-go, …) must read
    // its module/build caches and see its env, or it resolves zero packages and
    // emits an empty index. Empty for languages with no out-of-repo needs.
    let tc = crate::sandbox::toolchain::toolchain_access(language);
    let canon_path =
        |p: &std::path::Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let tc_read_rule = tc
        .read_paths
        .iter()
        .map(|p| format!("\n    (subpath \"{}\")", canon_path(p).to_string_lossy()))
        .collect::<String>();
    let tc_write_rule = tc
        .write_paths
        .iter()
        .map(|p| format!(" (subpath \"{}\")", canon_path(p).to_string_lossy()))
        .collect::<String>();

    // The Seatbelt sandbox matches paths by their *resolved* form (symlinks and
    // firmlinks followed). On macOS `/tmp` → `/private/tmp` and `/var` →
    // `/private/var`, so a subpath rule built from the unresolved path silently
    // fails to match the real path the kernel sees — e.g. a scratch dir under
    // `/var/folders/...` would be unwritable. Canonicalize so the rules match.
    // Fall back to the original path if canonicalization fails (path still valid
    // for the rule; the operation simply remains denied, fail-closed).
    let repo_canon = std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    let scratch_canon =
        std::fs::canonicalize(scratch_dir).unwrap_or_else(|_| scratch_dir.to_path_buf());
    let repo = repo_canon.to_string_lossy();
    let scratch = scratch_canon.to_string_lossy();

    // Allow reading the directory holding the binary being exec'd. Builtins
    // (current_exe) live under the repo, but external providers (travsr-lang-*,
    // rust-analyzer) resolve to ~/.travsr/bin, node_modules, or /opt/homebrew/bin
    // which are not otherwise readable. npm-installed providers are a *symlink*
    // (e.g. ~/.nvm/.../bin/travsr-lang-go → .../node_modules/.../travsr-lang-go),
    // so we must allow BOTH the symlink's directory (to resolve/exec it) and the
    // canonicalized target's directory (to read the real binary). Without the
    // symlink dir the kernel cannot even read the link to follow it.
    let mut program_dirs: Vec<String> = Vec::new();
    let mut push_parent = |path: &Path| {
        if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            let s = dir.to_string_lossy().into_owned();
            if !program_dirs.contains(&s) {
                program_dirs.push(s);
            }
        }
    };
    push_parent(Path::new(program));
    if let Ok(real) = std::fs::canonicalize(program) {
        push_parent(&real);
    }
    let program_dir_rule = if program_dirs.is_empty() {
        String::new()
    } else {
        let subpaths = program_dirs
            .iter()
            .map(|d| format!("(subpath \"{d}\")"))
            .collect::<Vec<_>>()
            .join(" ");
        format!("(allow file-read* {subpaths})\n")
    };

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

    // `(import "system.sb")` pulls in Apple's maintained base profile, which
    // grants the minimal access the dynamic linker needs at process startup —
    // most importantly reading the dyld shared cache (on Apple Silicon this lives
    // on a separate APFS volume at /System/Volumes/Preboot/Cryptexes/..., which is
    // NOT covered by a `(subpath "/System")` rule because the kernel resolves it
    // to a different real path). Without this import, `(deny default)` blocks the
    // shared-cache read and every spawned binary SIGABRTs inside dyld4::CacheFinder
    // *before main() runs* — surfacing to the daemon as an instant EOF on the
    // handshake read ("failed to fill whole buffer"). The import precedes our
    // `(deny default)` / `(deny network*)` / write-confinement rules, which still
    // take effect (last-match-wins), so network and out-of-scratch writes stay
    // denied (verified by test).
    let profile = format!(
        r#"(version 1)
(import "system.sb")
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
    (subpath "/opt/homebrew")
    (subpath "{repo}")
    (subpath "{scratch}"){tc_read_rule})
{program_dir_rule}(allow file-write* (subpath "{scratch}"){tc_write_rule})
(allow mach-lookup
    (global-name "com.apple.dyld")
    (global-name "com.apple.logd")
    (global-name "com.apple.system.logger"))
(allow sysctl-read)
"#,
        network_rule = network_rule,
        repo = repo,
        scratch = scratch,
        program_dir_rule = program_dir_rule,
        tc_read_rule = tc_read_rule,
        tc_write_rule = tc_write_rule,
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
    // Per-language toolchain env (e.g. GOPATH/GOCACHE/GOMODCACHE/HOME for go) so the
    // analyzer's build tool can locate its caches inside the otherwise-cleared env.
    for (key, val) in &tc.env {
        cmd.env(key, val);
    }
    Ok(SandboxedSpawn::Wrapped(cmd))
}

#[cfg(not(target_os = "macos"))]
pub fn build_sandboxed_command(
    _p: &str,
    _a: &[&str],
    _r: &Path,
    _s: &Path,
    _policy: &SandboxPolicy,
) -> Result<SandboxedSpawn, SandboxUnavailable> {
    Err(SandboxUnavailable(
        "macOS sandbox not available on this platform".into(),
    ))
}
