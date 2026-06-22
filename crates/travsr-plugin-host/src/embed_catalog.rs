// Static catalog of downloadable embed backends.
//
// Adding a new backend = one catalog entry + a plugin binary release in
// Travsr-com/travsr-embed. Zero changes to travsr-core, travsr-store, or the
// main binary are required.
//
// Mirrors the Phase B PhaseBEntry / CATALOG pattern in phase_b/catalog.rs.

/// One model file the CLI must download from HuggingFace.
#[derive(Debug, Clone, Copy)]
pub struct EmbedModelFile {
    /// Filename placed under `~/.travsr/models/<backend_id>/`.
    pub name: &'static str,
    /// Path component after the HuggingFace base URL, e.g. `"onnx/model_int8.onnx"`.
    pub url_path: &'static str,
    /// HuggingFace repo slug, e.g. `"nomic-ai/nomic-embed-text-v1.5"`.
    pub hf_repo: &'static str,
    /// Approximate download size in MiB — shown in `travsr embed init` progress.
    pub size_hint_mb: u32,
}

/// One downloadable embedding backend.
///
/// The sidecar binary (`travsr-embed-<id>`) encapsulates the ONNX runtime,
/// sqlite-vec, and all model weights. The main binary only needs reqwest to
/// download the binary + model files; it has zero new native deps.
#[derive(Debug, Clone, Copy)]
pub struct EmbedBackend {
    /// Stable identifier stored as `model_id` in `node_embeddings`. Must be
    /// unique across backends. Changing this value invalidates existing embeddings.
    pub id: &'static str,
    /// One-line description shown by `travsr embed list`.
    pub description: &'static str,
    /// Raw embedding dimension (before any MRL truncation applied by the plugin).
    pub dim: u32,
    /// Binary name on disk, e.g. `"travsr-embed-nomic-v1.5-int8"`. The CLI
    /// appends the Rust target triple when downloading from GitHub Releases.
    pub binary_name: &'static str,
    /// GitHub repo slug for `travsr embed init` binary download.
    pub github_repo: &'static str,
    /// Version tag used when the GitHub API is unreachable.
    pub version_fallback: &'static str,
    /// Model files the CLI must download into `~/.travsr/models/<id>/`.
    pub model_files: &'static [EmbedModelFile],
}

pub const BACKENDS: &[EmbedBackend] = &[
    EmbedBackend {
        id: "nomic-v1.5-int8",
        description: "nomic-embed-text-v1.5 int8 ONNX — 137 MB, MRL-256 (dim=256), local inference",
        dim: 256,
        binary_name: "travsr-embed-nomic",
        github_repo: "Travsr-com/travsr-embed",
        version_fallback: "v1.0.0",
        model_files: &[
            EmbedModelFile {
                name: "model_int8.onnx",
                url_path: "onnx/model_int8.onnx",
                hf_repo: "nomic-ai/nomic-embed-text-v1.5",
                size_hint_mb: 137,
            },
            EmbedModelFile {
                name: "tokenizer.json",
                url_path: "tokenizer.json",
                hf_repo: "nomic-ai/nomic-embed-text-v1.5",
                size_hint_mb: 1,
            },
        ],
    },
    // Future backends — adding one requires only a catalog entry + plugin binary release:
    // EmbedBackend { id: "voyage-code-3", ... }
    // EmbedBackend { id: "openai-text-3-small", ... }
];

/// Look up a backend by its stable id string.
pub fn lookup(id: &str) -> Option<&'static EmbedBackend> {
    BACKENDS.iter().find(|b| b.id == id)
}

/// Spawn `travsr-embed-<backend> --reindex <db_path> --phase1 <threshold>` as a
/// fully detached background process.  Returns `true` if a process was launched.
///
/// Phase 1 covers symbol nodes with `shell_number >= threshold` — the
/// high-centrality core.  The sidecar rebuilds the HNSW index at the end so
/// KNN queries become available as soon as this pass finishes.
///
/// Silently returns `false` when no backend is configured or the binary is
/// missing (`travsr embed init` not yet run).
pub fn spawn_background_reindex_phase1(db_path: &std::path::Path, threshold: u32) -> bool {
    (|| -> Option<bool> {
        let backend_id = active_backend_id()?;
        let backend = lookup(&backend_id)?;
        let home = dirs::home_dir()?;
        let bin_path = home
            .join(".travsr")
            .join("bin")
            .join(backend.binary_name);
        if !bin_path.exists() {
            return Some(false);
        }
        // RFC-019: pass embed.db path explicitly so the sidecar writes embeddings
        // there instead of into graph.db, eliminating WAL write contention.
        let embed_db_path = db_path.with_file_name("embed.db");
        std::process::Command::new(&bin_path)
            .arg("--reindex")
            .arg(db_path)
            .arg("--embed-db")
            .arg(&embed_db_path)
            .arg("--phase1")
            .arg(threshold.to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        Some(true)
    })()
    .unwrap_or(false)
}

/// Spawn `travsr-embed-<backend> --reindex <db_path> --phase2 <threshold>` as a
/// fully detached background process.  Returns `true` if a process was launched.
///
/// Phase 2 covers symbol nodes with `shell_number < threshold` — the low-centrality
/// symbols deferred from the fast Phase 1 pass.  The sidecar skips inline HNSW
/// updates and rebuilds the full index at the end, so both Phase 1 and Phase 2
/// nodes end up in the final HNSW without a file-write race.
///
/// Silently returns `false` when no backend is configured or the binary is missing
/// (`travsr embed init` not yet run) — callers should treat this as a no-op.
pub fn spawn_background_reindex_phase2(db_path: &std::path::Path, threshold: u32) -> bool {
    (|| -> Option<bool> {
        let backend_id = active_backend_id()?;
        let backend = lookup(&backend_id)?;
        let home = dirs::home_dir()?;
        let bin_path = home
            .join(".travsr")
            .join("bin")
            .join(backend.binary_name);
        if !bin_path.exists() {
            return Some(false);
        }
        let embed_db_path = db_path.with_file_name("embed.db");
        std::process::Command::new(&bin_path)
            .arg("--reindex")
            .arg(db_path)
            .arg("--embed-db")
            .arg(&embed_db_path)
            .arg("--phase2")
            .arg(threshold.to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        Some(true)
    })()
    .unwrap_or(false)
}

/// Spawn `travsr-embed-<backend> --reindex <db_path>` as a fully detached
/// background process.  Embeds ALL symbol nodes that do not yet have an
/// embedding row, regardless of `shell_number`.  Returns `true` if launched.
///
/// Used after Phase B completes to pick up newly-added SCIP semantic nodes
/// that were absent when Phase 1/2 ran (those nodes have `shell_number = NULL`
/// and are invisible to `spawn_background_reindex_phase2`'s threshold filter).
///
/// The sidecar is idempotent — nodes already embedded are skipped via the
/// `NOT EXISTS` subquery in the reindex SQL — so calling this when no new
/// nodes exist is a fast no-op.
///
/// Silently returns `false` when no backend is configured or the binary is
/// missing (`travsr embed init` not yet run).
pub fn spawn_background_reindex_all(db_path: &std::path::Path) -> bool {
    (|| -> Option<bool> {
        let backend_id = active_backend_id()?;
        let backend = lookup(&backend_id)?;
        let home = dirs::home_dir()?;
        let bin_path = home
            .join(".travsr")
            .join("bin")
            .join(backend.binary_name);
        if !bin_path.exists() {
            return Some(false);
        }
        let embed_db_path = db_path.with_file_name("embed.db");
        std::process::Command::new(&bin_path)
            .arg("--reindex")
            .arg(db_path)
            .arg("--embed-db")
            .arg(&embed_db_path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        Some(true)
    })()
    .unwrap_or(false)
}

/// Read the active backend id from `~/.travsr/embed.toml`.
/// Returns `None` when the file is absent, unreadable, or has no `active` key.
/// Used by the daemon to select the correct sidecar without depending on travsr-cli.
pub fn active_backend_id() -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Config {
        active: Option<String>,
    }
    let home = dirs::home_dir()?;
    let content = std::fs::read_to_string(home.join(".travsr").join("embed.toml")).ok()?;
    let cfg: Config = toml::from_str(&content).ok()?;
    cfg.active
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_ids_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for b in BACKENDS {
            assert!(seen.insert(b.id), "duplicate backend id: {}", b.id);
        }
    }

    #[test]
    fn lookup_finds_nomic() {
        let b = lookup("nomic-v1.5-int8").expect("nomic backend must be in catalog");
        assert_eq!(b.dim, 256);
        assert!(!b.model_files.is_empty());
    }

    #[test]
    fn lookup_returns_none_for_unknown() {
        assert!(lookup("__nonexistent__").is_none());
    }
}
