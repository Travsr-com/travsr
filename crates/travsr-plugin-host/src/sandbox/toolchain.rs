//! Per-language toolchain access for Phase B sandboxes.
//!
//! Build-tool analyzers (`scip-go`, `scip-java`, …) drive the language's real
//! build to resolve symbols. To do that they must read their language's module /
//! build caches and see the toolchain's environment variables. The default
//! `Standard` sandbox (ADR-017 Rule 1) `env_clear`s everything and denies reads
//! outside the repo + scratch, so the analyzer resolves **zero** packages and
//! emits an empty index (observed: scip-go indexes `[0/0]` packages under the
//! sandbox vs 244 implementations unsandboxed).
//!
//! This module computes the **minimal** extra read/write paths + env each
//! language's analyzer needs, so the sandbox can grant exactly those and nothing
//! more. Languages with no out-of-repo toolchain needs (e.g. builtins) return an
//! empty grant set — a no-op.
//!
//! Extending to a new language = add a match arm with that toolchain's caches +
//! env (e.g. java → `~/.gradle`, `~/.m2`, `JAVA_HOME`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

/// Extra sandbox grants a language's Phase B analyzer needs beyond the repo +
/// scratch. All paths are host paths; the sandbox layer canonicalizes and maps
/// them per-platform (sandbox-exec subpaths on macOS, bwrap binds on Linux).
#[derive(Debug, Clone, Default)]
pub struct ToolchainAccess {
    /// Directories the analyzer must be able to **read** (toolchain, module cache).
    pub read_paths: Vec<PathBuf>,
    /// Directories the analyzer must be able to **write** (build cache).
    pub write_paths: Vec<PathBuf>,
    /// Env vars to pass through into the otherwise-cleared sandbox env.
    pub env: Vec<(String, String)>,
}

/// Compute the toolchain grants for a language's Phase B analyzer.
/// Empty for languages with no out-of-repo toolchain needs.
pub fn toolchain_access(language: &str) -> ToolchainAccess {
    match language {
        "go" => go_access(),
        // TODO(phase-b-toolchain): java → ~/.gradle, ~/.m2, JAVA_HOME, GRADLE_USER_HOME;
        //   python → site-packages / venv; csharp → ~/.nuget. Same pattern as go_access.
        _ => ToolchainAccess::default(),
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Run `go env <keys>` and return key→value. Empty if `go` is not on PATH or fails.
/// `go env KEY1 KEY2` prints one value per line in argument order.
fn go_env(keys: &[&str]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(output) = Command::new("go").arg("env").args(keys).output() else {
        return out;
    };
    if !output.status.success() {
        return out;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for (k, v) in keys.iter().zip(text.lines()) {
        let v = v.trim();
        if !v.is_empty() {
            out.insert((*k).to_string(), v.to_string());
        }
    }
    out
}

/// scip-go uses `go/packages` (i.e. the real `go` toolchain) to load packages, so
/// it needs: the module cache (`GOMODCACHE`, read), the build cache (`GOCACHE`,
/// read+write), `GOPATH` (covers `~/go/bin/scip-go` itself + `pkg/mod`), and the
/// `GO*`/`HOME` env so the toolchain finds them. Paths resolved via `go env`, with
/// `$HOME`-based fallbacks if `go` is not reachable at sandbox-build time.
fn go_access() -> ToolchainAccess {
    let env_map = go_env(&["GOPATH", "GOCACHE", "GOMODCACHE"]);

    let gopath = env_map
        .get("GOPATH")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join("go")));
    let gomodcache = env_map
        .get("GOMODCACHE")
        .map(PathBuf::from)
        .or_else(|| gopath.as_ref().map(|p| p.join("pkg/mod")));
    let gocache = env_map.get("GOCACHE").map(PathBuf::from).or_else(|| {
        // Default GOCACHE: macOS → $HOME/Library/Caches/go-build, Linux → $HOME/.cache/go-build.
        home().map(|h| {
            if cfg!(target_os = "macos") {
                h.join("Library/Caches/go-build")
            } else {
                h.join(".cache/go-build")
            }
        })
    });

    let mut read_paths = Vec::new();
    let mut write_paths = Vec::new();
    let mut env = Vec::new();

    if let Some(p) = &gopath {
        read_paths.push(p.clone()); // covers ~/go/bin/scip-go + pkg/mod
        env.push(("GOPATH".to_string(), p.to_string_lossy().into_owned()));
    }
    if let Some(p) = &gomodcache {
        read_paths.push(p.clone()); // modules are read-only
        env.push(("GOMODCACHE".to_string(), p.to_string_lossy().into_owned()));
    }
    if let Some(p) = &gocache {
        read_paths.push(p.clone());
        write_paths.push(p.clone()); // `go build` writes compiled artifacts here
        env.push(("GOCACHE".to_string(), p.to_string_lossy().into_owned()));
    }
    if let Some(h) = home() {
        // Go tooling consults $HOME for defaults; pass it through.
        env.push(("HOME".to_string(), h.to_string_lossy().into_owned()));
    }

    ToolchainAccess {
        read_paths,
        write_paths,
        env,
    }
}
