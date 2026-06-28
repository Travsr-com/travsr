// Static catalog of downloadable embed backends + single-sidecar reindex orchestrator.
//
// RFC-021: single-sidecar parallel embedding — the orchestrator spawns ONE sidecar
// process with `--parallel N`. The sidecar loads the model ONCE (~270 MB) and uses
// N internal reader threads to feed one inference thread. This eliminates the
// N × 270 MB model-load OOM that RFC-020's multi-process design caused on 8 GB machines.
//
// RAM formula: 1 × model_weights + 1 × SQLite caches + OS overhead.
// Previously: N × model_weights + N × SQLite caches → OOM at N = 8 on 8 GB.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Hard ceiling on parallel reader threads inside the sidecar.
/// Override with TRAVSR_EMBED_WORKERS env var.
pub const MAX_EMBED_WORKERS: usize = 8;

/// FT-H2: maximum wall-clock time for one reindex pass.
/// A large repo with 100 k nodes × 384-dim embeddings at ~500 nodes/s takes
/// ~3 min; 30 min gives 6× headroom. Override with TRAVSR_REINDEX_TIMEOUT_SECS.
fn reindex_timeout_secs() -> u64 {
    std::env::var("TRAVSR_REINDEX_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&x: &u64| x > 0)
        .unwrap_or(30 * 60)
}

/// FT-H1: process-level single-flight guard.
/// Prevents Phase1/Phase2/post-commit from launching ≥2 reindex sidecars that
/// write to the same embed.db concurrently (no temp-db isolation in RFC-021).
static REINDEX_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// RAM budget for the whole reindex run (single model + caches + overhead).
/// Used by derive_num_workers() to cap the reader-thread count only, not
/// the number of sidecar processes (which is always 1).
const WORKER_RAM_BUDGET_MB: u64 = 500;

/// Fraction of symbol nodes targeted by Phase 1 (eager) embedding.
/// The shell_number threshold is derived so that nodes with
/// shell_number >= threshold cover at most this fraction of total symbol nodes.
/// Smaller repos where this fraction covers nearly all nodes skip the phase split.
const PHASE1_COVERAGE_FRACTION: f64 = 0.25;

// ── Catalog types ─────────────────────────────────────────────────────────────

/// One model file the CLI must download from HuggingFace.
#[derive(Debug, Clone, Copy)]
pub struct EmbedModelFile {
    /// Filename placed under `~/.travsr/models/<backend_id>/`.
    pub name: &'static str,
    /// Path component after the HuggingFace base URL, e.g. `"onnx/model_int8.onnx"`.
    pub url_path: &'static str,
    /// HuggingFace repo slug, e.g. `"BAAI/bge-small-en-v1.5"`.
    pub hf_repo: &'static str,
    /// Approximate download size in MiB — shown in `travsr embed init` progress.
    pub size_hint_mb: u32,
}

/// One downloadable embedding backend.
#[derive(Debug, Clone, Copy)]
pub struct EmbedBackend {
    pub id: &'static str,
    pub description: &'static str,
    pub dim: u32,
    /// Model parameter count in millions (e.g. 33 for bge-small).
    pub params_m: u32,
    /// MTEB retrieval score (higher = better, max ~68).
    pub mteb: f32,
    /// Approximate peak RAM during reindex in MB.
    pub ram_mb: u32,
    /// Approximate reindex time in seconds for a 3.5k-node repo.
    pub init_secs: u32,
    pub binary_name: &'static str,
    pub github_repo: &'static str,
    pub version_fallback: &'static str,
    pub model_files: &'static [EmbedModelFile],
}

pub const BACKENDS: &[EmbedBackend] = &[
    EmbedBackend {
        id: "bge-small-en-v1.5",
        description: "Fastest. Good for everyday use on any machine. May miss specialised \
                      technical vocabulary (e.g. algorithm-specific jargon).",
        dim: 384,
        params_m: 33,
        mteb: 62.2,
        ram_mb: 200,
        init_secs: 11,
        binary_name: "travsr-embed",
        github_repo: "Travsr-com/travsr-embed",
        version_fallback: "v1.0.0",
        model_files: &[
            EmbedModelFile {
                name: "model.onnx",
                url_path: "onnx/model.onnx",
                hf_repo: "BAAI/bge-small-en-v1.5",
                size_hint_mb: 127,
            },
            EmbedModelFile {
                name: "tokenizer.json",
                url_path: "tokenizer.json",
                hf_repo: "BAAI/bge-small-en-v1.5",
                size_hint_mb: 1,
            },
        ],
    },
    EmbedBackend {
        id: "bge-base-en-v1.5",
        description: "Recommended for technical codebases. 3× parameter increase closes \
                      specialised vocabulary gaps (algorithm names, domain jargon). \
                      ~4× slower reindex; same BERT architecture, ARM64 compatible.",
        dim: 768,
        params_m: 109,
        mteb: 63.6,
        ram_mb: 450,
        init_secs: 47,
        binary_name: "travsr-embed",
        github_repo: "Travsr-com/travsr-embed",
        version_fallback: "v1.0.0",
        model_files: &[
            EmbedModelFile {
                name: "model.onnx",
                url_path: "onnx/model.onnx",
                hf_repo: "BAAI/bge-base-en-v1.5",
                size_hint_mb: 270,
            },
            EmbedModelFile {
                name: "tokenizer.json",
                url_path: "tokenizer.json",
                hf_repo: "BAAI/bge-base-en-v1.5",
                size_hint_mb: 1,
            },
        ],
    },
    EmbedBackend {
        id: "bge-large-en-v1.5",
        description: "Maximum accuracy. Best semantic coverage for large or multilingual \
                      codebases. Requires ~1.4 GB RAM; ~13× slower reindex than small. \
                      Not recommended on machines with less than 8 GB available RAM.",
        dim: 1024,
        params_m: 335,
        mteb: 64.2,
        ram_mb: 1400,
        init_secs: 150,
        binary_name: "travsr-embed",
        github_repo: "Travsr-com/travsr-embed",
        version_fallback: "v1.0.0",
        model_files: &[
            EmbedModelFile {
                name: "model.onnx",
                url_path: "onnx/model.onnx",
                hf_repo: "BAAI/bge-large-en-v1.5",
                size_hint_mb: 800,
            },
            EmbedModelFile {
                name: "tokenizer.json",
                url_path: "tokenizer.json",
                hf_repo: "BAAI/bge-large-en-v1.5",
                size_hint_mb: 1,
            },
        ],
    },
];

/// Look up a backend by its stable id string.
pub fn lookup(id: &str) -> Option<&'static EmbedBackend> {
    BACKENDS.iter().find(|b| b.id == id)
}

// ── Orchestrator types ────────────────────────────────────────────────────────

/// Which subset of nodes each reindex pass covers.
#[derive(Debug, Clone, Copy)]
enum PhaseFilter {
    /// Phase 1: high-centrality symbols with shell_number >= threshold.
    Phase1 { threshold: u32 },
    /// Phase 2: low-centrality symbols with shell_number < threshold (or NULL).
    Phase2 { threshold: u32 },
    /// All: embed every pending symbol regardless of shell_number.
    All,
}

impl PhaseFilter {
    fn sidecar_flag(&self) -> Option<(&'static str, u32)> {
        match self {
            PhaseFilter::Phase1 { threshold } => Some(("--phase1", *threshold)),
            PhaseFilter::Phase2 { threshold } => Some(("--phase2", *threshold)),
            PhaseFilter::All => None,
        }
    }
}

// ── C-01: thread count ────────────────────────────────────────────────────────

/// Derive the number of parallel reader threads for the sidecar (C-01).
///
/// Priority: TRAVSR_EMBED_WORKERS env var → P-core count (clamped) → RAM guard.
/// Note: this controls reader threads INSIDE one sidecar, not process count.
fn derive_num_workers() -> usize {
    let env_override = std::env::var("TRAVSR_EMBED_WORKERS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok());

    let logical = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    derive_num_workers_inner(logical, available_memory_mb(), env_override)
}

/// Pure inner function — takes all inputs explicitly so tests never touch env vars.
fn derive_num_workers_inner(
    logical_cpus: usize,
    ram_mb: u64,
    env_override: Option<usize>,
) -> usize {
    if let Some(n) = env_override {
        return n.clamp(1, MAX_EMBED_WORKERS);
    }
    let cpu_bound = logical_cpus.min(MAX_EMBED_WORKERS);
    if ram_mb > 0 {
        let ram_bound = ((ram_mb / WORKER_RAM_BUDGET_MB) as usize).max(1);
        cpu_bound.min(ram_bound)
    } else {
        cpu_bound
    }
}

/// Best-effort AVAILABLE RAM in MiB (not total physical RAM).
///
/// macOS: parses `vm_stat` for free + inactive pages × page_size.
///        This reflects what's actually available, not just installed.
/// Linux: reads MemAvailable from /proc/meminfo (already available-based).
/// Returns 0 when unavailable — caller skips the RAM guard.
fn available_memory_mb() -> u64 {
    #[cfg(target_os = "macos")]
    {
        let out = Command::new("vm_stat").output().ok();
        if let Some(out) = out {
            let text = String::from_utf8_lossy(&out.stdout);
            // First line: "Mach Virtual Memory Statistics: (page size of 16384 bytes)"
            let page_size: u64 = text
                .lines()
                .next()
                .and_then(|l| {
                    let s = l.trim();
                    let i = s.find("page size of ")?;
                    let rest = &s[i + "page size of ".len()..];
                    rest.split_whitespace().next()?.parse().ok()
                })
                .unwrap_or(16_384);
            let mut free_pages: u64 = 0;
            let mut inactive_pages: u64 = 0;
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix("Pages free:") {
                    free_pages = rest.trim().trim_end_matches('.').parse().unwrap_or(0);
                } else if let Some(rest) = line.strip_prefix("Pages inactive:") {
                    inactive_pages = rest.trim().trim_end_matches('.').parse().unwrap_or(0);
                }
            }
            let available_bytes = (free_pages + inactive_pages) * page_size;
            if available_bytes > 0 {
                return available_bytes / (1024 * 1024);
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        // /proc/meminfo MemAvailable line: "MemAvailable:   12345678 kB"
        if let Ok(content) = std::fs::read_to_string("/proc/meminfo") {
            for line in content.lines() {
                if let Some(rest) = line.strip_prefix("MemAvailable:") {
                    if let Ok(kb) = rest.trim().trim_end_matches(" kB").parse::<u64>() {
                        return kb / 1024;
                    }
                }
            }
        }
    }
    0
}

// ── Per-repo threshold derivation ────────────────────────────────────────────

/// Public re-export for `travsr embed reindex` so it can show the resolved worker count.
pub fn derive_num_workers_for_cli() -> usize {
    derive_num_workers()
}

/// Public re-export for `travsr embed status` so it shows the real threshold label.
pub fn derive_phase1_threshold_for_status(db_path: &Path) -> Option<u32> {
    derive_phase1_threshold(db_path, PHASE1_COVERAGE_FRACTION)
}

/// Derive per-repo Phase 1 shell_number threshold from the k-core distribution.
///
/// Returns the minimum shell_number such that symbol nodes with
/// shell_number >= threshold cover at most `fraction` of all symbol nodes.
/// This makes Phase 1 time roughly proportional to repo size while always
/// embedding the structurally most important nodes first.
///
/// Returns None when the db has no shell_number data (pre-k-core run) or
/// when every node is covered before reaching the fraction limit (small repos).
fn derive_phase1_threshold(db_path: &Path, fraction: f64) -> Option<u32> {
    let conn = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    conn.query_row(
        "SELECT MIN(shell_number) \
         FROM ( \
             SELECT shell_number, \
                    SUM(COUNT(*)) OVER (ORDER BY shell_number DESC) AS cum, \
                    SUM(COUNT(*)) OVER ()                            AS total \
             FROM nodes \
             WHERE kind NOT IN \
                 ('file','file-module','import','module','field','variable') \
               AND shell_number IS NOT NULL \
             GROUP BY shell_number \
         ) \
         WHERE CAST(cum AS REAL) / total <= ?1",
        [fraction],
        |r| r.get(0),
    )
    .ok()
    .flatten()
}

// ── Core orchestrator ─────────────────────────────────────────────────────────

/// Spawn ONE sidecar with `--parallel N` and wait for it to complete.
///
/// RFC-021: one sidecar process loads the model once; N reader threads inside
/// the sidecar feed a single inference loop. No temp-db management, no merge.
fn run_parallel_reindex(
    bin_path: &Path,
    db_path: &Path,
    embed_db_path: &Path,
    _model_id: &str,
    phase: PhaseFilter,
    quiet: bool,
) -> anyhow::Result<()> {
    let n = derive_num_workers();

    let mut cmd = Command::new(bin_path);
    cmd.arg("--model-id")
        .arg(_model_id)
        .arg("--reindex")
        .arg(db_path)
        .arg("--embed-db")
        .arg(embed_db_path)
        .arg("--parallel")
        .arg(n.to_string())
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    // Suppress sidecar stdout when the caller renders its own progress UI
    // (e.g. `travsr embed init`). Background daemon paths leave it inherited.
    if quiet {
        cmd.stdout(Stdio::null());
    }

    if let Some((flag, val)) = phase.sidecar_flag() {
        cmd.arg(flag).arg(val.to_string());
    }

    tracing::info!(
        n,
        phase = ?phase,
        "embed: spawning sidecar (single model, {} reader threads)",
        n
    );

    // FT-H2: spawn + deadline poll so unkillable/hung sidecars don't orphan
    // the background reindex thread indefinitely. cmd.status() blocks forever
    // when the sidecar stalls; try_wait() lets us enforce a hard deadline.
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "embed: failed to spawn sidecar");
            return Err(anyhow::Error::from(e).context("failed to spawn embed sidecar"));
        }
    };
    let timeout = reindex_timeout_secs();
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout);
    loop {
        match child.try_wait() {
            Ok(Some(s)) if s.success() => {
                tracing::info!(n, "embed: reindex completed");
                return Ok(());
            }
            Ok(Some(s)) => {
                tracing::warn!(exit = ?s.code(), "embed: sidecar exited with failure");
                anyhow::bail!("embed sidecar exited with code {:?}", s.code());
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    tracing::warn!(
                        timeout_secs = timeout,
                        "embed: reindex sidecar timed out — killing"
                    );
                    let _ = child.kill();
                    let _ = child.wait();
                    anyhow::bail!("embed sidecar reindex timed out after {timeout}s");
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(e) => {
                anyhow::bail!("embed sidecar wait error: {e}");
            }
        }
    }
}

// ── Public spawn functions ────────────────────────────────────────────────────

/// Spawn reindex for Phase 1 (shell_number >= derived threshold) as a detached
/// background thread. Returns true if the orchestrator thread was launched.
///
/// The threshold is derived per-repo from the k-core shell_number distribution
/// so that Phase 1 covers the top PHASE1_COVERAGE_FRACTION of symbol nodes by
/// centrality. This ensures Phase 1 completes in a few minutes regardless of
/// repo size while always embedding the structurally most important nodes first.
pub fn spawn_background_reindex_phase1(db_path: &Path) -> bool {
    // FT-H1: single-flight guard — skip if another reindex is already running.
    if REINDEX_IN_FLIGHT
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        tracing::debug!("embed Phase 1: reindex already in-flight — skipping");
        return false;
    }
    let Some((bin_path, embed_db_path, model_id)) = resolve_backend(db_path) else {
        REINDEX_IN_FLIGHT.store(false, Ordering::SeqCst);
        return false;
    };
    let Some(threshold) = derive_phase1_threshold(db_path, PHASE1_COVERAGE_FRACTION) else {
        REINDEX_IN_FLIGHT.store(false, Ordering::SeqCst);
        tracing::warn!(
            db = %db_path.display(),
            "embed Phase 1: k-core data not ready — skipping (will retry after next Phase B)"
        );
        return false;
    };
    tracing::info!(threshold, "embed Phase 1: derived shell_number threshold");
    let db_path = db_path.to_path_buf();
    std::thread::Builder::new()
        .name("embed-reindex-phase1".into())
        .spawn(move || {
            if let Err(e) = run_parallel_reindex(
                &bin_path,
                &db_path,
                &embed_db_path,
                &model_id,
                PhaseFilter::Phase1 { threshold },
                false,
            ) {
                tracing::warn!("embed Phase 1 failed: {e:#}");
            }
            REINDEX_IN_FLIGHT.store(false, Ordering::SeqCst);
        })
        .is_ok()
}

/// Spawn reindex for Phase 2 (shell_number < derived threshold) as a detached
/// background thread. Returns true if the orchestrator thread was launched.
///
/// Uses the same threshold derivation as Phase 1 so the two phases are
/// complementary and together cover all symbol nodes.
pub fn spawn_background_reindex_phase2(db_path: &Path) -> bool {
    // FT-H1: single-flight guard.
    if REINDEX_IN_FLIGHT
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        tracing::debug!("embed Phase 2: reindex already in-flight — skipping");
        return false;
    }
    let Some((bin_path, embed_db_path, model_id)) = resolve_backend(db_path) else {
        REINDEX_IN_FLIGHT.store(false, Ordering::SeqCst);
        return false;
    };
    // Derive same threshold as Phase 1 for complementary coverage.
    // If k-core data is missing, fall back to embedding all remaining nodes.
    let phase = match derive_phase1_threshold(db_path, PHASE1_COVERAGE_FRACTION) {
        Some(threshold) => {
            tracing::info!(threshold, "embed Phase 2: derived shell_number threshold");
            PhaseFilter::Phase2 { threshold }
        }
        None => {
            tracing::warn!("embed Phase 2: k-core data missing — embedding all remaining nodes");
            PhaseFilter::All
        }
    };
    let db_path = db_path.to_path_buf();
    std::thread::Builder::new()
        .name("embed-reindex-phase2".into())
        .spawn(move || {
            if let Err(e) =
                run_parallel_reindex(&bin_path, &db_path, &embed_db_path, &model_id, phase, false)
            {
                tracing::warn!("embed Phase 2 failed: {e:#}");
            }
            REINDEX_IN_FLIGHT.store(false, Ordering::SeqCst);
        })
        .is_ok()
}

/// Spawn reindex for all pending nodes (used after Phase B completes)
/// as a detached background thread. Returns true if launched.
pub fn spawn_background_reindex_all(db_path: &Path) -> bool {
    // FT-H1: single-flight guard.
    if REINDEX_IN_FLIGHT
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        tracing::debug!("embed reindex-all: reindex already in-flight — skipping");
        return false;
    }
    let Some((bin_path, embed_db_path, model_id)) = resolve_backend(db_path) else {
        REINDEX_IN_FLIGHT.store(false, Ordering::SeqCst);
        return false;
    };
    let db_path = db_path.to_path_buf();
    std::thread::Builder::new()
        .name("embed-reindex-all".into())
        .spawn(move || {
            if let Err(e) = run_parallel_reindex(
                &bin_path,
                &db_path,
                &embed_db_path,
                &model_id,
                PhaseFilter::All,
                false,
            ) {
                tracing::warn!("embed reindex-all failed: {e:#}");
            }
            REINDEX_IN_FLIGHT.store(false, Ordering::SeqCst);
        })
        .is_ok()
}

/// Blocking reindex — called from `travsr embed reindex` (CLI path).
///
/// Runs the sidecar in the calling thread. Suitable for interactive use
/// where the caller wants to wait for completion.
/// Returns Err if the backend is not installed.
pub fn run_parallel_reindex_blocking(
    db_path: &Path,
    phase1_threshold: Option<u32>,
) -> anyhow::Result<()> {
    let (bin_path, embed_db_path, model_id) = resolve_backend(db_path).ok_or_else(|| {
        anyhow::anyhow!(
            "No embedding backend active or binary missing. \
             Run `travsr embed init` first."
        )
    })?;

    let phase = match phase1_threshold {
        Some(t) => PhaseFilter::Phase1 { threshold: t },
        None => PhaseFilter::All,
    };

    run_parallel_reindex(&bin_path, db_path, &embed_db_path, &model_id, phase, false)
}

/// Blocking reindex with sidecar stdout suppressed — called from `travsr embed init`
/// which renders its own progress UI. All nodes, no phase split.
pub fn run_parallel_reindex_blocking_quiet(db_path: &Path) -> anyhow::Result<()> {
    let (bin_path, embed_db_path, model_id) = resolve_backend(db_path).ok_or_else(|| {
        anyhow::anyhow!(
            "No embedding backend active or binary missing. \
             Run `travsr embed init` first."
        )
    })?;
    run_parallel_reindex(
        &bin_path,
        db_path,
        &embed_db_path,
        &model_id,
        PhaseFilter::All,
        true,
    )
}

// ── Shared resolution helper ──────────────────────────────────────────────────

/// Resolve the active backend's binary path, embed.db path, and model_id.
///
/// Reads the PER-REPO config from `<repo>/.travsr/embed.toml` (derived from
/// `db_path`). Returns `None` when the repo has not been configured with
/// `travsr embed init` — callers must not auto-embed unconfigured repos.
fn resolve_backend(db_path: &Path) -> Option<(PathBuf, PathBuf, String)> {
    let repo_root = db_path.parent().and_then(|p| p.parent())?;
    let backend_id = repo_backend_id(repo_root)?;
    let backend = lookup(&backend_id)?;
    let home = dirs::home_dir()?;
    let bin_path = home.join(".travsr").join("bin").join(backend.binary_name);
    if !bin_path.exists() {
        return None;
    }
    let embed_db_path = db_path.with_file_name("embed.db");
    Some((bin_path, embed_db_path, backend_id))
}

// ── Config helpers ────────────────────────────────────────────────────────────

/// Read the embed backend configured for a specific repo.
///
/// Reads `<repo_root>/.travsr/embed.toml`. Returns `None` when absent —
/// absence means "not configured; do not auto-embed this repo."
pub fn repo_backend_id(repo_root: &Path) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Config {
        active: Option<String>,
    }
    let content = std::fs::read_to_string(repo_root.join(".travsr").join("embed.toml")).ok()?;
    let cfg: Config = toml::from_str(&content).ok()?;
    cfg.active
}

/// Write the per-repo embed model choice to `<repo_root>/.travsr/embed.toml`.
///
/// Called by `travsr embed init` after the user selects a model in a repo.
/// The `.travsr/` directory must already exist (guaranteed by `travsr init`).
pub fn write_repo_backend_id(repo_root: &Path, backend_id: &str) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let path = repo_root.join(".travsr").join("embed.toml");
    let content = format!("active = \"{backend_id}\"\n");
    std::fs::write(&path, content)
        .with_context(|| format!("writing repo embed config to {}", path.display()))
}

/// Read the active backend id from `~/.travsr/embed.toml`.
///
/// Machine-scoped: records what is installed. Use `repo_backend_id` for
/// per-repo embedding decisions; this is only for install/list/hint paths.
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── TC-00: catalog invariants ─────────────────────────────────────────────

    #[test]
    fn catalog_ids_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for b in BACKENDS {
            assert!(seen.insert(b.id), "duplicate backend id: {}", b.id);
        }
    }

    #[test]
    fn lookup_finds_bge() {
        let b = lookup("bge-small-en-v1.5").expect("bge backend must be in catalog");
        assert_eq!(b.dim, 384);
        assert!(!b.model_files.is_empty());
    }

    #[test]
    fn lookup_returns_none_for_unknown() {
        assert!(lookup("__nonexistent__").is_none());
    }

    // ── TC-01: worker count clamping ──────────────────────────────────────────

    #[test]
    fn worker_count_env_override_is_clamped() {
        assert_eq!(
            derive_num_workers_inner(2, 16_000, Some(0)),
            1,
            "0 should clamp to 1"
        );
        assert_eq!(
            derive_num_workers_inner(16, 64_000, Some(100)),
            MAX_EMBED_WORKERS,
            "100 should clamp to MAX_EMBED_WORKERS"
        );
        assert_eq!(derive_num_workers_inner(8, 16_000, Some(3)), 3);
    }

    #[test]
    fn worker_count_cpu_bound() {
        assert_eq!(derive_num_workers_inner(2, 16_000, None), 2);
        assert_eq!(
            derive_num_workers_inner(12, 64_000, None),
            MAX_EMBED_WORKERS
        );
    }

    #[test]
    fn worker_count_ram_guard() {
        // 8 cores, 2 000 MB available → floor(2000 / 500) = 4
        assert_eq!(derive_num_workers_inner(8, 2_000, None), 4);
        // 8 cores, 300 MB available → max(floor(300/500), 1) = 1
        assert_eq!(derive_num_workers_inner(8, 300, None), 1);
        // ram_mb = 0 means "unknown" → skip RAM guard, use cpu_bound
        assert_eq!(derive_num_workers_inner(4, 0, None), 4);
    }

    #[test]
    fn worker_count_8gb_machine_with_2gb_available() {
        // Simulates an 8 GB machine (like the dev machine) with ~2.2 GB available.
        // With the old hw.memsize approach: 8192 / 500 = 16 → capped at 8 → OOM.
        // With the new vm_stat approach: 2259 / 500 = 4 → safe.
        assert_eq!(derive_num_workers_inner(8, 2_259, None), 4);
    }
}
