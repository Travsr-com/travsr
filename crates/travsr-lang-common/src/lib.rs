//! travsr-lang-common — shared runtime for `travsr-lang-*` sidecar binaries
//! (RFC-013 Direction A).
//!
//! Each language sidecar is: a Phase A parse function from `travsr-analysis`
//! (the same code the host used when parsing was in-process — graph output is
//! identical by construction), plus an optional Phase B wrapper that runs the
//! language's SCIP tool and ingests its index. This crate holds everything
//! shared so a sidecar's `main.rs` is just declarations.
//!
//! Sandbox contract (ADR-017): the host spawns the sidecar under the sandbox
//! policy the Phase B catalog declares for the language (Standard, or
//! RequiresElevated with an approved host allowlist). Phase A parses are always
//! spawned Standard — grammar parsing needs no network and no writes outside
//! the scratch dir.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use travsr_analysis::ParseOutput;
use travsr_plugin_protocol::{
    FfiMarker as WireFfi, FfiMarkerKind as WireKind, InvokeRequest, InvokeResponse, ParseRequest,
    ParseResponse, Plugin,
};
use travsr_plugin_sdk::{run_plugin, Language};

/// Phase A parse function signature — matches `travsr_analysis::<lang>::parse`.
pub type ParseFn = fn(&str, &Path, &str) -> anyhow::Result<ParseOutput>;

/// Optional Phase B: run the language's semantic tool and return raw SCIP bytes.
/// `Ok(None)` = tool not installed (graceful skip, Phase A still works).
pub type ScipRunFn = fn(root: &Path, scratch: &Path) -> anyhow::Result<Option<Vec<u8>>>;

/// A complete language sidecar declared as data.
pub struct LangSidecar {
    pub language: Language,
    pub extensions: &'static [&'static str],
    pub parse: ParseFn,
    /// `None` → Phase B unsupported by this binary (handshake says so; the
    /// host's catalog gating never routes Phase B here).
    pub phase_b: Option<ScipRunFn>,
}

impl Plugin for LangSidecar {
    fn language(&self) -> Language {
        self.language
    }

    fn extensions(&self) -> &[&str] {
        self.extensions
    }

    fn supports_phase_b(&self) -> bool {
        self.phase_b.is_some()
    }

    fn parse(&self, req: &ParseRequest) -> ParseResponse {
        match (self.parse)(&req.corpus, &req.path, &req.vname_path) {
            Ok(out) => output_to_response(out),
            Err(e) => {
                tracing::warn!(
                    "{} parse {}: {e}",
                    self.language.as_str(),
                    req.path.display()
                );
                ParseResponse::default()
            }
        }
    }

    fn invoke_phase_b(&self, req: &InvokeRequest) -> InvokeResponse {
        let Some(run) = self.phase_b else {
            return InvokeResponse::default();
        };
        match run(&req.root, &req.scratch) {
            Ok(Some(bytes)) => match travsr_indexer::ingest_scip(&bytes, &req.corpus) {
                Ok(out) => InvokeResponse {
                    nodes: out.nodes,
                    edges: out.edges,
                    ..Default::default()
                },
                Err(e) => {
                    tracing::warn!("{} scip ingest: {e}", self.language.as_str());
                    InvokeResponse::default()
                }
            },
            Ok(None) => {
                tracing::info!(
                    "{}: semantic tool not available — Phase B skipped",
                    self.language.as_str()
                );
                InvokeResponse::default()
            }
            Err(e) => {
                tracing::warn!("{} phase B tool failed: {e}", self.language.as_str());
                InvokeResponse::default()
            }
        }
    }
}

/// Entry point for a sidecar `main()`: tracing to stderr, then the wire loop.
pub fn run(sidecar: LangSidecar) {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("error")),
        )
        .init();
    run_plugin(sidecar);
}

/// ParseOutput (analysis types) → wire ParseResponse. Mirrors the host-side
/// `plugins::convert::parse_output_to_response` — keep the two in sync.
pub fn output_to_response(out: ParseOutput) -> ParseResponse {
    ParseResponse {
        nodes: out.nodes,
        edges: out.edges,
        ffi_markers: out
            .ffi_markers
            .into_iter()
            .filter_map(|m| {
                Some(WireFfi {
                    source_node_id: m.node_id.0,
                    kind: map_ffi_kind(&m.kind)?,
                    local_name: m.local_name.clone(),
                    bound_name: m.bound_name.clone(),
                    arity: m.arity,
                    module: m.module.clone(),
                    corpus: m.corpus.clone(),
                })
            })
            .collect(),
    }
}

fn map_ffi_kind(k: &travsr_analysis::ffi::FfiMarkerKind) -> Option<WireKind> {
    use travsr_analysis::ffi::FfiMarkerKind as Rich;
    Some(match k {
        Rich::NapiExport => WireKind::NapiExport,
        Rich::NapiCall => WireKind::NapiCall,
        Rich::PyO3Export => WireKind::PyO3Export,
        Rich::PyO3Call => WireKind::PyO3Call,
        Rich::CgoExport => WireKind::CgoExport,
        Rich::GoCallC => WireKind::GoCallC,
        Rich::JniExport => WireKind::JniExport,
        Rich::JniCall => WireKind::JniCall,
    })
}

// ── Phase B tool-running helpers ──────────────────────────────────────────────

const SCIP_TIMEOUT: Duration = Duration::from_secs(600);

/// Locate a tool in `~/.travsr/bin` first (where `travsr lang install` puts
/// binaries), then on PATH.
pub fn find_tool(name: &str) -> Option<PathBuf> {
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

/// Resolve the SCIP output path inside the sandbox-authorized scratch dir
/// (falling back to the system temp dir for older daemons that send an empty
/// scratch).
pub fn scip_output_path(scratch: &Path, lang: &str) -> PathBuf {
    let dir = if scratch.as_os_str().is_empty() {
        std::env::temp_dir()
    } else {
        scratch.to_path_buf()
    };
    dir.join(format!("travsr-scip-{lang}-{}.scip", std::process::id()))
}

/// Run `tool` with `args` in `root`, wait (10-min poll timeout), then read and
/// delete the SCIP output file. Non-zero exit returns Err with captured stderr.
pub fn run_scip_tool(
    tool: &Path,
    args: &[&std::ffi::OsStr],
    root: &Path,
    output: &Path,
) -> anyhow::Result<Vec<u8>> {
    let tool_name = tool
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| tool.display().to_string());

    let mut child = std::process::Command::new(tool)
        .args(args)
        .current_dir(root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn {tool_name} for {}", root.display()))?;

    let deadline = Instant::now() + SCIP_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().context("polling child")? {
            if !status.success() {
                let mut stderr = String::new();
                if let Some(ref mut err) = child.stderr {
                    use std::io::Read as _;
                    let _ = err.read_to_string(&mut stderr);
                }
                anyhow::bail!("{tool_name} exited with {status}: {}", stderr.trim());
            }
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("{tool_name} timed out after {}s", SCIP_TIMEOUT.as_secs());
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    let bytes = std::fs::read(output)
        .with_context(|| format!("reading SCIP output {}", output.display()))?;
    let _ = std::fs::remove_file(output);
    Ok(bytes)
}
