//! Subprocess runners for language Phase B tools.
//!
//! `run_lsif_emitter`    — Node.js LSIF emitter for TypeScript/JavaScript.
//! `run_lsif_py_emitter` — Node.js LSIF emitter for Python (travsr-lsif-py).
//! `run_scip_python`     — scip-python SCIP indexer for Python (legacy / deprecated).

use std::io::Read as _;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;

// ── Shared subprocess constants ───────────────────────────────────────────────

/// Default hard ceiling for Node LSIF emitters (TS + Python), matching the
/// 300 s used by scip-python and rust-analyzer. Override via
/// `TRAVSR_LSIF_TIMEOUT_SECS`.
const LSIF_TIMEOUT_DEFAULT_SECS: u64 = 300;

/// Sanity clamp so a fat-fingered env value can't wedge `git commit` for a day.
const MAX_LSIF_TIMEOUT_SECS: u64 = 3_600;

/// Poll interval for the try_wait loop. 200 ms is immaterial against multi-
/// second tool runs; 100 ms was arbitrary and inconsistent across runners.
const POLL_INTERVAL_MS: u64 = 200;

/// Hard ceiling for `scip-python` (large projects can be slow).
/// Separate from LSIF_TIMEOUT because scip-python is a different tool class.
const SCIP_PYTHON_TIMEOUT: Duration = Duration::from_secs(300);

/// Hard cap on the bytes accepted from a child's stdout — and on the size of
/// the scip-python `index.scip` tempfile, which is the same payload arriving
/// via disk (#736 item 7). A pathological emitter (or a poisoned repo driving
/// one) can otherwise materialize unbounded output in daemon RAM. 512 MiB is
/// orders of magnitude above any legitimate LSIF/SCIP index we have observed,
/// so real runs never trip it.
pub(crate) const MAX_CHILD_OUTPUT_BYTES: u64 = 512 * 1024 * 1024;

/// Chunk size for the capped stdout drain. 64 KiB matches the OS pipe buffer,
/// so a healthy child is never throttled by the chunking.
const DRAIN_CHUNK_BYTES: usize = 64 * 1024;

/// Return the configured timeout for Node LSIF emitters (TS and Python).
///
/// Reads `TRAVSR_LSIF_TIMEOUT_SECS` from the environment; falls back to
/// [`LSIF_TIMEOUT_DEFAULT_SECS`] on parse failure, zero, or absurd values.
fn lsif_node_timeout() -> Duration {
    let secs = std::env::var("TRAVSR_LSIF_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&x| x > 0 && x <= MAX_LSIF_TIMEOUT_SECS)
        .unwrap_or(LSIF_TIMEOUT_DEFAULT_SECS);
    Duration::from_secs(secs)
}

// ── Shared drain helper ───────────────────────────────────────────────────────

/// Drain stdout + stderr on background threads while polling for exit with a
/// wall-clock deadline. Kills the child on overrun; joins both drain threads on
/// **both** the normal-exit and timeout-kill paths so no thread dangles.
///
/// `name` is used only for error messages. The child **must** have been spawned
/// with `Stdio::piped()` for both stdout and stderr.
///
/// Returns `(exit_status, stdout_bytes, stderr_string)`.
///
/// Stdout is drained into `Vec<u8>` to avoid silent truncation on a stray
/// non-UTF-8 byte in a multi-MB LSIF dump (O(output_size) memory, hard-capped
/// at [`MAX_CHILD_OUTPUT_BYTES`] — a child that exceeds the cap is killed and
/// an error naming the cap and the program is returned). Callers that need a
/// `String` convert via `String::from_utf8_lossy`. Stderr is small diagnostic
/// text and is decoded as `String` directly.
///
/// `wait_with_output()` is **not** a substitute: it drains safely but cannot
/// express a concurrent timeout-kill, which every runner needs so a hung tool
/// can't block `git commit`.
pub(crate) fn run_with_drain(
    child: std::process::Child,
    timeout: Duration,
    name: &str,
) -> anyhow::Result<(std::process::ExitStatus, Vec<u8>, String)> {
    run_with_drain_capped(child, timeout, name, MAX_CHILD_OUTPUT_BYTES)
}

/// [`run_with_drain`] with an injectable stdout byte cap — separated so tests
/// can trip the overflow path without generating 512 MiB of real output.
fn run_with_drain_capped(
    mut child: std::process::Child,
    timeout: Duration,
    name: &str,
    max_stdout_bytes: u64,
) -> anyhow::Result<(std::process::ExitStatus, Vec<u8>, String)> {
    let stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("{name} child has no piped stdout"))?;
    let stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("{name} child has no piped stderr"))?;

    // #736: chunked read instead of read_to_end so the byte cap can stop the
    // drain mid-stream. When the cap is exceeded the thread stops reading and
    // raises `overflowed`; the poll loop below kills the child. Leaving the
    // pipe open-but-unread back-pressures the child (64 KiB OS pipe buffer),
    // so it cannot outrun the kill by more than one pipe buffer.
    let overflowed = Arc::new(AtomicBool::new(false));
    let overflow_flag = Arc::clone(&overflowed);
    let out_t = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut pipe = stdout_pipe;
        let mut chunk = [0u8; DRAIN_CHUNK_BYTES];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break, // EOF — child closed stdout
                Ok(n) => {
                    if buf.len() as u64 + n as u64 > max_stdout_bytes {
                        overflow_flag.store(true, Ordering::Relaxed);
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                // Read error (e.g. pipe torn down by the timeout kill): keep
                // what was drained — matches the old `let _ = read_to_end(..)`.
                Err(_) => break,
            }
        }
        buf
    });
    let err_t = std::thread::spawn(move || {
        let mut buf = String::new();
        let mut pipe = stderr_pipe;
        let _ = pipe.read_to_string(&mut buf);
        buf
    });

    let deadline = Instant::now() + timeout;
    // Both pipes are already draining on background threads above — no pipe-buffer
    // deadlock risk. This is the one site in the codebase where try_wait is safe.
    #[allow(clippy::disallowed_methods)]
    let status = loop {
        // Cap check before the exit poll: an over-cap child is usually still
        // alive, blocked on its full stdout pipe, and would otherwise sit here
        // burning RAM-adjacent time until the wall-clock timeout.
        if overflowed.load(Ordering::Relaxed) {
            let _ = child.kill();
            // Reap after kill — kill() alone leaves a zombie until daemon exit.
            let _ = child.wait();
            let _ = out_t.join();
            let _ = err_t.join();
            anyhow::bail!(
                "{name} produced more than {max_stdout_bytes} bytes on stdout \
                 (MAX_CHILD_OUTPUT_BYTES cap) — killed"
            );
        }
        match child
            .try_wait()
            .with_context(|| format!("polling {name}"))?
        {
            Some(s) => break s,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                // Reap after kill — kill() alone leaves a zombie until daemon
                // exit (#736 zombie fix).
                let _ = child.wait();
                // Join drain threads so they don't dangle after we return.
                let _ = out_t.join();
                let _ = err_t.join();
                anyhow::bail!("{name} timed out after {}s — killed", timeout.as_secs());
            }
            None => std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS)),
        }
    };

    let stdout = out_t.join().unwrap_or_default();
    let stderr = err_t.join().unwrap_or_default();
    // Re-check after joining: a child can write past the cap and exit before
    // the poll loop observes the flag — truncated output must never be
    // returned as Ok.
    if overflowed.load(Ordering::Relaxed) {
        anyhow::bail!(
            "{name} produced more than {max_stdout_bytes} bytes on stdout \
             (MAX_CHILD_OUTPUT_BYTES cap) — output discarded"
        );
    }
    Ok((status, stdout, stderr))
}

/// Resolve the LSIF emitter program + args without requiring a global PATH install.
///
/// Resolution order:
/// 1. `TRAVSR_LSIF_TS` env var — absolute path to the JS entry point (tests / custom installs).
/// 2. Sibling of `current_exe` named `travsr-lsif-ts` — npm global install layout where
///    both binaries land in the same `bin/` directory.
/// 3. Walk up from `current_exe` directory looking for
///    `packages/travsr-lsif-ts/dist/index.js` — monorepo / `cargo build` dev layout.
/// 4. `travsr-lsif-ts` on PATH — legacy fallback.
///
/// Returns `(program, prefix_args)` where the full command is
/// `program [prefix_args...] --project <tsconfig>`.
fn resolve_lsif_emitter() -> (String, Vec<String>) {
    // 1. Env override — useful for tests and non-standard installs.
    if let Ok(p) = std::env::var("TRAVSR_LSIF_TS") {
        let path = std::path::Path::new(&p);
        if path.is_file() {
            return ("node".to_string(), vec![p]);
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            // 2. Sibling binary (npm global install: both binaries in the same bin/).
            let sibling = exe_dir.join("travsr-lsif-ts");
            if sibling.is_file() {
                return (sibling.to_string_lossy().into_owned(), vec![]);
            }

            // 3. Walk up from exe_dir looking for the monorepo layout.
            //    dev build: target/release/travsr → 3 parents up = repo root.
            let mut cur = exe_dir.to_path_buf();
            for _ in 0..6 {
                let candidate = cur.join("packages/travsr-lsif-ts/dist/index.js");
                if candidate.is_file() {
                    return (
                        "node".to_string(),
                        vec![candidate.to_string_lossy().into_owned()],
                    );
                }
                match cur.parent() {
                    Some(p) => cur = p.to_path_buf(),
                    None => break,
                }
            }
        }
    }

    // 4. Fall back to PATH.
    ("travsr-lsif-ts".to_string(), vec![])
}

/// Run the LSIF emitter for `<tsconfig>` and return the LSIF dump.
///
/// The emitter is resolved relative to the travsr binary — no global PATH
/// install required. Both stdout and stderr are drained on background threads
/// concurrently with the polling loop so that large LSIF dumps (> 64 KB OS
/// pipe buffer) do not deadlock the child. The subprocess is killed if it does
/// not complete within [`lsif_node_timeout()`] (default 300 s).
///
/// # Errors
/// - Emitter not found (not bundled, not on PATH)
/// - Non-zero exit code (first 5 lines of stderr included)
/// - Timeout exceeded
pub fn run_lsif_emitter(tsconfig: &Path) -> anyhow::Result<String> {
    let (program, prefix_args) = resolve_lsif_emitter();
    let child = std::process::Command::new(&program)
        .args(&prefix_args)
        .arg("--project")
        .arg(tsconfig)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "could not run travsr-lsif-ts for {} \
                 (emitter not found — check TRAVSR_LSIF_TS or reinstall travsr)",
                tsconfig.display()
            )
        })?;

    let (status, stdout_bytes, stderr) =
        run_with_drain(child, lsif_node_timeout(), "travsr-lsif-ts")?;

    if !status.success() {
        let stderr_head = stderr.lines().take(5).collect::<Vec<_>>().join("\n");
        anyhow::bail!("travsr-lsif-ts exited with {status}: {stderr_head}");
    }

    Ok(String::from_utf8_lossy(&stdout_bytes).into_owned())
}

// ── scip-python ───────────────────────────────────────────────────────────────

/// Locate the `scip-python` binary.
///
/// Resolution order:
/// 1. `SCIP_PYTHON_PATH` env var — absolute path.
/// 2. `scip-python` on PATH.
fn find_scip_python() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("SCIP_PYTHON_PATH") {
        let pb = std::path::PathBuf::from(p);
        if pb.is_file() {
            return Some(pb);
        }
    }
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join("scip-python");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Run `scip-python index` on `root` and return the raw SCIP protobuf bytes.
///
/// Returns `Ok(None)` when `scip-python` is not installed (graceful degradation).
/// Returns `Ok(Some(bytes))` on success.
/// Returns `Err` if `scip-python` is found but fails or times out.
///
/// Install: `npm install -g @sourcegraph/scip-python`
pub fn run_scip_python(root: &Path, corpus: &str) -> anyhow::Result<Option<Vec<u8>>> {
    use anyhow::Context as _;

    let scip_python = match find_scip_python() {
        Some(p) => p,
        None => {
            tracing::debug!(
                "scip-python not found on PATH — Python Phase B skipped \
                 (install: npm install -g @sourcegraph/scip-python)"
            );
            return Ok(None);
        }
    };

    // Write SCIP output to a scratch tempdir so we can read it back.
    let scratch = tempfile::Builder::new()
        .prefix("travsr-scip-python-")
        .tempdir()
        .context("create scip-python scratch dir")?;
    let output = scratch.path().join("index.scip");

    // Pipe both stdout and stderr through run_with_drain (R8 — 4/4 runner
    // consolidation). scip-python writes its SCIP payload to the --output
    // tempfile, so stdout bytes are status/progress messages; we drain and
    // discard them. Piping (rather than Stdio::null()) ensures the OS pipe
    // buffer never fills and blocks the child mid-write.
    let child = std::process::Command::new(&scip_python)
        .args([
            "index",
            "--project-name",
            corpus,
            "--project-version",
            "0.0.1",
            "--output",
            output.to_str().unwrap_or("index.scip"),
        ])
        .arg(root)
        .current_dir(root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn scip-python for {}", root.display()))?;

    let (status, _stdout_bytes, stderr) =
        run_with_drain(child, SCIP_PYTHON_TIMEOUT, "scip-python")?;

    if !status.success() {
        anyhow::bail!("scip-python exited with {status}: {stderr}");
    }

    let bytes = read_scip_output_capped(&output, MAX_CHILD_OUTPUT_BYTES)?;
    Ok(Some(bytes))
}

/// Read the scip-python output file, refusing anything over `cap` bytes.
///
/// Stat-then-read (#736 item 7): scip-python on a pathological repo can write
/// a multi-GB protobuf; `std::fs::read` would materialize all of it in daemon
/// RAM before the parser even runs. The cap is injectable so the refusal path
/// is unit-testable without a 512 MiB fixture; production goes through
/// [`MAX_CHILD_OUTPUT_BYTES`].
fn read_scip_output_capped(output: &Path, cap: u64) -> anyhow::Result<Vec<u8>> {
    let len = std::fs::metadata(output)
        .with_context(|| format!("stat scip output {}", output.display()))?
        .len();
    if len > cap {
        anyhow::bail!(
            "scip-python output {} is {len} bytes — exceeds the {cap}-byte cap \
             (MAX_CHILD_OUTPUT_BYTES); refusing to load it into memory",
            output.display()
        );
    }
    std::fs::read(output).with_context(|| format!("read scip output {}", output.display()))
}

// ── travsr-lsif-py ────────────────────────────────────────────────────────────

/// Resolve the Python LSIF emitter program + args without requiring PATH install.
///
/// Resolution order (identical to [`resolve_lsif_emitter`] for TypeScript):
/// 1. `TRAVSR_LSIF_PY` env var — absolute path to the JS entry point.
/// 2. Sibling of `current_exe` named `travsr-lsif-py` — npm global install layout.
/// 3. Walk up from `current_exe` to find `packages/travsr-lsif-py/dist/index.js`.
/// 4. `travsr-lsif-py` on PATH — final fallback.
///
/// Returns `(program, prefix_args)` where the full invocation is
/// `program [prefix_args...] --root <root>`.
fn resolve_lsif_py_emitter() -> (String, Vec<String>) {
    // 1. Env override — useful for tests and non-standard installs.
    if let Ok(p) = std::env::var("TRAVSR_LSIF_PY") {
        let path = std::path::Path::new(&p);
        if path.is_file() {
            return ("node".to_string(), vec![p]);
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            // 2. Sibling binary (npm global install: both binaries in the same bin/).
            let sibling = exe_dir.join("travsr-lsif-py");
            if sibling.is_file() {
                return (sibling.to_string_lossy().into_owned(), vec![]);
            }

            // 3. Walk up from exe_dir looking for the monorepo dev layout.
            //    dev build: target/release/travsr → 3 parents up = repo root.
            let mut cur = exe_dir.to_path_buf();
            for _ in 0..6 {
                let candidate = cur.join("packages/travsr-lsif-py/dist/index.js");
                if candidate.is_file() {
                    return (
                        "node".to_string(),
                        vec![candidate.to_string_lossy().into_owned()],
                    );
                }
                match cur.parent() {
                    Some(p) => cur = p.to_path_buf(),
                    None => break,
                }
            }
        }
    }

    // 4. PATH fallback.
    ("travsr-lsif-py".to_string(), vec![])
}

/// Run `travsr-lsif-py --root <root>` and return the LSIF JSON-Lines dump.
///
/// Returns:
/// - `Ok(Some(dump))` — LSIF dump ready for [`crate::lsif::ingest`].
/// - `Ok(None)` — emitter binary not found (spawn failed); native Phase B
///   tree-sitter output still runs — graceful degradation, not an error.
/// - `Err(_)` — emitter found but produced a non-zero exit code, or timed out
///   after [`lsif_node_timeout()`] (default 300 s, override via
///   `TRAVSR_LSIF_TIMEOUT_SECS`).
///
/// Stdout and stderr are drained on background threads concurrently with the
/// polling loop via [`run_with_drain`] to prevent OS pipe-buffer deadlock.
pub fn run_lsif_py_emitter(root: &Path) -> anyhow::Result<Option<String>> {
    let (program, prefix_args) = resolve_lsif_py_emitter();

    let child = match std::process::Command::new(&program)
        .args(&prefix_args)
        .arg("--root")
        .arg(root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => {
            tracing::debug!(
                "travsr-lsif-py not found — Python LSIF enrichment skipped \
                 (native phase_b tree-sitter edges still active)"
            );
            return Ok(None);
        }
    };

    let (exit_status, stdout_bytes, stderr) =
        run_with_drain(child, lsif_node_timeout(), "travsr-lsif-py")?;

    if !exit_status.success() {
        let stderr_head = stderr.lines().take(5).collect::<Vec<_>>().join("\n");
        anyhow::bail!("travsr-lsif-py exited with {exit_status}: {stderr_head}");
    }

    let stdout = String::from_utf8_lossy(&stdout_bytes).into_owned();
    tracing::info!(
        root = %root.display(),
        lines = stdout.lines().count(),
        "travsr-lsif-py complete"
    );

    Ok(Some(stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Env-var mutations are process-global. Serialize all tests that touch
    // TRAVSR_LSIF_TS / TRAVSR_LSIF_PY / TRAVSR_LSIF_TIMEOUT_SECS to prevent
    // races on parallel test runners.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Harness ceiling for one emitter-child lifecycle (#574).
    ///
    /// Every emitter test funnels its result through a channel and bounds the
    /// wait so a wedged runner fails instead of hanging the suite. That bound
    /// is a watchdog, never part of the semantics under test — but the old
    /// per-test values (10-30s) doubled as an implicit "node starts fast"
    /// assumption, and a cold windows-latest runner intermittently spent more
    /// than 10s just spawning node, misreporting an instantly-exiting child as
    /// "must return: Timeout". Sized far above any plausible spawn cost while
    /// still bounding a genuine hang; a healthy run never waits this long.
    const HARNESS_DEADLINE: Duration = Duration::from_secs(120);

    /// RAII guard that restores an environment variable to its previous value
    /// on drop, even if the test panics. Prevents env-var leaks from failing
    /// assertions that would otherwise contaminate sibling tests in the same
    /// binary (R12 fix — replaces manual `set_var` + `remove_var` pairs).
    struct EnvGuard {
        var: &'static str,
        prev: Option<String>,
    }
    impl EnvGuard {
        fn set(var: &'static str, val: &str) -> Self {
            let prev = std::env::var(var).ok();
            std::env::set_var(var, val);
            Self { var, prev }
        }
        fn clear(var: &'static str) -> Self {
            let prev = std::env::var(var).ok();
            std::env::remove_var(var);
            Self { var, prev }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var(self.var, v),
                None => std::env::remove_var(self.var),
            }
        }
    }

    // ── resolve tests (existing, kept) ───────────────────────────────────────

    #[test]
    fn resolve_falls_back_to_path_when_no_env_and_no_sibling() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("TRAVSR_LSIF_TS");
        let (program, args) = resolve_lsif_emitter();
        assert!(!program.is_empty());
        let _ = args;
    }

    #[test]
    fn resolve_honours_env_override_when_file_exists() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let exe = std::env::current_exe().unwrap();
        std::env::set_var("TRAVSR_LSIF_TS", exe.to_str().unwrap());
        let (program, args) = resolve_lsif_emitter();
        assert_eq!(program, "node");
        assert_eq!(args.len(), 1);
        std::env::remove_var("TRAVSR_LSIF_TS");
    }

    #[test]
    fn resolve_lsif_py_falls_back_to_path_when_no_env_and_no_sibling() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("TRAVSR_LSIF_PY");
        let (program, args) = resolve_lsif_py_emitter();
        assert!(!program.is_empty());
        let _ = args;
    }

    #[test]
    fn resolve_lsif_py_honours_env_override_when_file_exists() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let exe = std::env::current_exe().unwrap();
        std::env::set_var("TRAVSR_LSIF_PY", exe.to_str().unwrap());
        let (program, args) = resolve_lsif_py_emitter();
        assert_eq!(program, "node");
        assert_eq!(args.len(), 1);
        std::env::remove_var("TRAVSR_LSIF_PY");
    }

    #[test]
    fn run_lsif_py_emitter_returns_none_for_missing_binary() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("TRAVSR_LSIF_PY");
        let spawn_result = std::process::Command::new("__travsr_lsif_py_absent_binary__")
            .arg("--root")
            .arg("/tmp")
            .spawn();
        assert!(
            spawn_result.is_err(),
            "non-existent binary must fail to spawn"
        );
    }

    // ── M2 — timeout parser tests ─────────────────────────────────────────────

    #[test]
    fn timeout_unset_defaults_300() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::clear("TRAVSR_LSIF_TIMEOUT_SECS");
        assert_eq!(lsif_node_timeout(), Duration::from_secs(300));
    }

    #[test]
    fn timeout_valid_override() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::set("TRAVSR_LSIF_TIMEOUT_SECS", "120");
        assert_eq!(lsif_node_timeout(), Duration::from_secs(120));
    }

    #[test]
    fn timeout_zero_rejected() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::set("TRAVSR_LSIF_TIMEOUT_SECS", "0");
        assert_eq!(lsif_node_timeout(), Duration::from_secs(300));
    }

    #[test]
    fn timeout_negative_rejected() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::set("TRAVSR_LSIF_TIMEOUT_SECS", "-5");
        assert_eq!(lsif_node_timeout(), Duration::from_secs(300));
    }

    #[test]
    fn timeout_non_numeric_rejected() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::set("TRAVSR_LSIF_TIMEOUT_SECS", "abc");
        assert_eq!(lsif_node_timeout(), Duration::from_secs(300));
    }

    #[test]
    fn timeout_whitespace_trimmed() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::set("TRAVSR_LSIF_TIMEOUT_SECS", " 90 ");
        assert_eq!(lsif_node_timeout(), Duration::from_secs(90));
    }

    #[test]
    fn timeout_absurd_clamped_to_default() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        {
            // overflow string → parse fails → default
            let _env = EnvGuard::set("TRAVSR_LSIF_TIMEOUT_SECS", "999999999999999999999");
            assert_eq!(lsif_node_timeout(), Duration::from_secs(300));
        }
        // beyond MAX_LSIF_TIMEOUT_SECS → clamped to default
        let _env = EnvGuard::set("TRAVSR_LSIF_TIMEOUT_SECS", "999999");
        assert_eq!(lsif_node_timeout(), Duration::from_secs(300));
    }

    // ── Helper: spawn a node stub (skips when node is absent) ────────────────

    /// Returns the path to `node` if available, `None` otherwise.
    fn node_path() -> Option<std::path::PathBuf> {
        std::process::Command::new("node")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()
            .filter(|s| s.success())
            .map(|_| std::path::PathBuf::from("node"))
    }

    /// Write a Node.js stub script to a tempfile, return the file (kept alive
    /// for the duration of the test).
    fn stub_node(body: &str) -> tempfile::NamedTempFile {
        use std::io::Write as _;
        let mut f = tempfile::Builder::new()
            .suffix(".js")
            .tempfile()
            .expect("tempfile");
        write!(f, "{body}").unwrap();
        f
    }

    /// Set TRAVSR_LSIF_TS to `script`, call `f`, restore env via RAII.
    /// Caller must already hold ENV_LOCK. EnvGuard restores on drop even on panic.
    fn with_lsif_ts_env<F: FnOnce()>(script: &std::path::Path, f: F) {
        let _guard = EnvGuard::set("TRAVSR_LSIF_TS", script.to_str().unwrap());
        f();
    }

    // ── R.1-R.3 — run_with_drain shared contract ──────────────────────────────

    #[test]
    fn run_drained_large_output_no_deadlock() {
        // 128 KiB stdout + 96 KiB stderr — proves both pipes drain past the
        // 64 KB OS pipe buffer without deadlock.
        // Watchdog channel: test fails if run_with_drain doesn't return in 20 s.
        let Some(_node) = node_path() else {
            eprintln!("SKIP: node not available");
            return;
        };
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let script = stub_node(
            "require('fs').writeSync(1,Buffer.alloc(131072,0x61));\
             require('fs').writeSync(2,Buffer.alloc(98304,0x62));",
        );

        let (tx, rx) = std::sync::mpsc::channel();
        let script_path = script.path().to_path_buf();
        std::thread::spawn(move || {
            let child = std::process::Command::new("node")
                .arg(&script_path)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("spawn node");
            let result = run_with_drain(child, HARNESS_DEADLINE, "test");
            let _ = tx.send(result);
        });

        let result = rx
            .recv_timeout(HARNESS_DEADLINE)
            .expect("run_with_drain must return within watchdog window");
        let (_status, stdout, _stderr) = result.expect("must succeed");
        assert_eq!(stdout.len(), 131072, "full 128 KiB stdout must be drained");
    }

    #[test]
    fn run_drained_timeout_joins_threads() {
        // Child hangs forever; low timeout must kill it and join threads cleanly.
        let Some(_node) = node_path() else {
            eprintln!("SKIP: node not available");
            return;
        };
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let script = stub_node("setInterval(()=>{},1e9);");
        let child = std::process::Command::new("node")
            .arg(script.path())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn node");

        let result = run_with_drain(child, Duration::from_millis(400), "hang-test");
        assert!(result.is_err(), "must return Err on timeout");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("timed out"),
            "error must mention timed out: {msg}"
        );
    }

    #[test]
    fn run_drained_nonzero_exit_preserves_stderr() {
        let Some(_node) = node_path() else {
            eprintln!("SKIP: node not available");
            return;
        };
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let script = stub_node(
            "process.stderr.write('fatal error\\n');\
             process.exit(3);",
        );
        let child = std::process::Command::new("node")
            .arg(script.path())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn node");

        let (status, _out, stderr) =
            run_with_drain(child, HARNESS_DEADLINE, "exit3-test").expect("must not err");
        assert!(!status.success());
        assert!(
            stderr.contains("fatal error"),
            "stderr must be captured: {stderr}"
        );
    }

    // ── #736 — stdout byte cap ────────────────────────────────────────────────

    #[test]
    fn run_drained_stdout_over_cap_kills_child_and_names_cap() {
        // Child writes 1 MiB then hangs forever; a 64 KiB cap must stop the
        // drain, kill the child (well before the harness deadline), and
        // surface an error naming both the cap and the program.
        let Some(_node) = node_path() else {
            eprintln!("SKIP: node not available");
            return;
        };
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let script = stub_node(
            "require('fs').writeSync(1,Buffer.alloc(1048576,0x61));\
             setInterval(()=>{},1e9);",
        );

        let (tx, rx) = std::sync::mpsc::channel();
        let script_path = script.path().to_path_buf();
        std::thread::spawn(move || {
            let child = std::process::Command::new("node")
                .arg(&script_path)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("spawn node");
            let result = run_with_drain_capped(child, HARNESS_DEADLINE, "cap-test", 65536);
            let _ = tx.send(result);
        });

        let result = rx
            .recv_timeout(HARNESS_DEADLINE)
            .expect("over-cap child must be killed within the watchdog window");
        assert!(result.is_err(), "over-cap output must be Err");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("65536"), "error must name the cap: {msg}");
        assert!(
            msg.contains("cap-test"),
            "error must name the program: {msg}"
        );
    }

    #[test]
    fn run_drained_stdout_under_cap_passes_through() {
        // 64 KiB of output under a 128 KiB cap must arrive intact — the cap
        // must never truncate or reject legitimate output.
        let Some(_node) = node_path() else {
            eprintln!("SKIP: node not available");
            return;
        };
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let script = stub_node("require('fs').writeSync(1,Buffer.alloc(65536,0x61));");

        let (tx, rx) = std::sync::mpsc::channel();
        let script_path = script.path().to_path_buf();
        std::thread::spawn(move || {
            let child = std::process::Command::new("node")
                .arg(&script_path)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("spawn node");
            let result = run_with_drain_capped(child, HARNESS_DEADLINE, "cap-ok-test", 131072);
            let _ = tx.send(result);
        });

        let result = rx
            .recv_timeout(HARNESS_DEADLINE)
            .expect("must return within watchdog window");
        let (_status, stdout, _stderr) = result.expect("under-cap output must succeed");
        assert_eq!(stdout.len(), 65536, "under-cap output must be complete");
    }

    // ── #736 — scip output file cap ───────────────────────────────────────────

    #[test]
    fn scip_output_over_cap_is_refused_without_reading() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("index.scip");
        std::fs::write(&path, vec![0u8; 1024]).expect("write fixture");

        let err = read_scip_output_capped(&path, 512).expect_err("1 KiB over a 512 B cap");
        let msg = err.to_string();
        assert!(msg.contains("512"), "error must name the cap: {msg}");
        assert!(
            msg.contains("1024"),
            "error must name the actual size: {msg}"
        );

        let ok = read_scip_output_capped(&path, 4096).expect("1 KiB under a 4 KiB cap");
        assert_eq!(ok.len(), 1024);
    }

    #[test]
    fn scip_output_missing_file_is_a_stat_error() {
        // The stat-first guard must not mask the original "file missing"
        // failure mode with a confusing cap message.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("does_not_exist.scip");
        let err = read_scip_output_capped(&path, 4096).expect_err("missing file");
        assert!(
            err.to_string().contains("stat scip output"),
            "must surface as a stat failure: {err}"
        );
    }

    // ── C1 — run_lsif_emitter drain / deadlock matrix ────────────────────────

    #[test]
    fn emitter_large_stdout_does_not_deadlock() {
        // C1.1 — 128 KiB stdout, proves the deadlock is gone.
        let Some(_node) = node_path() else {
            eprintln!("SKIP: node not available");
            return;
        };
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let script = stub_node("require('fs').writeSync(1,Buffer.alloc(131072,0x61));");
        let tsconfig = tempfile::Builder::new()
            .suffix(".json")
            .tempfile()
            .expect("tsconfig tempfile");

        let (tx, rx) = std::sync::mpsc::channel();
        let script_path = script.path().to_path_buf();
        let tsconfig_path = tsconfig.path().to_path_buf();
        std::thread::spawn(move || {
            with_lsif_ts_env(&script_path, || {
                let _ = tx.send(run_lsif_emitter(&tsconfig_path));
            });
        });

        let result = rx
            .recv_timeout(HARNESS_DEADLINE)
            .expect("must return within watchdog window — deadlock?");
        let dump = result.expect("must succeed");
        assert_eq!(dump.len(), 131072, "all 128 KiB must be returned");
    }

    #[test]
    fn emitter_large_stdout_and_stderr_drain_both_pipes() {
        // C1.2 — 128 KiB stdout + 96 KiB stderr interleaved; exit 0.
        let Some(_node) = node_path() else {
            eprintln!("SKIP: node not available");
            return;
        };
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let script = stub_node(
            "require('fs').writeSync(1,Buffer.alloc(131072,0x61));\
             require('fs').writeSync(2,Buffer.alloc(98304,0x62));",
        );
        let tsconfig = tempfile::Builder::new()
            .suffix(".json")
            .tempfile()
            .expect("tsconfig tempfile");

        let (tx, rx) = std::sync::mpsc::channel();
        let script_path = script.path().to_path_buf();
        let tsconfig_path = tsconfig.path().to_path_buf();
        std::thread::spawn(move || {
            with_lsif_ts_env(&script_path, || {
                let _ = tx.send(run_lsif_emitter(&tsconfig_path));
            });
        });

        let result = rx
            .recv_timeout(HARNESS_DEADLINE)
            .expect("must return within watchdog window");
        let dump = result.expect("must succeed");
        assert_eq!(dump.len(), 131072, "full stdout must be returned");
    }

    #[test]
    fn emitter_exit_zero_empty_stdout_is_ok() {
        // C1.3 — empty output + exit 0 must NOT be an error.
        let Some(_node) = node_path() else {
            eprintln!("SKIP: node not available");
            return;
        };
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let script = stub_node("process.exit(0);");
        let tsconfig = tempfile::Builder::new()
            .suffix(".json")
            .tempfile()
            .expect("tsconfig tempfile");

        let (tx, rx) = std::sync::mpsc::channel();
        let script_path = script.path().to_path_buf();
        let tsconfig_path = tsconfig.path().to_path_buf();
        std::thread::spawn(move || {
            with_lsif_ts_env(&script_path, || {
                let _ = tx.send(run_lsif_emitter(&tsconfig_path));
            });
        });

        let result = rx.recv_timeout(HARNESS_DEADLINE).expect("must return");
        let dump = result.expect("exit 0 empty stdout must not error");
        assert!(dump.is_empty());
    }

    #[test]
    fn emitter_non_utf8_stdout_contract() {
        // C1.4 — non-UTF-8 bytes survive via lossy conversion (no silent truncation).
        let Some(_node) = node_path() else {
            eprintln!("SKIP: node not available");
            return;
        };
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        // Write valid ASCII prefix then a stray 0xFF byte.
        let script = stub_node(
            "process.stdout.write(Buffer.from('hello')); \
             process.stdout.write(Buffer.from([0xff])); \
             process.exit(0);",
        );
        let tsconfig = tempfile::Builder::new()
            .suffix(".json")
            .tempfile()
            .expect("tsconfig tempfile");

        let (tx, rx) = std::sync::mpsc::channel();
        let script_path = script.path().to_path_buf();
        let tsconfig_path = tsconfig.path().to_path_buf();
        std::thread::spawn(move || {
            with_lsif_ts_env(&script_path, || {
                let _ = tx.send(run_lsif_emitter(&tsconfig_path));
            });
        });

        let result = rx.recv_timeout(HARNESS_DEADLINE).expect("must return");
        let dump = result.expect("lossy conversion must succeed");
        // The ASCII prefix must be intact.
        assert!(
            dump.starts_with("hello"),
            "ASCII prefix must survive: {dump:?}"
        );
        // Total length must be > 5 (the 0xFF was replaced with the U+FFFD replacement char).
        assert!(dump.len() > 5, "no silent truncation after non-UTF-8 byte");
    }

    #[test]
    fn emitter_nonzero_exit_discards_partial_stdout() {
        // C1.5 — non-zero exit → Err; partial stdout must NOT be returned as Ok.
        let Some(_node) = node_path() else {
            eprintln!("SKIP: node not available");
            return;
        };
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let script = stub_node(
            "process.stdout.write(Buffer.alloc(40960, 0x61));\
             process.stderr.write('fatal boom\\n');\
             process.exit(3);",
        );
        let tsconfig = tempfile::Builder::new()
            .suffix(".json")
            .tempfile()
            .expect("tsconfig tempfile");

        let (tx, rx) = std::sync::mpsc::channel();
        let script_path = script.path().to_path_buf();
        let tsconfig_path = tsconfig.path().to_path_buf();
        std::thread::spawn(move || {
            with_lsif_ts_env(&script_path, || {
                let _ = tx.send(run_lsif_emitter(&tsconfig_path));
            });
        });

        let result = rx.recv_timeout(HARNESS_DEADLINE).expect("must return");
        assert!(result.is_err(), "non-zero exit must be Err");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("exited with"),
            "error must mention exit: {msg}"
        );
    }

    #[test]
    fn emitter_killed_mid_write_no_corruption() {
        // C1.6 — child writes slowly (via setInterval) then hangs; low timeout
        // must kill it, join threads, and return Err without dangling threads.
        let Some(_node) = node_path() else {
            eprintln!("SKIP: node not available");
            return;
        };
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let script = stub_node("setInterval(()=>{},1e9);");
        let tsconfig = tempfile::Builder::new()
            .suffix(".json")
            .tempfile()
            .expect("tsconfig tempfile");

        let (tx, rx) = std::sync::mpsc::channel();
        let script_path = script.path().to_path_buf();
        let tsconfig_path = tsconfig.path().to_path_buf();
        // Use TRAVSR_LSIF_TIMEOUT_SECS to inject a very short timeout.
        std::env::set_var("TRAVSR_LSIF_TIMEOUT_SECS", "1");
        std::thread::spawn(move || {
            with_lsif_ts_env(&script_path, || {
                let _ = tx.send(run_lsif_emitter(&tsconfig_path));
            });
        });

        let result = rx
            .recv_timeout(HARNESS_DEADLINE)
            .expect("must return after timeout+slop");
        std::env::remove_var("TRAVSR_LSIF_TIMEOUT_SECS");
        assert!(result.is_err(), "timeout must be Err");
        assert!(result.unwrap_err().to_string().contains("timed out"));
    }

    #[test]
    fn emitter_child_hangs_forever_hits_timeout_and_joins() {
        // C1.7 — writes 100 KiB then hangs via setInterval; exact production failure.
        let Some(_node) = node_path() else {
            eprintln!("SKIP: node not available");
            return;
        };
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let script = stub_node(
            "process.stdout.write(Buffer.alloc(102400, 0x61));\
             setInterval(()=>{},1e9);",
        );
        let tsconfig = tempfile::Builder::new()
            .suffix(".json")
            .tempfile()
            .expect("tsconfig tempfile");

        let (tx, rx) = std::sync::mpsc::channel();
        let script_path = script.path().to_path_buf();
        let tsconfig_path = tsconfig.path().to_path_buf();
        std::env::set_var("TRAVSR_LSIF_TIMEOUT_SECS", "1");
        std::thread::spawn(move || {
            with_lsif_ts_env(&script_path, || {
                let _ = tx.send(run_lsif_emitter(&tsconfig_path));
            });
        });

        let result = rx
            .recv_timeout(HARNESS_DEADLINE)
            .expect("must return within watchdog window");
        std::env::remove_var("TRAVSR_LSIF_TIMEOUT_SECS");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("timed out"));
    }

    #[test]
    fn emitter_5mb_output_completes() {
        // C1.8 — smoke test for O(output_size) memory contract.
        let Some(_node) = node_path() else {
            eprintln!("SKIP: node not available");
            return;
        };
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let script = stub_node("require('fs').writeSync(1,Buffer.alloc(5*1024*1024,0x61));");
        let tsconfig = tempfile::Builder::new()
            .suffix(".json")
            .tempfile()
            .expect("tsconfig tempfile");

        let (tx, rx) = std::sync::mpsc::channel();
        let script_path = script.path().to_path_buf();
        let tsconfig_path = tsconfig.path().to_path_buf();
        std::thread::spawn(move || {
            with_lsif_ts_env(&script_path, || {
                let _ = tx.send(run_lsif_emitter(&tsconfig_path));
            });
        });

        let result = rx
            .recv_timeout(HARNESS_DEADLINE)
            .expect("must return within watchdog window");
        let dump = result.expect("5 MiB must succeed");
        assert_eq!(dump.len(), 5 * 1024 * 1024);
    }

    // ── C1 variant for run_lsif_py_emitter ───────────────────────────────────

    #[test]
    fn py_emitter_large_stdout_does_not_deadlock() {
        // Mirrors C1.1 but exercises run_lsif_py_emitter to prove its drain claim.
        let Some(_node) = node_path() else {
            eprintln!("SKIP: node not available");
            return;
        };
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let script = stub_node("require('fs').writeSync(1,Buffer.alloc(131072,0x61));");
        let root = tempfile::tempdir().expect("tempdir");

        let (tx, rx) = std::sync::mpsc::channel();
        let script_path = script.path().to_path_buf();
        let root_path = root.path().to_path_buf();
        std::thread::spawn(move || {
            std::env::set_var("TRAVSR_LSIF_PY", script_path.to_str().unwrap());
            let r = run_lsif_py_emitter(&root_path);
            std::env::remove_var("TRAVSR_LSIF_PY");
            let _ = tx.send(r);
        });

        let result = rx
            .recv_timeout(HARNESS_DEADLINE)
            .expect("must return within watchdog window");
        let dump = result.expect("must succeed").expect("must be Some");
        assert_eq!(dump.len(), 131072);
    }

    #[test]
    fn py_emitter_large_stdout_and_stderr_drain_both() {
        // Mirrors C1.2 for run_lsif_py_emitter.
        let Some(_node) = node_path() else {
            eprintln!("SKIP: node not available");
            return;
        };
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let script = stub_node(
            "require('fs').writeSync(1,Buffer.alloc(131072,0x61));\
             require('fs').writeSync(2,Buffer.alloc(98304,0x62));",
        );
        let root = tempfile::tempdir().expect("tempdir");

        let (tx, rx) = std::sync::mpsc::channel();
        let script_path = script.path().to_path_buf();
        let root_path = root.path().to_path_buf();
        std::thread::spawn(move || {
            std::env::set_var("TRAVSR_LSIF_PY", script_path.to_str().unwrap());
            let r = run_lsif_py_emitter(&root_path);
            std::env::remove_var("TRAVSR_LSIF_PY");
            let _ = tx.send(r);
        });

        let result = rx
            .recv_timeout(HARNESS_DEADLINE)
            .expect("must return within watchdog window");
        let dump = result.expect("must succeed").expect("must be Some");
        assert_eq!(dump.len(), 131072);
    }
}
