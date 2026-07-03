//! travsr-lang-kotlin — Phase B sidecar for Kotlin.
//!
//! Phase A (tree-sitter structural indexing) is in-process in the host — see
//! `travsr-plugin-host::plugins::kotlin`. This binary is the Phase B provider
//! from the catalog (`phase_b::catalog`, `provider_binary`): spawned sandboxed
//! by the host via `Sidecar::spawn`, it wraps `scip-java` (which indexes
//! Kotlin via its Gradle/Maven integration), converts the SCIP index into
//! graph nodes/edges, and returns them over the wire protocol.
//!
//! Requires:
//! - `scip-java` in `~/.travsr/bin` or on PATH (`travsr lang install kotlin`)
//! - Elevated sandbox approval (Maven/Gradle need network) — see lang.toml

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use travsr_plugin_sdk::{
    run_plugin, InvokeRequest, InvokeResponse, Language, ParseRequest, ParseResponse, Plugin,
};

/// Informational — Phase A routing happens in the host, not here.
const EXTENSIONS: &[&str] = &["kt", "kts"];

const SCIP_TOOL: &str = "scip-java";
const SCIP_TIMEOUT: Duration = Duration::from_secs(600);

struct KotlinPhaseB;

impl Plugin for KotlinPhaseB {
    fn language(&self) -> Language {
        Language::Kotlin
    }

    fn extensions(&self) -> &[&str] {
        EXTENSIONS
    }

    fn supports_phase_b(&self) -> bool {
        true
    }

    /// Phase A is in-process in the host; this provider is Phase B only.
    fn parse(&self, _req: &ParseRequest) -> ParseResponse {
        ParseResponse::default()
    }

    fn invoke_phase_b(&self, req: &InvokeRequest) -> InvokeResponse {
        match run_scip_java(&req.root) {
            Ok(Some(bytes)) => match travsr_indexer::ingest_scip(&bytes, &req.corpus) {
                Ok(out) => InvokeResponse {
                    nodes: out.nodes,
                    edges: out.edges,
                },
                Err(e) => {
                    tracing::warn!("kotlin scip ingest: {e}");
                    InvokeResponse::default()
                }
            },
            Ok(None) => {
                tracing::info!("scip-java not available — Kotlin Phase B skipped");
                InvokeResponse::default()
            }
            Err(e) => {
                tracing::warn!("scip-java failed: {e}");
                InvokeResponse::default()
            }
        }
    }
}

/// Locate a tool in `~/.travsr/bin` first (where `travsr lang install` puts
/// binaries), then on PATH.
fn find_tool(name: &str) -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("HOME") {
        let installed = Path::new(&home).join(".travsr").join("bin").join(name);
        if installed.is_file() {
            return Some(installed);
        }
    }
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

/// Run `scip-java index` on `root` and return the raw SCIP protobuf bytes.
/// `Ok(None)` = tool not installed (graceful skip).
fn run_scip_java(root: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    let Some(tool) = find_tool(SCIP_TOOL) else {
        return Ok(None);
    };

    let scratch = tempfile::Builder::new()
        .prefix("travsr-scip-kotlin-")
        .tempdir()
        .context("create scip-java scratch dir")?;
    let output = scratch.path().join("index.scip");

    let mut child = std::process::Command::new(&tool)
        .arg("index")
        .arg("--output")
        .arg(&output)
        .current_dir(root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn {SCIP_TOOL} for {}", root.display()))?;

    wait_with_timeout(&mut child, SCIP_TIMEOUT, SCIP_TOOL)?;

    let bytes = std::fs::read(&output)
        .with_context(|| format!("reading SCIP output {}", output.display()))?;
    Ok(Some(bytes))
}

/// Poll `child` until exit or `timeout`; kill on timeout. Returns Err on
/// non-zero exit with captured stderr for diagnosis.
fn wait_with_timeout(
    child: &mut std::process::Child,
    timeout: Duration,
    tool: &str,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().context("polling child")? {
            if !status.success() {
                let mut stderr = String::new();
                if let Some(ref mut err) = child.stderr {
                    use std::io::Read as _;
                    let _ = err.read_to_string(&mut stderr);
                }
                anyhow::bail!("{tool} exited with {status}: {}", stderr.trim());
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("{tool} timed out after {}s", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("error")),
        )
        .init();
    run_plugin(KotlinPhaseB);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without scip-java installed, Phase B must degrade to an empty response
    /// — never panic or error the wire.
    #[test]
    fn phase_b_degrades_gracefully_without_tool() {
        let tmp = tempfile::tempdir().unwrap();
        let req = InvokeRequest {
            root: tmp.path().to_path_buf(),
            corpus: "github.com/travsr/test".into(),
        };
        let resp = KotlinPhaseB.invoke_phase_b(&req);
        assert!(resp.nodes.is_empty());
        assert!(resp.edges.is_empty());
    }

    /// Phase A must be a no-op — it lives in-process in the host.
    #[test]
    fn parse_returns_empty() {
        let req = ParseRequest {
            path: PathBuf::from("x.kt"),
            vname_path: "x.kt".into(),
            corpus: String::new(),
            package: String::new(),
            source: None,
        };
        assert!(KotlinPhaseB.parse(&req).nodes.is_empty());
    }
}
