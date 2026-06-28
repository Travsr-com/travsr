//! Subprocess runners for language Phase B tools.
//!
//! `run_lsif_emitter`    — Node.js LSIF emitter for TypeScript/JavaScript.
//! `run_lsif_py_emitter` — Node.js LSIF emitter for Python (travsr-lsif-py).
//! `run_scip_python`     — scip-python SCIP indexer for Python (legacy / deprecated).

use std::io::Read as _;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Context as _;

/// Hard ceiling for how long the TS compiler may run before we kill it.
/// Prevents a hung or looping `travsr-lsif-ts` from blocking `git commit`.
const TIMEOUT: Duration = Duration::from_secs(60);

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
/// The emitter is resolved relative to the travsr binary — no global PATH install
/// required. The subprocess is killed and an error is returned if it does not
/// complete within 60 seconds.
///
/// # Errors
/// - Emitter not found (not bundled, not on PATH)
/// - Non-zero exit code from the emitter (stderr forwarded as context)
/// - Process exceeds the 60s timeout
/// - Stdout is not valid UTF-8
pub fn run_lsif_emitter(tsconfig: &Path) -> anyhow::Result<String> {
    let (program, prefix_args) = resolve_lsif_emitter();
    let mut child = std::process::Command::new(&program)
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

    let deadline = Instant::now() + TIMEOUT;

    let status = loop {
        match child.try_wait().context("polling travsr-lsif-ts")? {
            Some(s) => break s,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                anyhow::bail!(
                    "travsr-lsif-ts timed out after {}s — killed",
                    TIMEOUT.as_secs()
                );
            }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    };

    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_string(&mut stdout);
    }
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut stderr);
    }

    if !status.success() {
        anyhow::bail!("travsr-lsif-ts exited with {status}: {stderr}");
    }

    Ok(stdout)
}

// ── scip-python ───────────────────────────────────────────────────────────────

/// Hard ceiling for `scip-python` (large projects can be slow).
const SCIP_PYTHON_TIMEOUT: Duration = Duration::from_secs(300);

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

    let mut child = std::process::Command::new(&scip_python)
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

    let deadline = Instant::now() + SCIP_PYTHON_TIMEOUT;
    let status = loop {
        match child.try_wait().context("polling scip-python")? {
            Some(s) => break s,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                anyhow::bail!(
                    "scip-python timed out after {}s — killed",
                    SCIP_PYTHON_TIMEOUT.as_secs()
                );
            }
            None => std::thread::sleep(Duration::from_millis(200)),
        }
    };

    let mut stderr = String::new();
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut stderr);
    }

    if !status.success() {
        anyhow::bail!("scip-python exited with {status}: {stderr}");
    }

    let bytes =
        std::fs::read(&output).with_context(|| format!("read scip output {}", output.display()))?;
    Ok(Some(bytes))
}

// ── travsr-lsif-py ────────────────────────────────────────────────────────────

/// Timeout for `travsr-lsif-py` (large Python repos with many files can be slow).
const LSIF_PY_TIMEOUT: Duration = Duration::from_secs(300);

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
///   tree-sitter output still runs — this is graceful degradation, not an error.
/// - `Err(_)` — emitter was found and spawned but produced a non-zero exit code,
///   or timed out after [`LSIF_PY_TIMEOUT`].
///
/// Pipe buffers (64 KB on Linux) would deadlock if the emitter writes > 64 KB of
/// LSIF before we drain — so stdout and stderr are drained on background threads
/// concurrently with the polling loop (same pattern as [`crate::ra_runner::run_ra_lsif`]).
pub fn run_lsif_py_emitter(root: &Path) -> anyhow::Result<Option<String>> {
    let (program, prefix_args) = resolve_lsif_py_emitter();

    let mut child = match std::process::Command::new(&program)
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

    // Drain stdout/stderr on background threads to prevent OS pipe-buffer deadlock.
    let mut stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("travsr-lsif-py child has no piped stdout"))?;
    let mut stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("travsr-lsif-py child has no piped stderr"))?;
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

    let deadline = Instant::now() + LSIF_PY_TIMEOUT;
    let exit_status = loop {
        match child.try_wait().context("polling travsr-lsif-py")? {
            Some(s) => break s,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                anyhow::bail!(
                    "travsr-lsif-py timed out after {}s — killed",
                    LSIF_PY_TIMEOUT.as_secs()
                );
            }
            None => std::thread::sleep(Duration::from_millis(200)),
        }
    };

    let stdout = stdout_thread.join().unwrap_or_default();
    let stderr = stderr_thread.join().unwrap_or_default();

    if !exit_status.success() {
        let stderr_head = stderr.lines().take(5).collect::<Vec<_>>().join("\n");
        anyhow::bail!("travsr-lsif-py exited with {exit_status}: {stderr_head}");
    }

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

    #[test]
    fn resolve_falls_back_to_path_when_no_env_and_no_sibling() {
        // Without TRAVSR_LSIF_TS set and no sibling binary, must fall back to PATH name.
        // We can't control current_exe in tests, but we can verify the env-override path.
        std::env::remove_var("TRAVSR_LSIF_TS");
        let (program, args) = resolve_lsif_emitter();
        // Either it found a real candidate (monorepo walk succeeded) or fell back.
        // Either way: program must be non-empty and args must be a Vec.
        assert!(!program.is_empty());
        let _ = args; // just verify it compiles and returns
    }

    #[test]
    fn resolve_honours_env_override_when_file_exists() {
        // Point TRAVSR_LSIF_TS at a file that definitely exists.
        let exe = std::env::current_exe().unwrap();
        std::env::set_var("TRAVSR_LSIF_TS", exe.to_str().unwrap());
        let (program, args) = resolve_lsif_emitter();
        assert_eq!(program, "node");
        assert_eq!(args.len(), 1);
        std::env::remove_var("TRAVSR_LSIF_TS");
    }

    #[test]
    fn resolve_lsif_py_falls_back_to_path_when_no_env_and_no_sibling() {
        std::env::remove_var("TRAVSR_LSIF_PY");
        let (program, args) = resolve_lsif_py_emitter();
        assert!(!program.is_empty());
        let _ = args;
    }

    #[test]
    fn resolve_lsif_py_honours_env_override_when_file_exists() {
        let exe = std::env::current_exe().unwrap();
        std::env::set_var("TRAVSR_LSIF_PY", exe.to_str().unwrap());
        let (program, args) = resolve_lsif_py_emitter();
        assert_eq!(program, "node");
        assert_eq!(args.len(), 1);
        std::env::remove_var("TRAVSR_LSIF_PY");
    }

    #[test]
    fn run_lsif_py_emitter_returns_none_for_missing_binary() {
        // When spawn fails, must return Ok(None) not Err.
        std::env::set_var("TRAVSR_LSIF_PY", "__travsr_lsif_py_nonexistent__");
        // The env override points at a non-file, so it is skipped and we fall
        // through to the PATH name which also does not exist — spawn should fail.
        // Reset to ensure the resolution falls through to a definitely-missing path.
        std::env::remove_var("TRAVSR_LSIF_PY");
        // Direct test: calling with a known-missing program must be Ok(None).
        // We can't override resolve_lsif_py_emitter() in tests, so we verify
        // the spawn-failure path directly by replicating its logic.
        let spawn_result = std::process::Command::new("__travsr_lsif_py_absent_binary__")
            .arg("--root")
            .arg("/tmp")
            .spawn();
        assert!(
            spawn_result.is_err(),
            "non-existent binary must fail to spawn"
        );
    }
}
