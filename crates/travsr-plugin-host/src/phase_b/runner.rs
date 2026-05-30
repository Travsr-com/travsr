//! Generic Phase B runner — executes any catalog entry against a repo root.
//! LSIF output is ingested via travsr_indexer::ingest_lsif.
//! SCIP output: stub (returns empty InvokeResponse with warning) — full SCIP
//! ingestion is a separate sprint deliverable.

use std::path::Path;
use std::time::Duration;
use anyhow::Context as _;
use tracing::warn;
use travsr_plugin_protocol::InvokeResponse;
use crate::phase_b::catalog::{OutputFormat, PhaseBEntry};

const PHASE_B_TIMEOUT: Duration = Duration::from_secs(300);

/// Run a Phase B tool for a repo root and return the graph output.
/// Blocks until the tool exits or the timeout is reached.
pub fn run_phase_b(entry: &PhaseBEntry, root: &Path) -> anyhow::Result<InvokeResponse> {
    // Substitute path placeholders in args
    let root_str = root.to_string_lossy();
    let tsconfig = root.join("tsconfig.json");
    let tsconfig_str = tsconfig.to_string_lossy();

    let args: Vec<String> = entry.args.iter().map(|&a| {
        a.replace("{root}", &root_str)
         .replace("{tsconfig}", &tsconfig_str)
    }).collect();

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

    let mut child = cmd.spawn()
        .with_context(|| format!("failed to spawn `{}`", entry.command))?;

    // Enforce wall-clock timeout
    let deadline = std::time::Instant::now() + PHASE_B_TIMEOUT;
    let status = loop {
        match child.try_wait().context("polling Phase B tool")? {
            Some(s) => break s,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                anyhow::bail!("`{}` timed out after {}s", entry.command, PHASE_B_TIMEOUT.as_secs());
            }
            None => std::thread::sleep(Duration::from_millis(200)),
        }
    };

    let mut stdout = String::new();
    let mut stderr_out = String::new();
    if let Some(mut out) = child.stdout.take() {
        use std::io::Read;
        let _ = out.read_to_string(&mut stdout);
    }
    if let Some(mut err) = child.stderr.take() {
        use std::io::Read;
        let _ = err.read_to_string(&mut stderr_out);
    }

    if !status.success() {
        anyhow::bail!("`{}` exited with {status}: {stderr_out}", entry.command);
    }

    // Ingest output
    match entry.output_format {
        OutputFormat::Lsif => {
            let out = travsr_indexer::ingest_lsif(&stdout)
                .context("LSIF ingest failed")?;
            Ok(InvokeResponse { nodes: out.nodes, edges: out.edges })
        }
        OutputFormat::Scip => {
            // TODO(travsr): #254 — implement SCIP protobuf ingestion
            // scip-go, scip-java, scip-php etc. emit binary protobuf to a file,
            // not JSON to stdout. Full SCIP ingestion requires:
            //   1. Write output to a temp file (--output flag)
            //   2. Parse the SCIP protobuf
            //   3. Convert to travsr Node/Edge
            // Tracked separately. For now return empty output with a warning.
            warn!(
                lang = entry.language,
                "SCIP ingestion not yet implemented — Phase B output from `{}` discarded. \
                 Tracking issue #254.",
                entry.command
            );
            Ok(InvokeResponse::default())
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
