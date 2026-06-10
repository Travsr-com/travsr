use std::io::Read as _;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use travsr_core::{Edge, EdgeKind, Node, NodeId, VName};

const TIMEOUT_SECS: u64 = 300;

/// Platform-correct binary name for the emitter.
fn emitter_name() -> String {
    format!("travsr-dart-index-emitter{}", std::env::consts::EXE_SUFFIX)
}

fn emitter_path() -> Option<PathBuf> {
    // 1. Explicit override — highest priority, works on every platform.
    if let Ok(p) = std::env::var("TRAVSR_DART_EMITTER") {
        let path = PathBuf::from(&p);
        if path.exists() {
            return Some(path);
        }
    }

    let name = emitter_name();

    let exe = std::env::current_exe().ok();

    // 2. Dev monorepo: target/{debug|release}/travsr (or .exe on Windows)
    //    → ../../.. = workspace root → packages/dart-scip-emitter/bin/emit-native
    if let Some(ref exe) = exe {
        let dev = exe
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .map(|root| {
                root.join("packages")
                    .join("dart-scip-emitter")
                    .join("bin")
                    .join("emit-native")
            });
        if let Some(ref p) = dev {
            if p.exists() {
                return Some(p.clone());
            }
        }
    }

    // 3. Installed: sibling of the daemon binary in the same bin directory.
    //    Works when both travsr and travsr-dart-index-emitter are in ~/.travsr/bin/.
    if let Some(ref exe) = exe {
        let sibling = exe.parent().map(|bin| bin.join(&name));
        if let Some(ref p) = sibling {
            if p.exists() {
                return Some(p.clone());
            }
        }
    }

    // 4. Well-known install location: ~/.travsr/bin/travsr-dart-index-emitter.
    //    Covers the case where the daemon binary lives in a different directory
    //    (e.g. /usr/local/bin via npm link) but `travsr lang install dart` has run.
    let home = std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok()); // Windows fallback
    if let Some(home) = home {
        let well_known = PathBuf::from(home).join(".travsr").join("bin").join(&name);
        if well_known.exists() {
            return Some(well_known);
        }
    }

    None
}

/// Call the Dart AOT emitter binary directly and return Phase B nodes + edges.
///
/// Bypasses the travsr-lang-dart sidecar entirely. The sidecar spawns the same
/// emitter but crashes with SIGABRT (`kDartIsolateSnapshotData not found`) when
/// the emitter is a nested subprocess of the sandboxed sidecar. Running it as a
/// direct child of the daemon process (which has a clean env with HOME intact)
/// avoids the crash.
pub fn extract_native_phase_b(
    corpus: &str,
    root: &Path,
) -> anyhow::Result<(Vec<Node>, Vec<Edge>)> {
    let emitter = emitter_path().context(
        "travsr-dart-index-emitter not found — \
         set $TRAVSR_DART_EMITTER or run `travsr lang install dart`",
    )?;

    let scratch = tempfile::tempdir().context("failed to create scratch dir")?;
    let output_path = scratch.path().join("index.json");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(TIMEOUT_SECS);

    tracing::debug!(
        emitter = %emitter.display(),
        root = %root.display(),
        output = %output_path.display(),
        "phase_b_dart: launching emitter"
    );

    let mut child = std::process::Command::new(&emitter)
        .arg(root)
        .arg(&output_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn {}", emitter.display()))?;

    let status = loop {
        match child.try_wait().context("polling dart emitter")? {
            Some(s) => break s,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("dart emitter timed out after {TIMEOUT_SECS}s");
            }
            None => std::thread::sleep(std::time::Duration::from_millis(200)),
        }
    };

    let mut stderr_buf = String::new();
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut stderr_buf);
    }
    if !stderr_buf.is_empty() {
        tracing::debug!("phase_b_dart stderr:\n{stderr_buf}");
    }

    anyhow::ensure!(
        status.success(),
        "dart emitter exited with {status}: {stderr_buf}"
    );

    parse_emitter_output(&output_path, corpus)
}

fn parse_emitter_output(json_path: &Path, corpus: &str) -> anyhow::Result<(Vec<Node>, Vec<Edge>)> {
    let bytes = std::fs::read(json_path)
        .with_context(|| format!("reading emitter output {}", json_path.display()))?;

    if bytes.is_empty() {
        return Ok((vec![], vec![]));
    }

    let root_val: serde_json::Value =
        serde_json::from_slice(&bytes).context("parsing emitter JSON")?;

    let docs = root_val["documents"]
        .as_array()
        .context("missing 'documents' in emitter output")?;

    // Pass 1: build symbol → NodeId map from all definitions.
    let mut def_ids: std::collections::HashMap<String, NodeId> =
        std::collections::HashMap::new();
    let mut nodes: Vec<Node> = Vec::new();

    for doc in docs {
        let path = doc["path"].as_str().unwrap_or("");
        let Some(defs) = doc["definitions"].as_array() else {
            continue;
        };
        for d in defs {
            let sym = d["symbol"].as_str().unwrap_or("");
            let kind = d["kind"].as_str().unwrap_or("definition");
            let line = d["line"].as_u64().unwrap_or(0) as u32;
            if sym.is_empty() {
                continue;
            }
            let vname = VName::new(corpus, "", path, "dart", sym);
            let node_id = vname.id();
            def_ids.insert(sym.to_string(), node_id);
            nodes.push(Node::new(vname, kind).with_line(line));
        }
    }

    // Pass 2: resolve references → RefCall edges.
    let mut edges: Vec<Edge> = Vec::new();

    for doc in docs {
        let path = doc["path"].as_str().unwrap_or("");
        let Some(refs) = doc["references"].as_array() else {
            continue;
        };
        let file_id = VName::new(corpus, "", path, "dart", "file").id();
        for r in refs {
            let sym = r["symbol"].as_str().unwrap_or("");
            if sym.is_empty() {
                continue;
            }
            if let Some(&dst_id) = def_ids.get(sym) {
                edges.push(Edge::new(file_id, dst_id, EdgeKind::RefCall));
            }
        }
    }

    tracing::info!(
        nodes = nodes.len(),
        edges = edges.len(),
        "phase_b_dart: ingestion complete"
    );

    Ok((nodes, edges))
}
