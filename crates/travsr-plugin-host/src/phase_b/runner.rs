//! Generic Phase B runner — executes any catalog entry against a repo root.
//! LSIF output is ingested via travsr_indexer::ingest_lsif.
//! SCIP output is ingested via travsr_indexer::ingest_scip.

use crate::phase_b::catalog::{OutputFormat, PhaseBEntry};
use anyhow::Context as _;
use std::path::Path;
use std::time::Duration;
use tracing::warn;
use travsr_plugin_protocol::InvokeResponse;

const PHASE_B_TIMEOUT: Duration = Duration::from_secs(300);

/// Run a Phase B tool for a repo root and return the graph output.
/// Blocks until the tool exits or the timeout is reached.
pub fn run_phase_b(entry: &PhaseBEntry, root: &Path) -> anyhow::Result<InvokeResponse> {
    // Create a scratch dir for SCIP output files. Kept alive until after ingest.
    let scratch = tempfile::TempDir::new().context("creating SCIP output scratch dir")?;
    let scip_output = scratch.path().join("index.scip");
    let scip_out_str = scip_output.to_string_lossy().into_owned();

    // Substitute path placeholders in args
    let root_str = root.to_string_lossy();
    let tsconfig = root.join("tsconfig.json");
    let tsconfig_str = tsconfig.to_string_lossy();

    let args: Vec<String> = entry
        .args
        .iter()
        .map(|&a| {
            a.replace("{root}", &root_str)
                .replace("{tsconfig}", &tsconfig_str)
                .replace("{output}", &scip_out_str)
        })
        .collect();

    // Check tool is available
    which_tool(entry.command).with_context(|| {
        format!(
            "`{}` not found on PATH.\nInstall: {}",
            entry.command, entry.install_hint
        )
    })?;

    // Run the tool
    let mut cmd = std::process::Command::new(entry.command);
    cmd.args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn `{}`", entry.command))?;

    // Drain stdout and stderr on background threads so the child is never
    // blocked on a full pipe buffer (classic deadlock for multi-MB LSIF output).
    let stdout_handle = {
        let pipe = child.stdout.take().expect("piped stdout");
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = String::new();
            let _ = std::io::BufReader::new(pipe).read_to_string(&mut buf);
            buf
        })
    };
    let stderr_handle = {
        let pipe = child.stderr.take().expect("piped stderr");
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = String::new();
            let _ = std::io::BufReader::new(pipe).read_to_string(&mut buf);
            buf
        })
    };

    // Enforce wall-clock timeout. Pipes are drained above so child can make progress.
    let deadline = std::time::Instant::now() + PHASE_B_TIMEOUT;
    let status = loop {
        match child.try_wait().context("polling Phase B tool")? {
            Some(s) => break s,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                anyhow::bail!(
                    "`{}` timed out after {}s",
                    entry.command,
                    PHASE_B_TIMEOUT.as_secs()
                );
            }
            None => std::thread::sleep(Duration::from_millis(200)),
        }
    };

    let stdout = stdout_handle.join().unwrap_or_default();
    let stderr_out = stderr_handle.join().unwrap_or_default();

    if !status.success() {
        anyhow::bail!("`{}` exited with {status}: {stderr_out}", entry.command);
    }

    // Ingest output
    match entry.output_format {
        OutputFormat::Lsif => {
            let out = travsr_indexer::ingest_lsif(&stdout).context("LSIF ingest failed")?;
            Ok(InvokeResponse {
                nodes: out.nodes,
                edges: out.edges,
            })
        }
        OutputFormat::Scip => {
            if scip_output.exists() {
                let out =
                    travsr_indexer::ingest_scip(&scip_output, "").context("SCIP ingest failed")?;
                // scratch is still alive here — drop happens after this block
                Ok(InvokeResponse {
                    nodes: out.nodes,
                    edges: out.edges,
                })
            } else {
                warn!(
                    lang = entry.language,
                    "SCIP tool `{}` produced no output at {} — check tool installation",
                    entry.command,
                    scip_output.display()
                );
                Ok(InvokeResponse::default())
            }
        }
    }
}

/// Check if a binary exists on PATH.
fn which_tool(name: &str) -> anyhow::Result<std::path::PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
        .ok_or_else(|| anyhow::anyhow!("`{name}` not found on PATH"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_tool_finds_existing_binary() {
        // sh is always on PATH
        assert!(which_tool("sh").is_ok());
    }

    #[test]
    fn which_tool_returns_err_for_missing() {
        assert!(which_tool("__travsr_nonexistent_tool__").is_err());
    }
}
