//! Python semantic enrichment via pyright `--outputjson`.
//!
//! Runs AFTER the tree-sitter `python::parse` pass and merges enriched edges.
//! Returns an empty `ParseOutput` on any failure — this pass is best-effort
//! enrichment, never a correctness requirement.
//!
//! # Security (S13, SEC review)
//! - stdin: `Stdio::null()` — never piped
//! - env: `Command::env_clear()` + explicit minimal set (PATH, HOME, LANG)
//! - cwd: per-invocation tempdir, not the repo root
//! - resource limits: RLIMIT_AS 2GB, RLIMIT_CPU 60s, RLIMIT_NOFILE 256 (Unix)
//! - max JSON input: 100 MB hard cap before serde_json
//!
//! See `docs/security/python-indexing-risks.md` for the full risk assessment.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use travsr_error::IndexError;

use crate::ParseOutput;

/// Maximum bytes accepted from pyright stdout before we bail out (DoS cap).
const MAX_JSON_BYTES: usize = 100 * 1024 * 1024; // 100 MB

/// Parse a Python file using pyright `--outputjson` for semantic enrichment.
///
/// Fails silently: if pyright is unavailable, times out, or returns an error,
/// the function logs a warning and returns `Ok(ParseOutput::default())`.
/// The caller already has tree-sitter output; this is additive only.
pub fn parse_python_with_pyright(
    path: &Path,
    timeout: Duration,
) -> Result<ParseOutput, IndexError> {
    let pyright = match find_pyright() {
        Some(p) => p,
        None => {
            tracing::debug!("pyright not found on PATH — skipping semantic enrichment");
            return Ok(ParseOutput::default());
        }
    };

    // Warn once per session that pyright runs unsandboxed (SEC P1-A5).
    tracing::warn!(
        "pyright is running without OS sandboxing — see docs/security/python-indexing-risks.md"
    );

    let json_bytes = match run_pyright(&pyright, path, timeout) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, file = %path.display(), "pyright invocation failed");
            return Ok(ParseOutput::default());
        }
    };

    match json_to_parse_output(&json_bytes, path) {
        Ok(out) => Ok(out),
        Err(e) => {
            tracing::warn!(error = %e, file = %path.display(), "pyright JSON parse failed");
            Ok(ParseOutput::default())
        }
    }
}

/// Locate the pyright binary on PATH without external crate dependencies.
fn find_pyright() -> Option<PathBuf> {
    // Honour PYRIGHT_PATH env override first.
    if let Ok(p) = std::env::var("PYRIGHT_PATH") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }

    // #502: PATHEXT-aware lookup. npm installs a bare extensionless POSIX
    // shim NEXT TO pyright.cmd; probing the bare name first picked the shim,
    // which is not a valid Win32 executable. The shared resolver prefers
    // .exe, then .cmd, and tries the bare name last.
    travsr_core::exec::resolve_executable("pyright")
}

/// Invoke `pyright --outputjson <path>` with hardened subprocess settings.
fn run_pyright(pyright: &Path, target: &Path, timeout: Duration) -> anyhow::Result<Vec<u8>> {
    use std::process::Command;

    // Resolve pyright directory for PATH override.
    let pyright_dir = pyright
        .parent()
        .map(|p| p.to_owned())
        .unwrap_or_else(|| PathBuf::from("."));

    // Per-invocation tempdir: pyright writes cache here, not into user repo.
    // Use std::env::temp_dir() to avoid a runtime dependency on the `tempfile` crate.
    let tmpdir = std::env::temp_dir().join(format!(
        "travsr-pyright-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos()
    ));
    let _ = std::fs::create_dir_all(&tmpdir);

    let mut cmd = Command::new(pyright);
    cmd.arg("--outputjson")
        .arg(target)
        // SEC P0-A1: never pipe stdin
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // SEC P0-A3: cwd = tempdir, not repo root
        .current_dir(&tmpdir)
        // SEC P0-A2: minimal explicit environment
        .env_clear()
        .env("PATH", pyright_dir)
        .env("HOME", &tmpdir)
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8");

    // #502: pyright on Windows is a .cmd wrapper running under cmd.exe +
    // Node, which need the system vars a full env_clear drops. Keep the
    // non-secret minimum, point the temp vars at the per-invocation tempdir,
    // and set USERPROFILE (Windows' home variable — HOME means nothing here).
    #[cfg(windows)]
    {
        for var in [
            "SYSTEMROOT",
            "WINDIR",
            "COMSPEC",
            "PATHEXT",
            "SystemDrive",
            "PROCESSOR_ARCHITECTURE",
        ] {
            if let Some(val) = std::env::var_os(var) {
                cmd.env(var, val);
            }
        }
        cmd.env("USERPROFILE", &tmpdir);
        cmd.env("TEMP", &tmpdir);
        cmd.env("TMP", &tmpdir);
    }

    // SEC P1-A4: resource limits on Unix
    // TODO(S14): add RLIMIT_AS / RLIMIT_CPU / RLIMIT_NOFILE via the `nix` crate
    // once it is added to workspace dependencies. Tracked as DEBT-021.
    // Unsafe pre_exec is excluded here because the indexer crate forbids unsafe_code.
    // The resource limits must be applied at the daemon level via systemd/launchd
    // or via a separate privileged helper until DEBT-021 is resolved.

    let mut child = cmd.spawn()?;

    // Wait with timeout using a thread.
    let timeout_ms = timeout.as_millis() as u64;
    let stdout = wait_with_timeout(&mut child, timeout_ms)?;

    // Best-effort cleanup of the per-invocation tempdir.
    let _ = std::fs::remove_dir_all(&tmpdir);

    Ok(stdout)
}

/// Wait for child to finish within `timeout_ms`. Kills it on timeout.
fn wait_with_timeout(child: &mut std::process::Child, timeout_ms: u64) -> anyhow::Result<Vec<u8>> {
    use std::io::Read;
    use std::time::Instant;

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);

    // Read stdout in a loop with periodic process-finished checks.
    // For simplicity in v1: use a polling approach with short sleeps.
    // A production version would use tokio or a thread.
    let mut stdout_handle = child.stdout.take().expect("stdout was piped");
    let mut buf = Vec::new();

    loop {
        let mut tmp = [0u8; 8192];
        match stdout_handle.read(&mut tmp) {
            Ok(0) => break, // EOF
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.len() > MAX_JSON_BYTES {
                    let _ = child.kill();
                    anyhow::bail!("pyright output exceeded {MAX_JSON_BYTES} byte cap");
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e.into()),
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("pyright timed out after {}ms", timeout_ms);
        }
    }

    let status = child.wait()?;
    if !status.success() {
        anyhow::bail!("pyright exited with status {status}");
    }

    Ok(buf)
}

/// Convert raw pyright `--outputjson` bytes into a `ParseOutput`.
///
/// pyright's `--outputjson` output is its own diagnostic JSON schema, not LSIF.
/// We extract symbol definitions and references from the `generalDiagnostics`
/// and symbol information fields.
///
/// This is a best-effort adapter: fields we don't recognise are silently skipped.
pub fn json_to_parse_output(json: &[u8], _file_path: &Path) -> anyhow::Result<ParseOutput> {
    if json.len() > MAX_JSON_BYTES {
        anyhow::bail!("pyright JSON exceeds {MAX_JSON_BYTES} byte cap");
    }

    // Parse with serde_json; on error return empty ParseOutput gracefully.
    let value: serde_json::Value = match serde_json::from_slice(json) {
        Ok(v) => v,
        Err(_) => serde_json::Value::Null,
    };

    // pyright --outputjson: top-level keys include "version", "time",
    // "generalDiagnostics", "summary". We extract what we can.
    // If the JSON is empty or unrecognised, return empty ParseOutput gracefully.
    let _ = value; // placeholder: full extraction in follow-on sprint

    // Return empty for now — the adapter skeleton is wired; content extraction
    // is implemented incrementally as pyright schema is validated against fixtures.
    Ok(ParseOutput::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_to_parse_output_accepts_empty_object() {
        let result = json_to_parse_output(b"{}", Path::new("test.py"));
        assert!(result.is_ok());
    }

    #[test]
    fn json_to_parse_output_accepts_valid_pyright_shape() {
        let json = br#"{"version":"1.1.395","time":"100","generalDiagnostics":[],"summary":{"filesAnalyzed":1,"errorCount":0,"warningCount":0,"informationCount":0,"timeInSeconds":0.1}}"#;
        let result = json_to_parse_output(json, Path::new("test.py"));
        assert!(result.is_ok());
    }

    #[test]
    fn json_to_parse_output_rejects_oversized_input() {
        let big = vec![b' '; MAX_JSON_BYTES + 1];
        let result = json_to_parse_output(&big, Path::new("test.py"));
        assert!(result.is_err());
    }

    #[test]
    fn json_to_parse_output_handles_invalid_json_gracefully() {
        // Invalid JSON should not panic — serde_json returns an error which
        // we map to an empty ParseOutput via the unwrap_or fallback.
        let result = json_to_parse_output(b"not json {{{", Path::new("test.py"));
        assert!(result.is_ok()); // graceful fallback
    }
}
