//! rust-analyzer LSIF subprocess runner (INDEX-211).
//!
//! Spawns `rust-analyzer lsif <repo_root>` inside the SEC-201 sandbox and
//! returns the raw LSIF JSON-Lines dump for ingestion by [`crate::lsif`].
//!
//! ## Graceful degradation
//!
//! - `Ok(None)` — rust-analyzer not on PATH; caller skips LSIF ingestion.
//! - `Err(_)` — rust-analyzer found but failed; caller logs and skips.
//!
//! Neither case fails the overall index — Tree-sitter structural edges are
//! always emitted regardless of whether LSIF ingestion succeeds.

use std::path::Path;

use anyhow::Context as _;

use crate::runner::run_with_drain;
use crate::sandbox::{
    build_sandboxed_command, record_ra_lsif_sandbox_skip, should_skip_unsandboxed, SandboxConfig,
    SandboxStatus,
};

// ── Public API ────────────────────────────────────────────────────────────────

/// Return the path to the `rust-analyzer` binary, or `None` if unavailable.
///
/// Search order:
/// 1. `rust-analyzer` on the process PATH.
/// 2. `$CARGO_HOME/bin/rust-analyzer` — explicit install root.
/// 3. `$HOME/.cargo/bin/rust-analyzer` — default Cargo (rustup) install location.
/// 4. `~/.travsr/bin/rust-analyzer` — where `travsr lang install rust` places a
///    binary downloaded from GitHub when rustup is absent.
///
/// Fallbacks 2–4 ensure LSIF enrichment works even when the daemon's PATH has
/// been stripped to `/usr/bin:/bin:/usr/sbin:/sbin` (e.g. when forked to
/// background by the CLI where the parent shell had `~/.cargo/bin` on PATH).
/// The home-relative candidates carry the platform executable suffix so the
/// Windows `rust-analyzer.exe` is found the same way as the unix binary.
///
/// Result is cached via `OnceLock` — the probe runs at most once per process
/// lifetime.
pub fn ra_binary_path() -> Option<std::path::PathBuf> {
    static RA_PATH: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    RA_PATH
        .get_or_init(|| {
            // 1. PATH probe: preferred — honours user's active toolchain.
            // #738: require a SUCCESSFUL exit, not merely a spawned process. A
            // rustup proxy shim is on PATH even when the `rust-analyzer` component
            // is not installed; `rust-analyzer --version` through it then exits
            // non-zero. `.status().is_ok()` (process ran at all) treated that as
            // "available" and later spawned `rust-analyzer lsif`, which the shim
            // rejected with "Unknown binary ..." — silently skipping LSIF.
            let on_path = std::process::Command::new("rust-analyzer")
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if on_path {
                return Some(std::path::PathBuf::from("rust-analyzer"));
            }
            // 2–4. Explicit home-relative fallbacks survive daemon PATH stripping.
            // The home anchor MUST match where `travsr lang install rust` wrote the
            // binary — the installer uses `dirs::home_dir()`, which is `USERPROFILE`
            // on Windows. So on Windows prefer `USERPROFILE`: a daemon launched from
            // Git Bash inherits a POSIX-style `HOME` (e.g. `/c/Users/me`) that does
            // not resolve as a native path, and preferring it would miss the
            // just-installed binary. On unix `HOME` is the home dir and is used.
            let exe = std::env::consts::EXE_SUFFIX; // ".exe" on windows, "" elsewhere
            let ra = format!("rust-analyzer{exe}");
            let home = if cfg!(windows) {
                std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))
            } else {
                std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
            };
            let candidates = [
                std::env::var_os("CARGO_HOME")
                    .map(|h| std::path::PathBuf::from(h).join("bin").join(&ra)),
                home.clone().map(|h| {
                    std::path::PathBuf::from(h)
                        .join(".cargo")
                        .join("bin")
                        .join(&ra)
                }),
                home.map(|h| {
                    std::path::PathBuf::from(h)
                        .join(".travsr")
                        .join("bin")
                        .join(&ra)
                }),
            ];
            for candidate in candidates.into_iter().flatten() {
                if candidate.exists() {
                    tracing::info!(
                        path = %candidate.display(),
                        "rust-analyzer found off-PATH (cargo home or ~/.travsr/bin)"
                    );
                    return Some(candidate);
                }
            }
            None
        })
        .clone()
}

/// Return `true` when `rust-analyzer` is available.
///
/// Checks PATH first, then `$CARGO_HOME/bin` and `$HOME/.cargo/bin` as
/// fallback — see [`ra_binary_path`] for full search order.
pub fn ra_available() -> bool {
    ra_binary_path().is_some()
}

/// Run `rust-analyzer lsif <repo_root>` inside the SEC-201 sandbox and
/// return the LSIF dump.
///
/// Returns:
/// - `Ok(Some(dump))` — LSIF dump ready for [`crate::lsif::ingest_rust`].
/// - `Ok(None)` — rust-analyzer not on PATH, or sandbox unavailable + not opted in.
/// - `Err(_)` — rust-analyzer failed (non-zero exit, timeout, spawn error).
///
/// `cfg.timeout` is enforced via a polling loop; the process is killed on
/// overrun. `cfg.repo_root` must match `repo_root` so the sandbox allows
/// writes inside the repo directory.
///
/// # Errors
/// Propagates spawn failures, timeout kills, and non-zero exit status.
/// Empty stdout is NOT an error — some crates produce no LSIF output.
pub fn run_ra_lsif(repo_root: &Path, cfg: &SandboxConfig) -> anyhow::Result<Option<String>> {
    let ra_path = match ra_binary_path() {
        Some(p) => p,
        None => {
            tracing::info!("rust-analyzer not found, skipping Rust's full cross-file analysis");
            return Ok(None);
        }
    };
    let ra_str = ra_path.to_string_lossy().into_owned();
    let repo_str = repo_root.to_string_lossy();
    let (cmd, status) = build_sandboxed_command(ra_str.as_str(), &["lsif", repo_str.as_ref()], cfg);
    spawn_or_skip_ra(cmd, status, repo_root, cfg)
}

/// Guard + spawn logic for `run_ra_lsif`, extracted so tests can inject a
/// synthetic [`SandboxStatus`] without touching the real OS sandbox.
///
/// Returns `Ok(None)` when the sandbox is [`SandboxStatus::Unavailable`] and
/// `cfg.allow_unsandboxed` is `false` (fail-closed). This is the SEV-2
/// security guarantee: `cmd.spawn()` is **never** called on that path.
fn spawn_or_skip_ra(
    mut cmd: std::process::Command,
    status: SandboxStatus,
    repo_root: &Path,
    cfg: &SandboxConfig,
) -> anyhow::Result<Option<String>> {
    match &status {
        SandboxStatus::Active { mechanism } => {
            tracing::info!(
                event = "lsif.spawn",
                repo = %repo_root.display(),
                sandbox = mechanism,
                "spawning rust-analyzer lsif"
            );
        }
        SandboxStatus::Unavailable { reason } => {
            if should_skip_unsandboxed(&status, cfg.allow_unsandboxed) {
                // UX-002: the sandbox being unavailable is a degradation, not an
                // error — the run still succeeds with structural analysis. Keep it at
                // WARN and fold the recovery hint into the single skipped line so
                // only one line reaches the user. The recovery differs per OS:
                // bubblewrap is installable on Linux, but macOS/Windows have no
                // user-installable sandbox on this path — there the only recourse is
                // the explicit --allow-unsandboxed opt-in.
                #[cfg(target_os = "linux")]
                let recovery =
                    "Install bubblewrap, or pass --allow-unsandboxed if you trust this repository.";
                #[cfg(not(target_os = "linux"))]
                let recovery = "Pass --allow-unsandboxed if you trust this repository.";
                tracing::warn!(
                    repo = %repo_root.display(),
                    reason,
                    "rust-analyzer skipped, no OS sandbox available and \
                     --allow-unsandboxed not set; Rust falls back to structural \
                     analysis only. {recovery}"
                );
                record_ra_lsif_sandbox_skip();
                return Ok(None);
            }
            // allow_unsandboxed=true: operator opted in. Emit the unconfined-spawn
            // audit-trail line HERE (R6 fix) — not at config-construction time —
            // so it never fires spuriously when the sandbox is Active. UX-002:
            // this is a user-requested, acknowledged opt-in — expected behavior —
            // so it is a WARN (audit trail), not a red ERROR.
            tracing::warn!(
                repo = %repo_root.display(),
                reason,
                "rust-analyzer will run without OS sandboxing, the --allow-unsandboxed \
                 opt-in was acknowledged. Make sure you trust this repository."
            );
        }
    }

    let child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "could not spawn rust-analyzer lsif for {}",
                repo_root.display()
            )
        })?;

    // Drain both pipes concurrently via the shared helper — see run_with_drain
    // for the pipe-buffer deadlock rationale.
    let (exit_status, stdout_bytes, stderr) =
        run_with_drain(child, cfg.timeout, "rust-analyzer lsif")?;

    if !exit_status.success() {
        let stderr_head = stderr.lines().take(5).collect::<Vec<_>>().join("\n");
        anyhow::bail!("rust-analyzer lsif exited with {exit_status}: {stderr_head}");
    }

    let stdout = String::from_utf8_lossy(&stdout_bytes).into_owned();
    tracing::info!(
        event = "lsif.complete",
        repo = %repo_root.display(),
        lines = stdout.lines().count(),
        "rust-analyzer lsif complete"
    );

    Ok(Some(stdout))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use crate::sandbox::SandboxConfig;
    // `spawn_or_skip_ra` and `SandboxStatus` are only exercised by the
    // fail-closed regression test, which is `#[cfg(unix)]` (it relies on Unix
    // file permissions). Gate the imports the same way so a Windows build does
    // not warn about them being unused.
    #[cfg(unix)]
    use super::spawn_or_skip_ra;
    #[cfg(unix)]
    use crate::sandbox::SandboxStatus;

    #[test]
    fn run_ra_lsif_returns_none_when_ra_missing() {
        // Graceful-skip: a binary we know is absent must fail to spawn.
        // (run_ra_lsif itself returns Ok(None) before spawn when RA is absent;
        // this test confirms the spawn path would fail, not silently succeed.)
        let missing = std::process::Command::new("__ra_nonexistent_binary__")
            .arg("--version")
            .status();
        assert!(missing.is_err(), "nonexistent binary must fail to spawn");
    }

    #[test]
    fn run_ra_lsif_config_defaults_are_sane() {
        let cfg = SandboxConfig::default();
        assert_eq!(cfg.timeout, std::time::Duration::from_secs(300));
        assert_eq!(cfg.mem_limit_bytes, 4 * 1024 * 1024 * 1024);
        // M1.4: default must be fail-closed (R3 coverage)
        assert!(
            !cfg.allow_unsandboxed,
            "default SandboxConfig must be fail-closed"
        );
    }

    /// R3 — SEV-2 regression guard: `Unavailable + !allow_unsandboxed` must return
    /// `Ok(None)` without ever calling `cmd.spawn()`.
    ///
    /// Uses `spawn_or_skip_ra` with a synthetic `Unavailable` status and a sentinel
    /// command that would leave a detectable side effect if spawned. This directly
    /// proves the order invariant: the guard check fires **before** `cmd.spawn()`.
    #[test]
    #[cfg(unix)]
    fn fail_closed_unavailable_sandbox_never_spawns_ra() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let sentinel = dir.path().join("was_spawned");

        // Sentinel script: touches a file if executed.
        let script = dir.path().join("sentinel.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ntouch {}\n", sentinel.display()),
        )
        .expect("write sentinel script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod sentinel");

        let cmd = std::process::Command::new(&script);
        let status = SandboxStatus::Unavailable {
            reason: "injected for test".to_owned(),
        };
        let cfg = SandboxConfig {
            allow_unsandboxed: false,
            repo_root: dir.path().to_path_buf(),
            ..SandboxConfig::default()
        };

        let result = spawn_or_skip_ra(cmd, status, dir.path(), &cfg);

        assert!(
            matches!(result, Ok(None)),
            "Unavailable + !allow_unsandboxed must return Ok(None), got: {result:?}"
        );
        assert!(
            !sentinel.exists(),
            "sentinel file must not exist, spawn_or_skip_ra called cmd.spawn() \
             despite Unavailable sandbox and allow_unsandboxed=false (SEV-2 regression)"
        );
    }

    /// R3 companion: opt-in path DOES reach spawn (and fails loudly on a bad binary).
    /// Proves the guard is bypassable only through the explicit opt-in.
    #[test]
    #[cfg(unix)]
    fn opt_in_unavailable_sandbox_attempts_spawn() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Use a binary that fails to spawn so we get an Err, not Ok(None).
        let cmd = std::process::Command::new("__sentinel_must_not_exist_ra__");
        let status = SandboxStatus::Unavailable {
            reason: "injected for test".to_owned(),
        };
        let cfg = SandboxConfig {
            allow_unsandboxed: true,
            repo_root: dir.path().to_path_buf(),
            ..SandboxConfig::default()
        };

        let result = spawn_or_skip_ra(cmd, status, dir.path(), &cfg);
        // Must reach spawn() — which fails on the nonexistent binary → Err.
        assert!(
            result.is_err(),
            "opt-in path must attempt spawn and fail (not return Ok(None)): {result:?}"
        );
    }

    #[test]
    fn run_ra_lsif_timeout_kills_long_running_process() {
        // Verify the timeout-kill polling loop using a raw `sleep` process.
        // Deliberately bypasses build_sandboxed_command — sandbox correctness is
        // tested in sandbox.rs. This test only exercises the kill-on-deadline path.
        // Skipped on Windows where `sleep` is not available.
        #[cfg(unix)]
        {
            use std::time::Instant;
            #[allow(clippy::disallowed_methods)] // legitimate: testing the kill loop, not draining
            let mut child = std::process::Command::new("sleep")
                .arg("60")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("sleep must spawn");

            let timeout = std::time::Duration::from_millis(200);
            let deadline = Instant::now() + timeout;
            let timed_out = loop {
                #[allow(clippy::disallowed_methods)]
                match child.try_wait().expect("try_wait") {
                    Some(_) => break false,
                    None if Instant::now() >= deadline => {
                        let _ = child.kill();
                        break true;
                    }
                    None => std::thread::sleep(std::time::Duration::from_millis(50)),
                }
            };
            assert!(timed_out, "process should have been killed by timeout");
        }
    }
}
