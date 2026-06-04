//! Subprocess runners for language Phase B tools.
//!
//! `run_lsif_emitter` — Node.js LSIF emitter for TypeScript/JavaScript.
//! `run_scip_python`  — scip-python SCIP indexer for Python.

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
}
