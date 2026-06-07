//! Linux sandbox (bubblewrap + optional Landlock). Fail-closed per ADR-017 Rule 2.
use crate::sandbox::policy::{SandboxPolicy, SandboxUnavailable};
use crate::sandbox::SandboxedSpawn;
use std::path::Path;
#[cfg(target_os = "linux")]
use std::process::Command;

/// Permitted env vars (ADR-017 Rule 1). TMPDIR set by caller to scratch dir.
pub const ENV_ALLOWLIST: &[&str] = &["PATH", "LANG", "LC_ALL"];

/// Cached probe: does `bwrap --unshare-net` succeed on this host?
///
/// GitHub Actions Ubuntu 24.04 runners disallow `RTM_NEWADDR` inside a new
/// network namespace, so bwrap exits non-zero when it tries to bring up the
/// loopback interface. We probe once, cache the result, and skip `--unshare-net`
/// when it is not supported rather than aborting the sandbox invocation.
#[cfg(target_os = "linux")]
static NET_UNSHARE_OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

#[cfg(target_os = "linux")]
fn net_unshare_supported() -> bool {
    *NET_UNSHARE_OK.get_or_init(|| {
        // Mount /lib and /lib64 alongside /usr so the dynamic linker
        // (/lib64/ld-linux-x86-64.so.2) is reachable inside the probe sandbox.
        // On merged-usr systems these are host symlinks → /usr/lib{,64}; they
        // don't appear automatically inside bwrap's fresh tmpfs root.
        Command::new("bwrap")
            .args([
                "--unshare-net",
                "--ro-bind-try",
                "/usr",
                "/usr",
                "--ro-bind-try",
                "/lib",
                "/lib",
                "--ro-bind-try",
                "/lib64",
                "/lib64",
                "--proc",
                "/proc",
                "--dev",
                "/dev",
                "--",
                "true",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

#[cfg(target_os = "linux")]
pub fn build_sandboxed_command(
    program: &str,
    args: &[&str],
    repo_root: &Path,
    scratch_dir: &Path,
    policy: &SandboxPolicy,
    language: &str,
) -> Result<SandboxedSpawn, SandboxUnavailable> {
    if !bwrap_available() {
        return Err(SandboxUnavailable(
            "bubblewrap (bwrap) is not available or cannot create sandboxes on this host \
             (not on PATH, or kernel namespace support is restricted); \
             install bubblewrap and verify unprivileged user namespaces are enabled"
                .into(),
        ));
    }

    // For Elevated policy, validate fields first (fail-closed per ADR-017 Rule 2).
    if let SandboxPolicy::Elevated { .. } = policy {
        policy.validate()?;
    }

    let repo = repo_root.to_string_lossy();
    let scratch = scratch_dir.to_string_lossy();

    // Per-language toolchain grants (e.g. go module/build caches + GO*/HOME env).
    // Empty for languages with no out-of-repo needs.
    let tc = crate::sandbox::toolchain::toolchain_access(language);

    let mut cmd = Command::new("bwrap");

    match policy {
        SandboxPolicy::Standard => {
            // --unshare-net is probed at first use: on hosts where loopback setup
            // inside the new network namespace is blocked (e.g. GitHub Actions,
            // some container runtimes) bwrap exits non-zero with
            // "RTM_NEWADDR: Operation not permitted".
            // ADR-017 Rule 2: fail-closed. Network isolation is a primary egress
            // control; silently dropping it is not an acceptable degradation.
            if net_unshare_supported() {
                cmd.arg("--unshare-net");
            } else {
                return Err(SandboxUnavailable(
                    "bwrap --unshare-net not supported on this host (loopback blocked \
                     inside the network namespace); plugin disabled per ADR-017 Rule 2 \
                     — to run on a host without network-namespace support, request an \
                     Elevated policy exception via ADR-017 §Elevated"
                        .into(),
                ));
            }
        }
        SandboxPolicy::Elevated {
            permitted_hosts, ..
        } => {
            // Elevated: FS confinement via bwrap still applies, but --unshare-net
            // is intentionally skipped so the plugin can reach its permitted hosts.
            // Host-level filtering (firewall / egress proxy) must enforce the
            // permitted_hosts list — bwrap has no per-host network rule support.
            tracing::info!(
                permitted_hosts = ?permitted_hosts,
                "ADR-017 Elevated policy: network namespace isolation disabled; \
                 plugin may reach permitted hosts (enforce via egress controls)"
            );
        }
    }
    cmd.args(["--unshare-pid", "--unshare-uts", "--unshare-ipc"]);
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
    // Provide a writable scratch area inside the sandbox at /travsr-scratch by
    // bind-mounting the host scratch dir there. The host dir is owned by the
    // real UID (created via tempfile by the daemon), so the sandboxed process —
    // which runs as that same UID since we don't remap with --unshare-user — can
    // write to it. bwrap creates the /travsr-scratch mount point in its root
    // tmpfs automatically. A plain --tmpfs would be root-owned and unwritable.
    cmd.args(["--bind", scratch.as_ref(), "/travsr-scratch"]); // writable scratch
    cmd.args(["--ro-bind", repo.as_ref(), repo.as_ref()]); // repo: ro
    // Per-language toolchain caches: read-only module/toolchain dirs, writable
    // build cache. Bound at their host paths so the GO*/HOME env (set below) resolve.
    for path in &tc.read_paths {
        let p = path.to_string_lossy();
        cmd.args(["--ro-bind-try", p.as_ref(), p.as_ref()]);
    }
    for path in &tc.write_paths {
        let p = path.to_string_lossy();
        cmd.args(["--bind-try", p.as_ref(), p.as_ref()]);
    }
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
    // Per-language toolchain env (e.g. GOPATH/GOCACHE/GOMODCACHE/HOME) so the
    // analyzer's build tool locates its caches inside the cleared sandbox env.
    for (key, val) in &tc.env {
        cmd.env(key, val);
    }
    Ok(SandboxedSpawn::Wrapped(cmd))
}

/// Returns true if `bwrap` is on PATH (installed).
/// Use this to distinguish "not installed" from "installed but cannot namespace" —
/// the CI panic in sandbox tests should only fire for the former.
#[cfg(target_os = "linux")]
pub fn bwrap_is_on_path() -> bool {
    Command::new("bwrap")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// Cached probe: can bwrap actually create a sandbox on this host?
// On some CI runners (Docker-in-Docker, AppArmor-restricted kernels) bwrap is
// installed but cannot create user namespaces. We detect this once and treat such
// hosts as sandbox-unavailable so callers receive Err(SandboxUnavailable) and skip
// gracefully instead of spawning a bwrap that exits non-zero.
#[cfg(target_os = "linux")]
static BWRAP_FUNCTIONAL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

#[cfg(target_os = "linux")]
fn bwrap_available() -> bool {
    *BWRAP_FUNCTIONAL.get_or_init(|| {
        // Mount /lib and /lib64 alongside /usr so the dynamic linker
        // (/lib64/ld-linux-x86-64.so.2) is reachable inside the probe sandbox.
        // On merged-usr systems (Ubuntu 22.04+) /lib and /lib64 are host
        // symlinks → /usr/lib{,64}; they don't appear automatically inside
        // bwrap's fresh tmpfs root, causing execvp("true") to fail with ENOENT.
        Command::new("bwrap")
            .args([
                "--ro-bind-try",
                "/usr",
                "/usr",
                "--ro-bind-try",
                "/lib",
                "/lib",
                "--ro-bind-try",
                "/lib64",
                "/lib64",
                "--proc",
                "/proc",
                "--dev",
                "/dev",
                "--",
                "true",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

#[cfg(not(target_os = "linux"))]
pub fn build_sandboxed_command(
    _p: &str,
    _a: &[&str],
    _r: &Path,
    _s: &Path,
    _policy: &SandboxPolicy,
) -> Result<SandboxedSpawn, SandboxUnavailable> {
    Err(SandboxUnavailable(
        "Linux sandbox not available on this platform".into(),
    ))
}
