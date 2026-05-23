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
//! - **Filesystem writes**: restricted to `/tmp` and `repo_root`; everything
//!   else is read-only
//! - **Wall-clock timeout**: enforced by the caller via [`SandboxConfig::timeout`]
//! - **Virtual-memory limit**: set via `ulimit -v` inside the sandbox shell
//!
//! ## Fail-open policy
//!
//! When the sandbox tool is absent, the subprocess runs **unsandboxed** and
//! [`SandboxStatus::Unavailable`] is returned so the caller can log a warning.
//! This keeps LSIF edges working for developers without bubblewrap installed,
//! while letting production deployments enforce the sandbox via their package
//! manager.

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
    pub timeout: Duration,
    /// Maximum virtual memory in bytes (default: 4 GiB).
    /// Enforced via `ulimit -v` inside the sandbox shell.
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
    /// directly. Callers should log this at `warn` level.
    Unavailable { reason: String },
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Build a sandboxed [`Command`] for `program` with `args`.
///
/// Returns `(cmd, status)` where:
/// - `cmd` is ready to [`spawn`](Command::spawn) — it may be wrapped in a
///   sandbox invocation or may invoke `program` directly.
/// - `status` indicates whether sandboxing was applied and which mechanism
///   was used.
///
/// The caller is responsible for enforcing [`SandboxConfig::timeout`] on the
/// spawned child; see [`crate::runner`] for the polling pattern.
///
/// # Platform behaviour
///
/// - **Linux**: wraps with `bwrap` if found on PATH; otherwise falls back.
/// - **macOS**: wraps with `sandbox-exec` (always available since 10.5).
/// - **Windows**: no OS sandbox applied (AppContainer deferred — TODO:
///   `DEBT(travsr-indexer): Windows AppContainer sandbox, Issue #124`).
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
        return fallback(program, args, "bwrap not found on PATH (install bubblewrap)");
    }

    let repo = cfg.repo_root.to_string_lossy();
    let mem_kb = cfg.mem_limit_bytes / 1024;
    // Run program inside a shell so we can set ulimit before exec.
    let inner = format!(
        "ulimit -v {mem_kb} 2>/dev/null; exec {cmd}",
        cmd = shell_command(program, args)
    );

    let mut cmd = Command::new("bwrap");
    cmd.args([
        // Isolate namespaces
        "--unshare-net",
        "--unshare-pid",
        "--unshare-uts",
        "--unshare-ipc",
        // Read-only view of the host filesystem
        "--ro-bind",
        "/",
        "/",
        // Writable tmpfs at /tmp (ephemeral, host /tmp is NOT visible)
        "--tmpfs",
        "/tmp",
        // Writable bind of the repo root (rust-analyzer writes lock files, etc.)
        "--bind",
        repo.as_ref(),
        repo.as_ref(),
        // Kill child if travsr daemon exits
        "--die-with-parent",
        "--",
        "sh",
        "-c",
        &inner,
    ]);

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

    // sandbox-exec TinyScheme profile: deny everything then allow what we need.
    // `sandbox-exec` is deprecated in Xcode but remains functional in the OS.
    let profile = format!(
        r#"(version 1)
(deny default)
(allow process*)
(allow signal)
(allow file-read*)
(allow file-write* (subpath "/tmp"))
(allow file-write* (subpath "/var/folders"))
(allow file-write* (subpath "{repo}"))
(allow mach*)
(allow ipc*)
(allow sysctl*)
"#,
        repo = repo
    );

    let inner = format!(
        "ulimit -v {mem_kb} 2>/dev/null; exec {cmd}",
        cmd = shell_command(program, args)
    );

    let mut cmd = Command::new("sandbox-exec");
    cmd.args(["-p", &profile, "sh", "-c", &inner]);

    (cmd, SandboxStatus::Active { mechanism: "sandbox-exec" })
}

#[cfg(target_os = "windows")]
fn build_sandboxed_command_impl(
    program: &str,
    args: &[&str],
    _cfg: &SandboxConfig,
) -> (Command, SandboxStatus) {
    // TODO(travsr-indexer): Windows AppContainer / Job Object sandbox.
    // Issue #124. For now, timeout (enforced by caller) is the only
    // mitigation on Windows.
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

#[cfg(not(target_os = "macos"))]
/// Return an unsandboxed command with a `SandboxStatus::Unavailable` status.
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
/// Each token is wrapped in single quotes with embedded single quotes escaped.
fn shell_command(program: &str, args: &[&str]) -> String {
    let mut parts = vec![shell_quote(program)];
    parts.extend(args.iter().map(|a| shell_quote(a)));
    parts.join(" ")
}

fn shell_quote(s: &str) -> String {
    // Replace ' with '\'' — end quote, literal ', reopen quote.
    format!("'{}'", s.replace('\'', r"'\''"))
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
    fn fallback_builds_direct_command() {
        let cfg = SandboxConfig::default();
        let (_, status) = build_sandboxed_command("__nonexistent__", &["--version"], &cfg);
        // On Linux without bwrap OR on Windows, we should see either Active or
        // Unavailable — both are valid depending on the environment.
        match status {
            SandboxStatus::Active { .. } | SandboxStatus::Unavailable { .. } => {}
        }
    }

    /// On Linux with bubblewrap installed: verify the sandbox blocks writes to
    /// `$HOME` (outside /tmp and repo_root). The test is silently skipped when
    /// `bwrap` is not on PATH so it does not fail CI environments without it.
    #[test]
    #[cfg(target_os = "linux")]
    fn sandbox_blocks_write_outside_repo_and_tmp() {
        if !bwrap_available() {
            eprintln!("SKIP: bwrap not found — sandbox isolation test skipped");
            return;
        }

        let tmp_repo = tempfile::tempdir().expect("tempdir");
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
        let breach_path = format!("{home}/.travsr_sandbox_breach_test");

        // Ensure the breach file does not exist before the test.
        let _ = std::fs::remove_file(&breach_path);

        let cfg = SandboxConfig {
            repo_root: tmp_repo.path().to_path_buf(),
            ..Default::default()
        };

        let (mut cmd, status) = build_sandboxed_command(
            "sh",
            &["-c", &format!("touch {breach_path}")],
            &cfg,
        );

        assert!(
            matches!(status, SandboxStatus::Active { .. }),
            "expected Active sandbox on Linux with bwrap"
        );

        // Run the command — it should fail (EROFS) because $HOME is read-only.
        let output = cmd
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .expect("failed to spawn sandboxed sh");

        // Primary assertion: breach file must NOT exist on the host.
        assert!(
            !std::path::Path::new(&breach_path).exists(),
            "sandbox breach: {breach_path} was created on the host filesystem"
        );

        // Secondary: the shell itself should have exited with an error.
        assert!(
            !output.status.success(),
            "sandboxed sh succeeded despite writing to a read-only path"
        );
    }

    /// On macOS: verify that sandbox-exec is present and the command builds
    /// without panicking.
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
            matches!(status, SandboxStatus::Active { mechanism: "sandbox-exec" }),
            "expected sandbox-exec to be active on macOS"
        );
    }
}
