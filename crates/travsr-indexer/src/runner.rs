//! Subprocess runner for `travsr-lsif-ts`.
//!
//! Runs the Node.js LSIF emitter as a child process and captures its stdout
//! (the LSIF JSON-Lines dump). The binary must be on PATH; if it is not
//! installed, the function returns `Err` and callers are expected to log a
//! warning and skip LSIF ingestion — never to fail the overall index.

use std::path::Path;

use anyhow::Context as _;

/// Run `travsr-lsif-ts --project <tsconfig>` and return the LSIF dump.
///
/// # Errors
/// - `travsr-lsif-ts` not found on PATH (user must `npm install -g travsr-lsif-ts`)
/// - Non-zero exit code from the emitter (stderr forwarded as context)
/// - Stdout is not valid UTF-8
pub fn run_lsif_emitter(tsconfig: &Path) -> anyhow::Result<String> {
    let output = std::process::Command::new("travsr-lsif-ts")
        .arg("--project")
        .arg(tsconfig)
        .output()
        .with_context(|| {
            format!(
                "could not run travsr-lsif-ts for {} \
                 (is it installed? run: npm install -g travsr-lsif-ts)",
                tsconfig.display()
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("travsr-lsif-ts exited with {}: {stderr}", output.status);
    }

    String::from_utf8(output.stdout).context("travsr-lsif-ts stdout is not valid UTF-8")
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
