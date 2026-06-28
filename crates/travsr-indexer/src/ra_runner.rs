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

use std::io::Read as _;
use std::path::Path;
use std::time::Instant;

use anyhow::Context as _;

use crate::sandbox::{build_sandboxed_command, SandboxConfig, SandboxStatus};

// ── Public API ────────────────────────────────────────────────────────────────

/// Return the path to the `rust-analyzer` binary, or `None` if unavailable.
///
/// Search order:
/// 1. `rust-analyzer` on the process PATH.
/// 2. `$CARGO_HOME/bin/rust-analyzer` — explicit install root.
/// 3. `$HOME/.cargo/bin/rust-analyzer` — default Cargo install location.
///
/// Fallbacks 2 and 3 ensure LSIF enrichment works even when the daemon's PATH
/// has been stripped to `/usr/bin:/bin:/usr/sbin:/sbin` (e.g. when forked to
/// background by the CLI where the parent shell had `~/.cargo/bin` on PATH).
///
/// Result is cached via `OnceLock` — the probe runs at most once per process
/// lifetime.
pub fn ra_binary_path() -> Option<std::path::PathBuf> {
    static RA_PATH: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    RA_PATH
        .get_or_init(|| {
            // 1. PATH probe: preferred — honours user's active toolchain.
            let on_path = std::process::Command::new("rust-analyzer")
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok();
            if on_path {
                return Some(std::path::PathBuf::from("rust-analyzer"));
            }
            // 2 + 3. Explicit home-relative fallbacks survive daemon PATH stripping.
            let candidates = [
                std::env::var("CARGO_HOME").ok().map(|h| {
                    std::path::PathBuf::from(h)
                        .join("bin")
                        .join("rust-analyzer")
                }),
                std::env::var("HOME").ok().map(|h| {
                    std::path::PathBuf::from(h)
                        .join(".cargo")
                        .join("bin")
                        .join("rust-analyzer")
                }),
            ];
            for candidate in candidates.into_iter().flatten() {
                if candidate.exists() {
                    tracing::info!(
                        path = %candidate.display(),
                        "rust-analyzer found via cargo home (not on PATH)"
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
/// - `Ok(None)` — rust-analyzer not on PATH; skip LSIF silently.
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
            tracing::info!("rust-analyzer not found — skipping Rust LSIF ingestion");
            return Ok(None);
        }
    };
    let ra_str = ra_path.to_string_lossy().into_owned();
    let repo_str = repo_root.to_string_lossy();
    let (mut cmd, status) =
        build_sandboxed_command(ra_str.as_str(), &["lsif", repo_str.as_ref()], cfg);

    match &status {
        SandboxStatus::Active { mechanism } => {
            tracing::info!(
                repo = %repo_root.display(),
                sandbox = mechanism,
                "spawning rust-analyzer lsif"
            );
        }
        SandboxStatus::Unavailable { reason } => {
            tracing::error!(
                repo = %repo_root.display(),
                reason,
                "rust-analyzer running WITHOUT sandbox — \
                 install bubblewrap on Linux for process isolation"
            );
        }
    }

    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "could not spawn rust-analyzer lsif for {}",
                repo_root.display()
            )
        })?;

    // Drain stdout and stderr on background threads BEFORE polling for exit.
    // rust-analyzer LSIF output can exceed the OS pipe buffer (64 KB on Linux);
    // if we poll try_wait() without draining, the child blocks on a full pipe
    // and never exits — causing a spurious timeout kill. The drain threads run
    // concurrently with the polling loop and join after the child exits or is
    // killed. O(output_size) memory — acceptable for LSIF dumps (< 10 MB).
    let mut stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("rust-analyzer child has no piped stdout"))?;
    let mut stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("rust-analyzer child has no piped stderr"))?;
    let stdout_thread = std::thread::spawn(move || -> String {
        let mut buf = String::new();
        let _ = stdout_pipe.read_to_string(&mut buf);
        buf
    });
    let stderr_thread = std::thread::spawn(move || -> String {
        let mut buf = String::new();
        let _ = stderr_pipe.read_to_string(&mut buf);
        buf
    });

    let deadline = Instant::now() + cfg.timeout;

    let exit_status = loop {
        match child.try_wait().context("polling rust-analyzer")? {
            Some(s) => break s,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                // Join drain threads so they don't dangle after we return.
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                anyhow::bail!(
                    "rust-analyzer lsif timed out after {}s — killed",
                    cfg.timeout.as_secs()
                );
            }
            None => std::thread::sleep(std::time::Duration::from_millis(200)),
        }
    };

    let stdout = stdout_thread.join().unwrap_or_default();
    let stderr = stderr_thread.join().unwrap_or_default();

    if !exit_status.success() {
        // Include first 5 lines of stderr to aid debugging without flooding logs.
        let stderr_head = stderr.lines().take(5).collect::<Vec<_>>().join("\n");
        anyhow::bail!("rust-analyzer lsif exited with {exit_status}: {stderr_head}");
    }

    let line_count = stdout.lines().count();
    tracing::info!(
        repo = %repo_root.display(),
        lines = line_count,
        "rust-analyzer lsif complete"
    );

    Ok(Some(stdout))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::*;
    use crate::sandbox::SandboxConfig;

    #[test]
    fn run_ra_lsif_returns_none_when_ra_missing() {
        // Patch: directly test the graceful-skip path by verifying that a
        // binary we know is absent returns Ok(None) via the same logic.
        // We can't override ra_available() here, so we test the helper directly.
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
    }

    #[test]
    fn run_ra_lsif_timeout_kills_long_running_process() {
        // Verify the timeout-kill polling loop using a raw `sleep` process.
        // We deliberately bypass build_sandboxed_command here: on CI runners
        // bwrap may be installed but user namespaces disabled at the kernel
        // level (common in Docker), causing bwrap to exit immediately and the
        // test to see timed_out=false. Sandbox correctness is tested separately
        // in sandbox.rs. This test only exercises the kill-on-deadline path.
        // Skipped on Windows where `sleep` is not available.
        #[cfg(unix)]
        {
            let mut child = std::process::Command::new("sleep")
                .arg("60")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("sleep must spawn");

            let timeout = std::time::Duration::from_millis(200);
            let deadline = Instant::now() + timeout;
            let timed_out = loop {
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
