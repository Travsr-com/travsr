//! Phase B for Dart: call-site edges via the native AOT emitter.
//!
//! Calls the `travsr-dart-index-emitter` binary directly. Bypasses the
//! travsr-lang-dart sidecar to avoid a Dart AOT SIGABRT (`kDartIsolateSnapshotData
//! not found`) that occurs when the emitter is a nested subprocess of the sandboxed
//! sidecar. Running it as a direct child of the daemon (which has a clean env with
//! HOME intact) avoids the crash.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use travsr_core::{Edge, EdgeKind, Node, NodeId, ScipRef, VName};

const TIMEOUT_SECS: u64 = 300;

/// Platform-correct binary name for the emitter.
fn emitter_name() -> String {
    format!("travsr-dart-index-emitter{}", std::env::consts::EXE_SUFFIX)
}

/// Detect the Dart SDK root so the AOT emitter can be told where it lives.
///
/// #299 E1: the emitter is an AOT binary, so the analyzer's default SDK
/// auto-detection resolves relative to the emitter's own path
/// (`~/.travsr/lib/...`) and fails to read `lib/_internal/…`. We find `dart` on
/// `PATH`, canonicalize it (`<sdk>/bin/dart`), and take `<sdk>` — verified by the
/// presence of `lib/_internal/allowed_experiments.json` — then pass it via the
/// `DART_SDK` env the emitter reads. Returns `None` when no usable SDK is found
/// (the emitter falls back to auto-detection).
fn detect_dart_sdk() -> Option<PathBuf> {
    // Explicit override wins.
    if let Ok(p) = std::env::var("DART_SDK") {
        let path = PathBuf::from(&p);
        if !p.is_empty() && path.join("lib/_internal/allowed_experiments.json").exists() {
            return Some(path);
        }
    }
    let is_sdk_root = |sdk: &Path| sdk.join("lib/_internal/allowed_experiments.json").exists();

    let dart_name = format!("dart{}", std::env::consts::EXE_SUFFIX);
    let path_var = std::env::var_os("PATH")?;
    let mut first_dart: Option<PathBuf> = None;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(&dart_name);
        if !candidate.exists() {
            continue;
        }
        if first_dart.is_none() {
            first_dart = Some(candidate.clone());
        }
        // `<sdk>/bin/dart` → `<sdk>`. Canonicalize to follow symlinks
        // (Homebrew: /opt/homebrew/bin/dart → …/dart-sdk/<ver>/libexec/bin/dart).
        let real = std::fs::canonicalize(&candidate).unwrap_or(candidate);
        if let Some(sdk) = real.parent().and_then(|p| p.parent()) {
            if is_sdk_root(sdk) {
                return Some(sdk.to_path_buf());
            }
        }
    }
    // Version-manager shims (asdf / mise / volta) are shell scripts, not symlinks
    // into `<sdk>/bin/dart`, so the `parent().parent()` walk above lands on the
    // shim directory, not the SDK. Ask the `dart` tool itself where its SDK is.
    // (Without this the emitter falls back to a broken auto-detect → empty Dart
    // Phase B, reintroducing the #299 E1 failure.)
    if let Some(dart) = first_dart {
        if let Some(sdk) = sdk_root_via_dart(&dart) {
            if is_sdk_root(&sdk) {
                return Some(sdk);
            }
        }
    }
    tracing::warn!(
        "Dart SDK not found on PATH; Dart Phase B call-site edges will be unavailable. \
         Set DART_SDK to the SDK root (the directory containing lib/_internal) to enable it."
    );
    None
}

/// Ask the `dart` tool where its SDK lives, for the version-manager-shim case.
///
/// There is no `dart` flag that prints the SDK path (the old `--print-sdk-path`
/// was invalid and this fallback silently no-opped — U6a). Instead we run a
/// one-line program that prints `Platform.resolvedExecutable`'s SDK root
/// (`<sdk>/bin/dart` → `<sdk>`). This resolves correctly even through an asdf /
/// mise / volta shim, because the shim `exec`s the real binary so
/// `resolvedExecutable` is the true `<sdk>/bin/dart`, not the shim script.
///
/// Returns `None` on any failure (spawn error, non-zero exit, empty output) so
/// a broken shim degrades to the "SDK not found" warning in [`detect_dart_sdk`]
/// rather than handing the emitter an invalid path.
fn sdk_root_via_dart(dart: &Path) -> Option<PathBuf> {
    let scratch = tempfile::tempdir().ok()?;
    let prog = scratch.path().join("travsr_sdk_probe.dart");
    std::fs::write(
        &prog,
        "import 'dart:io';\n\
         void main() => print(File(Platform.resolvedExecutable).parent.parent.path);\n",
    )
    .ok()?;
    let out = std::process::Command::new(dart).arg(&prog).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!path.is_empty()).then(|| PathBuf::from(path))
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
pub fn extract_native_phase_b(
    corpus: &str,
    root: &Path,
) -> anyhow::Result<(Vec<Node>, Vec<Edge>, Vec<ScipRef>)> {
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

    let mut command = std::process::Command::new(&emitter);
    command
        .arg(root)
        .arg(&output_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    // #299 E1: point the AOT emitter at the real Dart SDK — without this the
    // analyzer resolves the SDK relative to the emitter binary and crashes
    // reading `lib/_internal/…`, leaving Dart Phase B empty.
    if let Some(sdk) = detect_dart_sdk() {
        tracing::debug!(sdk = %sdk.display(), "phase_b_dart: DART_SDK");
        command.env("DART_SDK", sdk);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn {}", emitter.display()))?;

    // Drain stderr on a background thread before entering the poll loop so a
    // verbose emitter can't fill the 64 KiB OS pipe buffer and block here
    // (same footgun as Issue #412 C1). Stdout is Stdio::null() — no risk there.
    // travsr-analysis is upstream of travsr-indexer so run_with_drain is not
    // available here; the pattern is intentionally inlined.
    let mut stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("dart emitter child has no piped stderr"))?;
    let err_t = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stderr_pipe.read_to_string(&mut buf);
        buf
    });

    // `try_wait` is used intentionally here: stderr is already draining above
    // so there is no pipe-buffer deadlock risk. run_with_drain (travsr-indexer)
    // cannot be called from this crate due to the dependency direction.
    #[allow(clippy::disallowed_methods)]
    let status = loop {
        match child.try_wait().context("polling dart emitter")? {
            Some(s) => break s,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = err_t.join();
                anyhow::bail!("dart emitter timed out after {TIMEOUT_SECS}s");
            }
            None => std::thread::sleep(std::time::Duration::from_millis(200)),
        }
    };

    let stderr_buf = err_t.join().unwrap_or_default();
    if !stderr_buf.is_empty() {
        tracing::debug!("phase_b_dart stderr:\n{stderr_buf}");
    }

    anyhow::ensure!(
        status.success(),
        "dart emitter exited with {status}: {stderr_buf}"
    );

    parse_emitter_output(&output_path, corpus)
}

/// Dart packages under `root` whose dependencies are not resolved.
///
/// `package:analyzer` needs `<pkg>/.dart_tool/package_config.json` (produced by
/// `dart pub get`) to resolve `package:` imports. Without it it silently
/// degrades to file-URI / SDK-only resolution: intra-package references still
/// resolve, but cross-package references are dropped. A fresh clone never
/// carries `package_config.json` (`.dart_tool/` is gitignored by Dart
/// convention), so this is the common case — and the resulting partial index
/// must never be reported as a confident zero.
///
/// We never run `dart pub get` ourselves: it mutates the user's tree and needs
/// the network (local-first). This only *detects* the degraded state so the
/// daemon can record it and `travsr status` / `find_references` can be honest.
///
/// Returns package directories (relative to `root`, `.` for the root package)
/// that have a `pubspec.yaml` but no resolved `package_config.json`, sorted and
/// capped. Empty when every Dart package is resolved (or none exist).
pub fn unresolved_dep_packages(root: &Path) -> Vec<String> {
    const MAX_REPORTED: usize = 20;
    let mut out = Vec::new();
    collect_unresolved(root, root, 0, &mut out);
    out.sort();
    out.dedup();
    out.truncate(MAX_REPORTED);
    out
}

fn collect_unresolved(root: &Path, dir: &Path, depth: usize, out: &mut Vec<String>) {
    const MAX_DEPTH: usize = 12;
    if depth > MAX_DEPTH {
        return;
    }
    if dir.join("pubspec.yaml").is_file()
        && !dir.join(".dart_tool").join("package_config.json").is_file()
    {
        let rel = dir.strip_prefix(root).unwrap_or(dir);
        // Forward slashes so the label is identical on Windows and Unix.
        let label = if rel.as_os_str().is_empty() {
            ".".to_string()
        } else {
            rel.to_string_lossy().replace('\\', "/")
        };
        out.push(label);
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        // `file_type()` does not follow symlinks, so a symlinked directory is
        // is_dir() == false here — that both skips it and avoids symlink loops.
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if !is_dir {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Hidden (.git, .dart_tool), build outputs, and vendored JS deps are
        // never Dart package roots and can be large.
        if name.starts_with('.') || name == "build" || name == "node_modules" {
            continue;
        }
        collect_unresolved(root, &entry.path(), depth + 1, out);
    }
}

fn parse_emitter_output(
    json_path: &Path,
    corpus: &str,
) -> anyhow::Result<(Vec<Node>, Vec<Edge>, Vec<ScipRef>)> {
    let bytes = std::fs::read(json_path)
        .with_context(|| format!("reading emitter output {}", json_path.display()))?;

    if bytes.is_empty() {
        return Ok((vec![], vec![], vec![]));
    }

    let root_val: serde_json::Value =
        serde_json::from_slice(&bytes).context("parsing emitter JSON")?;

    let docs = root_val["documents"]
        .as_array()
        .context("missing 'documents' in emitter output")?;

    // Pass 1: build symbol → NodeId map from all definitions.
    let mut def_ids: std::collections::HashMap<String, NodeId> = std::collections::HashMap::new();
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
            let mut node = Node::new(vname, kind).with_line(line);
            if let Some(el) = d["end_line"].as_u64() {
                node = node.with_end_line(el as u32);
            }
            nodes.push(node);
        }
    }

    // Pass 2: resolve references → RefCall edges + occurrence records.
    let mut edges: Vec<Edge> = Vec::new();
    // #299 S1/E1: emit ScipRef occurrence records (path:line) so the daemon
    // populates edge_sites and find_references works for Dart — the emitter
    // already gives us each reference's line.
    let mut refs_out: Vec<ScipRef> = Vec::new();

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
                // E5: prefer an attributed occurrence record — the daemon's
                // write_scip_attributed_batch resolves its `src` to the
                // enclosing function and populates edge_sites. Emit a
                // file-granular RefCall edge ONLY as a fallback when the emitter
                // gave no line (otherwise every call also got a spurious
                // file->callee edge alongside the attributed one).
                // Emitter lines are 1-based (definitions store them as-is).
                if let Some(line) = r["line"].as_u64() {
                    refs_out.push(ScipRef {
                        caller_path: path.to_string(),
                        caller_line: line as u32,
                        callee_id: dst_id,
                        // The Dart emitter reports call references; treat as calls
                        // so they create `ref/call` edges as before (#650).
                        is_call: true,
                    });
                } else {
                    edges.push(Edge::new(file_id, dst_id, EdgeKind::RefCall));
                }
            }
        }
    }

    tracing::info!(
        nodes = nodes.len(),
        edges = edges.len(),
        refs = refs_out.len(),
        "phase_b_dart: ingestion complete"
    );

    Ok((nodes, edges, refs_out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn parse_emitter_output_emits_scip_refs_for_resolved_references() {
        // #299 S1/E1: the Dart emitter's per-reference lines must become ScipRef
        // occurrence records (path:line) keyed to the definition node, so the
        // daemon can populate edge_sites and find_references works for Dart.
        // A reference to a symbol with no definition in this index is dropped.
        let json = r#"{
          "documents": [
            {
              "path": "animal.dart",
              "definitions": [
                {"symbol": "pkg:app/animal.dart::Animal.describe",
                 "kind": "method", "line": 8, "end_line": 10}
              ],
              "references": []
            },
            {
              "path": "main.dart",
              "definitions": [],
              "references": [
                {"symbol": "pkg:app/animal.dart::Animal.describe", "line": 16},
                {"symbol": "pkg:app/animal.dart::Animal.describe"},
                {"symbol": "pkg:ghost/x.dart::Ghost.method", "line": 99}
              ]
            }
          ]
        }"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(json.as_bytes()).unwrap();

        let (nodes, edges, refs) = parse_emitter_output(f.path(), "c").unwrap();

        // One definition node, carrying kind + line span.
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].kind, "method");
        assert_eq!(nodes[0].line, Some(8));
        assert_eq!(nodes[0].end_line, Some(10));
        let def_id = VName::new(
            "c",
            "",
            "animal.dart",
            "dart",
            "pkg:app/animal.dart::Animal.describe",
        )
        .id();
        assert_eq!(nodes[0].id, def_id);

        // E5: the line-bearing reference becomes exactly one occurrence record
        // (attributed downstream to the enclosing function) with NO redundant
        // file-granular edge; the undefined `Ghost.method` reference is dropped.
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].caller_path, "main.dart");
        assert_eq!(refs[0].caller_line, 16);
        assert_eq!(refs[0].callee_id, def_id);

        // E5: only the line-less reference falls back to a file-granular RefCall
        // edge (it cannot be attributed); the line-16 ref does NOT.
        let main_file_id = VName::new("c", "", "main.dart", "dart", "file").id();
        assert_eq!(
            edges.len(),
            1,
            "only the line-less ref emits a fallback edge"
        );
        assert_eq!(edges[0].src, main_file_id);
        assert_eq!(edges[0].dst, def_id);
        assert_eq!(edges[0].kind, EdgeKind::RefCall);
    }

    #[test]
    fn parse_emitter_output_empty_file_is_no_op() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let (nodes, edges, refs) = parse_emitter_output(f.path(), "c").unwrap();
        assert!(nodes.is_empty() && edges.is_empty() && refs.is_empty());
    }

    #[test]
    fn unresolved_dep_packages_flags_only_unresolved_packages() {
        // WS-2: a monorepo mirroring dart-lang/shelf — a resolved root package,
        // one nested package missing package_config.json (fresh clone), and a
        // non-package directory. Only the unresolved nested package is flagged.
        let root = tempfile::tempdir().unwrap();
        let r = root.path();

        // Root package: resolved (has .dart_tool/package_config.json).
        std::fs::write(r.join("pubspec.yaml"), "name: app\n").unwrap();
        std::fs::create_dir_all(r.join(".dart_tool")).unwrap();
        std::fs::write(r.join(".dart_tool/package_config.json"), "{}").unwrap();

        // Nested package: unresolved (no .dart_tool).
        let shelf = r.join("pkgs/shelf");
        std::fs::create_dir_all(&shelf).unwrap();
        std::fs::write(shelf.join("pubspec.yaml"), "name: shelf\n").unwrap();

        // A plain source dir with no pubspec: never a package.
        std::fs::create_dir_all(r.join("lib/src")).unwrap();

        let got = unresolved_dep_packages(r);
        assert_eq!(got, vec!["pkgs/shelf".to_string()]);
    }

    #[test]
    fn sdk_root_via_dart_resolves_installed_sdk() {
        // U6a: the shim fallback must run a program the installed Dart accepts
        // (unlike the old invalid `--print-sdk-path`) and resolve a real SDK
        // root — never silently no-op. Gated on a real `dart` being on PATH so
        // CI images without Dart skip rather than fail.
        let dart_name = format!("dart{}", std::env::consts::EXE_SUFFIX);
        let Some(dart) = std::env::var_os("PATH").and_then(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join(&dart_name))
                .find(|c| c.exists())
        }) else {
            eprintln!("skipping sdk_root_via_dart: no `dart` on PATH");
            return;
        };
        let sdk = sdk_root_via_dart(&dart).expect("dart probe should resolve an SDK root");
        assert!(
            sdk.join("lib/_internal/allowed_experiments.json").is_file(),
            "resolved path {sdk:?} is not a Dart SDK root"
        );
    }
}
