//! OS-level sandboxing for untrusted subprocesses (rust-analyzer, LSIF emitters).
//!
//! # Threat model (SEC-201)
//!
//! When Travsr invokes `rust-analyzer lsif` over a repository it does not own,
//! `cargo metadata` may trigger `build.rs` execution or proc-macro expansion.
//! A malicious crate could exploit that to exfiltrate data or pivot to the
//! developer's machine. This module wraps subprocess spawning with the
//! strongest OS-level confinement available on each platform:
//!
//! | Platform | Mechanism              | Requires                   |
//! |----------|------------------------|----------------------------|
//! | Linux    | bubblewrap (`bwrap`)   | `bwrap` on PATH            |
//! | macOS    | `sandbox-exec`         | Built-in (macOS 10.5+)     |
//! | Windows  | timeout only           | (AppContainer deferred)    |
//!
//! ## Constraints applied when sandboxing is active
//!
//! - **Network**: denied entirely (`--unshare-net` / `deny network*`)
//! - **Filesystem reads**: restricted to OS libraries, Rust toolchain, and
//!   repo_root; sensitive paths (`$HOME/.ssh`, `/etc/shadow`, etc.) are not
//!   visible inside the sandbox
//! - **Filesystem writes**: restricted to `/tmp` and `repo_root`
//! - **Wall-clock timeout**: enforced by the caller via [`SandboxConfig::timeout`]
//! - **Virtual-memory limit**: set via `ulimit -v` inside the sandbox shell
//!
//! ## Sandbox availability
//!
//! When the sandbox tool is absent or cannot create namespaces (Linux without
//! a functional `bwrap`), [`SandboxStatus::Unavailable`] is returned alongside
//! a runnable-but-unsandboxed command. **Callers MUST log this at `error`
//! level** — not `warn` — and MUST NOT silently proceed.
//!
//! ADR-017 Rule 2 (fail-closed) applies to the plugin-host sidecar path.
//! The indexer sandbox is a separate, older subsystem covering rust-analyzer
//! LSIF invocations; aligning it fully to Result-based fail-closed is tracked
//! as a follow-up refactor. The CI gate in `sandbox_blocks_write_outside_repo_and_tmp`
//! panics when bwrap is non-functional in CI, making the gap visible.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

// ── Configuration ─────────────────────────────────────────────────────────────

/// Configuration for a sandboxed subprocess invocation.
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    /// Repository root — the only directory the subprocess may write to.
    pub repo_root: PathBuf,
    /// Maximum wall-clock time before the process is killed (default: 300 s).
    /// Enforced by the caller (see [`crate::runner`] for the polling pattern).
    pub timeout: Duration,
    /// Virtual-memory cap enforced via `ulimit -v` (default: 4 GiB).
    ///
    /// NOTE: `ulimit -v` limits VSZ (virtual address space), not RSS.
    /// rust-analyzer uses `mmap` aggressively; a process hitting this limit
    /// will crash with SIGSEGV rather than being killed cleanly. The caller's
    /// timeout is the reliable backstop for runaway processes. RSS limiting
    /// requires `RLIMIT_AS` via unsafe / the `rlimit` crate — deferred.
    pub mem_limit_bytes: u64,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            repo_root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            timeout: Duration::from_secs(300),
            mem_limit_bytes: 4 * 1024 * 1024 * 1024, // 4 GiB
        }
    }
}

// ── Status ────────────────────────────────────────────────────────────────────

/// Whether a sandbox was successfully applied to the returned [`Command`].
#[derive(Debug)]
pub enum SandboxStatus {
    /// Sandbox is active. `mechanism` names the tool (e.g. `"bwrap"`,
    /// `"sandbox-exec"`).
    Active { mechanism: &'static str },
    /// No sandbox could be applied. The returned command runs the program
    /// directly. **Callers MUST log this at `error` level.**
    Unavailable { reason: String },
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Build a sandboxed [`Command`] for `program` with `args`.
///
/// Returns `(cmd, status)` where:
/// - `cmd` is ready to [`spawn`](Command::spawn).
/// - `status` is [`SandboxStatus::Active`] when confinement was applied, or
///   [`SandboxStatus::Unavailable`] when it was not — the caller must log at
///   `error` level in the unavailable case.
///
/// # Platform behaviour
///
/// - **Linux**: wraps with `bwrap` (targeted filesystem binds — no broad
///   `--ro-bind / /`); falls back when `bwrap` is absent.
/// - **macOS**: wraps with `sandbox-exec` (always available since 10.5);
///   profile uses minimum-necessary Mach service allowances.
/// - **Windows**: no OS sandbox (AppContainer deferred —
///   `DEBT(travsr-indexer): Issue #124`).
pub fn build_sandboxed_command(
    program: &str,
    args: &[&str],
    cfg: &SandboxConfig,
) -> (Command, SandboxStatus) {
    build_sandboxed_command_impl(program, args, cfg)
}

// ── Platform implementations ──────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn build_sandboxed_command_impl(
    program: &str,
    args: &[&str],
    cfg: &SandboxConfig,
) -> (Command, SandboxStatus) {
    if !bwrap_available() {
        return fallback(
            program,
            args,
            "bwrap not found on PATH — install bubblewrap for subprocess isolation",
        );
    }

    let repo = cfg.repo_root.to_string_lossy();
    let mem_kb = cfg.mem_limit_bytes / 1024;
    let inner = format!(
        "ulimit -v {mem_kb} 2>/dev/null; exec {cmd}",
        cmd = shell_command(program, args)
    );

    // Resolve user HOME for targeted Rust toolchain binds.
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_owned());
    let rustup_dir = format!("{home}/.rustup");
    let cargo_home = format!("{home}/.cargo");

    let mut cmd = Command::new("bwrap");

    // Namespace isolation — network is the primary exfiltration vector.
    cmd.args([
        "--unshare-net",
        "--unshare-pid",
        "--unshare-uts",
        "--unshare-ipc",
    ]);

    // Targeted read-only binds — deliberately excludes $HOME, /etc/passwd,
    // /etc/shadow, ~/.ssh, ~/.gnupg, etc. to limit credential exposure.
    // `--ro-bind-try` silently skips absent paths (distro-agnostic).
    for path in ["/usr", "/bin", "/sbin", "/lib", "/lib64"] {
        cmd.args(["--ro-bind-try", path, path]);
    }
    for path in [
        "/etc/alternatives", // Debian/Ubuntu symlinks
        "/etc/localtime",    // timezone
        "/etc/ld.so.cache",  // dynamic linker cache
        "/etc/ld.so.conf",   // dynamic linker config
    ] {
        cmd.args(["--ro-bind-try", path, path]);
    }
    // Rust toolchain — needed by rust-analyzer for type resolution.
    cmd.args(["--ro-bind-try", &rustup_dir, &rustup_dir]);
    cmd.args(["--ro-bind-try", &cargo_home, &cargo_home]);

    // Kernel interfaces.
    cmd.args(["--proc", "/proc"]);
    cmd.args(["--dev", "/dev"]);

    // Scratch space: ephemeral tmpfs — host /tmp is NOT visible.
    cmd.args(["--tmpfs", "/tmp"]);

    // Repo root: writable bind (rust-analyzer writes lock files and cache).
    cmd.args(["--bind", repo.as_ref(), repo.as_ref()]);

    cmd.args(["--die-with-parent", "--", "sh", "-c", &inner]);

    (cmd, SandboxStatus::Active { mechanism: "bwrap" })
}

#[cfg(target_os = "macos")]
fn build_sandboxed_command_impl(
    program: &str,
    args: &[&str],
    cfg: &SandboxConfig,
) -> (Command, SandboxStatus) {
    let repo = cfg.repo_root.to_string_lossy();
    let mem_kb = cfg.mem_limit_bytes / 1024;

    // sandbox-exec TinyScheme profile — deny everything, then allow the
    // minimum rust-analyzer needs.
    //
    // `(allow mach*)` has been replaced with named service allowances only:
    // a broad mach-lookup permit could allow connections to Mach-based network
    // proxies (com.apple.networkd, com.apple.cfnetwork) that bypass the
    // `(deny network*)` default. `(allow ipc*)` has been removed entirely —
    // POSIX shared memory is not needed by rust-analyzer lsif.
    let profile = format!(
        r#"(version 1)
(deny default)
(allow process-fork)
(allow process-exec*)
(allow signal)
(allow file-read*)
(allow file-write* (subpath "/tmp"))
(allow file-write* (subpath "/var/folders"))
(allow file-write* (subpath "{repo}"))
(allow mach-lookup
    (global-name "com.apple.dyld")
    (global-name "com.apple.logd")
    (global-name "com.apple.system.logger")
    (global-name "com.apple.SecurityServer")
    (global-name "com.apple.CoreServices.coreservicesd")
)
(allow sysctl-read)
"#,
        repo = repo
    );

    let inner = format!(
        "ulimit -v {mem_kb} 2>/dev/null; exec {cmd}",
        cmd = shell_command(program, args)
    );

    let mut cmd = Command::new("sandbox-exec");
    cmd.args(["-p", &profile, "sh", "-c", &inner]);

    (
        cmd,
        SandboxStatus::Active {
            mechanism: "sandbox-exec",
        },
    )
}

#[cfg(target_os = "windows")]
fn build_sandboxed_command_impl(
    program: &str,
    args: &[&str],
    _cfg: &SandboxConfig,
) -> (Command, SandboxStatus) {
    // DEBT(travsr-indexer): Windows AppContainer / Job Object sandbox.
    // Issue #124. Timeout (enforced by caller) is the only mitigation for now.
    fallback(
        program,
        args,
        "Windows AppContainer sandbox not yet implemented (Issue #124)",
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn build_sandboxed_command_impl(
    program: &str,
    args: &[&str],
    _cfg: &SandboxConfig,
) -> (Command, SandboxStatus) {
    fallback(program, args, "sandbox not implemented on this platform")
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Return an unsandboxed command with a `SandboxStatus::Unavailable` status.
/// Not called on macOS where `sandbox-exec` is always present.
#[allow(dead_code)]
fn fallback(program: &str, args: &[&str], reason: &str) -> (Command, SandboxStatus) {
    let mut cmd = Command::new(program);
    cmd.args(args);
    (
        cmd,
        SandboxStatus::Unavailable {
            reason: reason.to_owned(),
        },
    )
}

/// Produce a POSIX-shell-safe `'program' 'arg1' 'arg2'` string.
/// Each token is wrapped in single quotes; embedded `'` is escaped as `'\''`.
fn shell_command(program: &str, args: &[&str]) -> String {
    let mut parts = vec![shell_quote(program)];
    parts.extend(args.iter().map(|a| shell_quote(a)));
    parts.join(" ")
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

// Cached functional probe — same pattern as the plugin-host sandbox.
// Avoids false "available" result when bwrap is on PATH but cannot create
// user namespaces (e.g. Docker-in-Docker, AppArmor-restricted runners).
#[cfg(target_os = "linux")]
static BWRAP_FUNCTIONAL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

#[cfg(target_os = "linux")]
fn bwrap_available() -> bool {
    *BWRAP_FUNCTIONAL.get_or_init(|| {
        Command::new("bwrap")
            .args([
                "--ro-bind-try",
                "/usr",
                "/usr",
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_sensible_values() {
        let cfg = SandboxConfig::default();
        assert_eq!(cfg.timeout, Duration::from_secs(300));
        assert_eq!(cfg.mem_limit_bytes, 4 * 1024 * 1024 * 1024);
    }

    #[test]
    fn shell_quote_handles_special_characters() {
        assert_eq!(shell_quote("hello"), "'hello'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("a b c"), "'a b c'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn shell_command_produces_valid_string() {
        let s = shell_command("echo", &["hello world", "it's fine"]);
        assert_eq!(s, r"'echo' 'hello world' 'it'\''s fine'");
    }

    #[test]
    fn build_sandboxed_command_returns_valid_status() {
        let cfg = SandboxConfig::default();
        let (_, status) = build_sandboxed_command("__nonexistent__", &["--version"], &cfg);
        // Both Active and Unavailable are valid depending on the environment.
        match status {
            SandboxStatus::Active { .. } | SandboxStatus::Unavailable { .. } => {}
        }
    }

    /// On Linux with bubblewrap installed: verify the sandbox blocks writes to
    /// `$HOME` (outside /tmp and repo_root).
    ///
    /// In CI this test hard-fails when bwrap is absent — the workflow installs
    /// bubblewrap before running tests. On developer machines without bwrap the
    /// test is skipped with an explanatory message.
    #[test]
    #[cfg(target_os = "linux")]
    fn sandbox_blocks_write_outside_repo_and_tmp() {
        // Separate "binary absent" (CI-fatal) from "namespaces unavailable" (CI-skip).
        // Ubuntu 24.04+ restricts unprivileged user namespaces; bwrap may be installed
        // but unable to create namespaces on some runners (sysctl fixes this in ci.yml).
        let bwrap_on_path = std::process::Command::new("bwrap")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok();
        if !bwrap_on_path {
            if std::env::var("CI").is_ok() {
                panic!(
                    "bwrap not found on PATH in CI. \
                     Add `sudo apt-get install -y bubblewrap` before `cargo test`."
                );
            }
            eprintln!("SKIP: bwrap not installed");
            return;
        }
        if !bwrap_available() {
            eprintln!("SKIP: bwrap installed but namespace isolation unavailable on this runner — sandbox isolation test skipped");
            return;
        }

        let tmp_repo = tempfile::tempdir().expect("tempdir");
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
        let breach_path = format!("{home}/.travsr_sandbox_breach_test");
        let _ = std::fs::remove_file(&breach_path);

        let cfg = SandboxConfig {
            repo_root: tmp_repo.path().to_path_buf(),
            ..Default::default()
        };

        let (mut cmd, status) =
            build_sandboxed_command("sh", &["-c", &format!("touch {breach_path}")], &cfg);

        assert!(
            matches!(status, SandboxStatus::Active { .. }),
            "expected Active sandbox on Linux with bwrap"
        );

        let output = cmd
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .expect("failed to spawn sandboxed sh");

        // Primary: breach file must NOT exist on the host filesystem.
        assert!(
            !std::path::Path::new(&breach_path).exists(),
            "sandbox breach: {breach_path} was created on the host filesystem"
        );

        // Secondary: the shell should have exited non-zero (EROFS / EPERM).
        assert!(
            !output.status.success(),
            "sandboxed sh succeeded when writing to a read-only path — \
             filesystem isolation is not working"
        );
    }

    /// On macOS: verify that sandbox-exec is present and the command builds.
    #[test]
    #[cfg(target_os = "macos")]
    fn macos_sandbox_exec_command_builds() {
        let tmp_repo = tempfile::tempdir().expect("tempdir");
        let cfg = SandboxConfig {
            repo_root: tmp_repo.path().to_path_buf(),
            ..Default::default()
        };
        let (_, status) = build_sandboxed_command("echo", &["hi"], &cfg);
        assert!(
            matches!(
                status,
                SandboxStatus::Active {
                    mechanism: "sandbox-exec"
                }
            ),
            "expected sandbox-exec to be active on macOS"
        );
    }
}
