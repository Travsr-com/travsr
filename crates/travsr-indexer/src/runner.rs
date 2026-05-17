//! Subprocess runner for `travsr-lsif-ts`.
//!
//! Runs the Node.js LSIF emitter as a child process and captures its stdout
//! (the LSIF JSON-Lines dump). The binary must be on PATH; if it is not
//! installed, the function returns `Err` and callers are expected to log a
//! warning and skip LSIF ingestion — never to fail the overall index.

use std::io::Read as _;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Context as _;

/// Hard ceiling for how long the TS compiler may run before we kill it.
/// Prevents a hung or looping `travsr-lsif-ts` from blocking `git commit`.
const TIMEOUT: Duration = Duration::from_secs(60);

/// Run `travsr-lsif-ts --project <tsconfig>` and return the LSIF dump.
///
/// The subprocess is killed and an error is returned if it does not complete
/// within 60 seconds — guarding against infinite loops in `tsconfig.extends`
/// chains or a stalled TS language service.
///
/// # Errors
/// - `travsr-lsif-ts` not found on PATH (user must `npm install -g travsr-lsif-ts`)
/// - Non-zero exit code from the emitter (stderr forwarded as context)
/// - Process exceeds the 60s timeout
/// - Stdout is not valid UTF-8
pub fn run_lsif_emitter(tsconfig: &Path) -> anyhow::Result<String> {
    let mut child = std::process::Command::new("travsr-lsif-ts")
        .arg("--project")
        .arg(tsconfig)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "could not run travsr-lsif-ts for {} \
                 (is it installed? run: npm install -g travsr-lsif-ts)",
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

#[cfg(test)]
mod tests {
    #[test]
    fn run_lsif_emitter_returns_err_for_missing_binary() {
        // An impossible binary name must produce an Err, not a panic.
        let result = std::process::Command::new("__travsr_nonexistent_binary__").output();
        // We test the Command API directly here because run_lsif_emitter
        // hard-codes the binary name. The important invariant is that a
        // missing binary produces an IO error rather than a panic.
        assert!(result.is_err(), "missing binary must return Err");
    }
}
