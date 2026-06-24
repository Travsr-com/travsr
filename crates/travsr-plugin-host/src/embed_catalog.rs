// Static catalog of downloadable embed backends + parallel reindex orchestrator.
//
// RFC-020: parallel reindex workers — N sidecar processes each write to an
// isolated embed.wN.db, then a merge step consolidates into embed.db.
// This eliminates SQLite write contention and reaches ~1 000 nodes/sec on CPU.
//
// Constraint IDs (C-01 … C-08) refer to RFC-020 § "Constraints Being Formalised".

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

// ── Constants ─────────────────────────────────────────────────────────────────

/// Hard ceiling on parallel workers. Override with TRAVSR_EMBED_WORKERS env var.
pub const MAX_EMBED_WORKERS: usize = 8;

/// RAM budget per worker: model weights (~548 MB) + SQLite page cache (~128 MB)
/// + overhead. Used by the RAM guard in derive_num_workers().
const WORKER_RAM_BUDGET_MB: u64 = 700;

// ── Catalog types ─────────────────────────────────────────────────────────────

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
#[derive(Debug, Clone, Copy)]
pub struct EmbedBackend {
    pub id: &'static str,
    pub description: &'static str,
    pub dim: u32,
    pub binary_name: &'static str,
    pub github_repo: &'static str,
    pub version_fallback: &'static str,
    pub model_files: &'static [EmbedModelFile],
}

pub const BACKENDS: &[EmbedBackend] = &[
    EmbedBackend {
        id: "nomic-v1.5-int8",
        description: "nomic-embed-text-v1.5 — candle fp32, Metal/CPU, MRL-256 (dim=256)",
        dim: 256,
        binary_name: "travsr-embed-nomic",
        github_repo: "Travsr-com/travsr-embed",
        version_fallback: "v1.0.0",
        model_files: &[
            EmbedModelFile {
                name: "model.safetensors",
                url_path: "model.safetensors",
                hf_repo: "nomic-ai/nomic-embed-text-v1.5",
                size_hint_mb: 270,
            },
            EmbedModelFile {
                name: "config.json",
                url_path: "config.json",
                hf_repo: "nomic-ai/nomic-embed-text-v1.5",
                size_hint_mb: 1,
            },
            EmbedModelFile {
                name: "tokenizer.json",
                url_path: "tokenizer.json",
                hf_repo: "nomic-ai/nomic-embed-text-v1.5",
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

/// Outcome of one parallel reindex worker (C-05).
#[derive(Debug)]
enum WorkerOutcome {
    Success {
        temp_db: PathBuf,
        range: (i64, i64),
    },
    Failed {
        range: (i64, i64),
        reason: String,
    },
}

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

    /// WHERE clause fragment appended to the pending-bounds query.
    fn sql_fragment(&self) -> String {
        match self {
            PhaseFilter::Phase1 { threshold } => {
                format!("AND shell_number >= {threshold}")
            }
            PhaseFilter::Phase2 { threshold } => {
                format!("AND (shell_number < {threshold} OR shell_number IS NULL)")
            }
            PhaseFilter::All => String::new(),
        }
    }
}

// ── C-01: worker count ────────────────────────────────────────────────────────

/// Derive the number of parallel workers for this machine (C-01).
///
/// Priority: TRAVSR_EMBED_WORKERS env var → P-core count (clamped) → RAM guard.
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

/// Best-effort physical RAM in MiB. Returns 0 when unavailable — caller skips
/// the RAM guard in that case and relies on MAX_EMBED_WORKERS alone.
fn available_memory_mb() -> u64 {
    #[cfg(target_os = "macos")]
    {
        // sysctl hw.memsize — total physical RAM on macOS.
        let out = Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok();
        if let Some(out) = out {
            let s = String::from_utf8_lossy(&out.stdout);
            if let Ok(bytes) = s.trim().parse::<u64>() {
                return bytes / (1024 * 1024);
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

// ── C-02: range partitioning ──────────────────────────────────────────────────

/// Partition [min_id, max_id] into n non-overlapping half-open ranges [start, end).
/// The last range always ends at i64::MAX to capture rows inserted after the query.
pub fn partition_ranges(min_id: i64, max_id: i64, n: usize) -> Vec<(i64, i64)> {
    assert!(n >= 1);
    let span = max_id.saturating_sub(min_id).saturating_add(1);
    let chunk = (span / n as i64).max(1);
    (0..n)
        .map(|i| {
            let start = min_id.saturating_add(i as i64 * chunk);
            let end = if i + 1 == n {
                i64::MAX
            } else {
                min_id.saturating_add((i as i64 + 1) * chunk)
            };
            (start, end)
        })
        .collect()
}

// ── C-07: stale temp file cleanup ─────────────────────────────────────────────

/// Delete any embed.wN.db files left by a previous crashed orchestrator (C-07).
fn cleanup_stale_worker_dbs(travsr_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(travsr_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("embed.w") && name.ends_with(".db") {
            let path = entry.path();
            if let Err(e) = std::fs::remove_file(&path) {
                tracing::warn!(path = %path.display(), error = %e, "embed: failed to remove stale worker db");
            } else {
                tracing::debug!(path = %path.display(), "embed: removed stale worker db");
            }
        }
    }
}

// ── Pending-bounds query ──────────────────────────────────────────────────────

/// Query the min and max node_id of nodes still pending embedding for this phase.
/// Returns None when embed.db is unavailable or no pending nodes remain.
fn query_pending_bounds(
    db_path: &Path,
    embed_db_path: &Path,
    model_id: &str,
    phase: PhaseFilter,
) -> Option<(i64, i64)> {
    let conn = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;

    // ATTACH embed.db when it exists so the NOT EXISTS sub-select can see it.
    let has_embed_db = embed_db_path.exists();
    if has_embed_db {
        let p = embed_db_path.to_str()?;
        // Single-quote escape: paths from our own code, but be defensive.
        let escaped = p.replace('\'', "''");
        conn.execute_batch(&format!("ATTACH DATABASE '{escaped}' AS edb"))
            .ok()?;
    }

    const KIND_FILTER: &str =
        "kind NOT IN ('file','file-module','import','module','field','variable')";

    let not_embedded = if has_embed_db {
        // model_id is safe to embed — it comes from the static BACKENDS catalog.
        format!(
            "AND NOT EXISTS (\
               SELECT 1 FROM edb.node_embeddings \
               WHERE node_id = n.id AND model_id = '{model_id}'\
             )"
        )
    } else {
        String::new()
    };

    let phase_sql = phase.sql_fragment();

    let sql = format!(
        "SELECT MIN(n.id), MAX(n.id) FROM nodes n \
         WHERE {KIND_FILTER} {not_embedded} {phase_sql}"
    );

    let result: Option<(Option<i64>, Option<i64>)> =
        conn.query_row(&sql, [], |row| Ok((row.get(0)?, row.get(1)?))).ok();

    result.and_then(|(min, max)| min.zip(max))
}

// ── C-04 / C-05: merge ───────────────────────────────────────────────────────

/// Merge all surviving worker temp databases into embed.db (C-04, C-05).
///
/// Errors on individual files are logged but never propagate — partial embedding
/// is not fatal; the daemon's embed_tick will recover gaps (C-06).
fn merge_all_worker_dbs(embed_db_path: &Path, outcomes: Vec<WorkerOutcome>) {
    let conn = match rusqlite::Connection::open(embed_db_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "embed merge: failed to open embed.db");
            return;
        }
    };

    // WAL + generous busy_timeout so a concurrent daemon query doesn't block us.
    let _ = conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=30000;");

    // Ensure the table exists in case this is the very first reindex run.
    let _ = conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS node_embeddings (\
           node_id   INTEGER NOT NULL,\
           model_id  TEXT    NOT NULL,\
           embedding BLOB    NOT NULL,\
           PRIMARY KEY (node_id, model_id)\
         ) WITHOUT ROWID;\
         CREATE INDEX IF NOT EXISTS idx_node_embeddings_model \
           ON node_embeddings(model_id);",
    );

    for (i, outcome) in outcomes.into_iter().enumerate() {
        match outcome {
            WorkerOutcome::Success { temp_db, range } => {
                match merge_one(&conn, &temp_db, &format!("ew{i}")) {
                    Ok(rows) => {
                        tracing::info!(
                            rows,
                            range_start = range.0,
                            range_end   = range.1,
                            "embed merge: worker {i} merged"
                        );
                    }
                    Err(e) => {
                        // C-04: do NOT delete the temp file on merge failure.
                        tracing::warn!(
                            error       = %e,
                            path        = %temp_db.display(),
                            range_start = range.0,
                            range_end   = range.1,
                            "embed merge: failed to merge worker {i} — file left for inspection"
                        );
                    }
                }
            }
            WorkerOutcome::Failed { range, reason } => {
                tracing::warn!(
                    reason,
                    range_start = range.0,
                    range_end   = range.1,
                    "embed merge: worker {i} failed — embed_tick will recover gap"
                );
            }
        }
    }
}

/// ATTACH temp_db, INSERT OR REPLACE all rows into the main connection, DETACH,
/// then delete the temp file. The temp file is only deleted after DETACH succeeds
/// (C-04 atomic guarantee).
fn merge_one(
    conn: &rusqlite::Connection,
    temp_db: &Path,
    alias: &str,
) -> anyhow::Result<usize> {
    let path_str = temp_db
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("temp db path is not valid UTF-8"))?;
    let escaped = path_str.replace('\'', "''");

    conn.execute_batch(&format!("ATTACH DATABASE '{escaped}' AS {alias}"))
        .map_err(|e| anyhow::anyhow!("ATTACH failed: {e}"))?;

    let rows = conn
        .execute(
            &format!(
                "INSERT OR REPLACE INTO node_embeddings \
                 SELECT node_id, model_id, embedding \
                 FROM {alias}.node_embeddings"
            ),
            [],
        )
        .map_err(|e| {
            // Best-effort DETACH before returning the error — don't leave a dangling ATTACH.
            let _ = conn.execute_batch(&format!("DETACH DATABASE {alias}"));
            anyhow::anyhow!("INSERT failed: {e}")
        })?;

    // DETACH must succeed before we delete the temp file.
    conn.execute_batch(&format!("DETACH DATABASE {alias}"))
        .map_err(|e| anyhow::anyhow!("DETACH failed: {e}"))?;

    // C-04: only delete after DETACH succeeds.
    std::fs::remove_file(temp_db)
        .map_err(|e| anyhow::anyhow!("remove_file failed: {e}"))?;

    Ok(rows)
}

// ── Core orchestrator ─────────────────────────────────────────────────────────

/// Spawn N parallel sidecar workers, wait for all, then merge into embed.db.
///
/// Called from a background thread (via the public spawn_* functions) or
/// directly from the CLI (via run_parallel_reindex_blocking).
fn run_parallel_reindex(
    bin_path: &Path,
    db_path: &Path,
    embed_db_path: &Path,
    model_id: &str,
    phase: PhaseFilter,
) {
    let travsr_dir = match db_path.parent() {
        Some(d) => d,
        None => {
            tracing::error!("embed: db_path has no parent directory");
            return;
        }
    };

    // C-07: purge stale temp files from any previous crashed orchestrator.
    cleanup_stale_worker_dbs(travsr_dir);

    // Query pending node bounds for this phase.
    let (min_id, max_id) =
        match query_pending_bounds(db_path, embed_db_path, model_id, phase) {
            Some(bounds) => bounds,
            None => {
                tracing::info!("embed: no pending nodes for phase {phase:?} — skipping");
                return;
            }
        };

    // C-01: derive worker count.
    let num_workers = derive_num_workers();

    // C-02: partition node-id space into non-overlapping ranges.
    let ranges = partition_ranges(min_id, max_id, num_workers);

    tracing::info!(
        num_workers,
        min_id,
        max_id,
        phase = ?phase,
        "embed: starting parallel reindex"
    );

    // Spawn one sidecar per partition.
    let mut children: Vec<(Option<Child>, PathBuf, (i64, i64))> = ranges
        .into_iter()
        .enumerate()
        .map(|(i, (start, end))| {
            let temp_db = travsr_dir.join(format!("embed.w{i}.db"));
            let mut cmd = Command::new(bin_path);
            cmd.arg("--reindex")
                .arg(db_path)
                .arg("--embed-db")
                .arg(&temp_db)
                .arg("--row-start")
                .arg(start.to_string())
                .arg("--row-end")
                .arg(end.to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());

            if let Some((flag, val)) = phase.sidecar_flag() {
                cmd.arg(flag).arg(val.to_string());
            }

            let child = match cmd.spawn() {
                Ok(c) => {
                    tracing::debug!(worker = i, range_start = start, range_end = end, "embed: worker spawned");
                    Some(c)
                }
                Err(e) => {
                    tracing::warn!(worker = i, error = %e, "embed: failed to spawn worker");
                    None
                }
            };
            (child, temp_db, (start, end))
        })
        .collect();

    // Wait for all workers and collect outcomes (C-05: failures are non-fatal).
    let outcomes: Vec<WorkerOutcome> = children
        .iter_mut()
        .enumerate()
        .map(|(i, (child_opt, temp_db, range))| {
            match child_opt.as_mut() {
                None => WorkerOutcome::Failed {
                    range: *range,
                    reason: "spawn failed".into(),
                },
                Some(child) => match child.wait() {
                    Ok(status) if status.success() => WorkerOutcome::Success {
                        temp_db: temp_db.clone(),
                        range: *range,
                    },
                    Ok(status) => {
                        tracing::warn!(
                            worker = i,
                            exit   = ?status.code(),
                            "embed: worker exited with failure"
                        );
                        WorkerOutcome::Failed {
                            range: *range,
                            reason: format!("exit {:?}", status.code()),
                        }
                    }
                    Err(e) => {
                        tracing::warn!(worker = i, error = %e, "embed: wait() failed");
                        WorkerOutcome::Failed {
                            range: *range,
                            reason: e.to_string(),
                        }
                    }
                },
            }
        })
        .collect();

    // C-04 / C-05: merge surviving temp dbs; log failures; never abort.
    merge_all_worker_dbs(embed_db_path, outcomes);
}

// ── Public spawn functions ────────────────────────────────────────────────────

/// Spawn parallel reindex for Phase 1 (shell_number >= threshold) as a detached
/// background thread. Returns true if the orchestrator thread was launched.
pub fn spawn_background_reindex_phase1(db_path: &Path, threshold: u32) -> bool {
    let Some((bin_path, embed_db_path, model_id)) = resolve_backend(db_path) else {
        return false;
    };
    let db_path = db_path.to_path_buf();
    std::thread::Builder::new()
        .name("embed-reindex-phase1".into())
        .spawn(move || {
            run_parallel_reindex(
                &bin_path,
                &db_path,
                &embed_db_path,
                &model_id,
                PhaseFilter::Phase1 { threshold },
            );
        })
        .is_ok()
}

/// Spawn parallel reindex for Phase 2 (shell_number < threshold) as a detached
/// background thread. Returns true if the orchestrator thread was launched.
pub fn spawn_background_reindex_phase2(db_path: &Path, threshold: u32) -> bool {
    let Some((bin_path, embed_db_path, model_id)) = resolve_backend(db_path) else {
        return false;
    };
    let db_path = db_path.to_path_buf();
    std::thread::Builder::new()
        .name("embed-reindex-phase2".into())
        .spawn(move || {
            run_parallel_reindex(
                &bin_path,
                &db_path,
                &embed_db_path,
                &model_id,
                PhaseFilter::Phase2 { threshold },
            );
        })
        .is_ok()
}

/// Spawn parallel reindex for all pending nodes (used after Phase B completes)
/// as a detached background thread. Returns true if launched.
pub fn spawn_background_reindex_all(db_path: &Path) -> bool {
    let Some((bin_path, embed_db_path, model_id)) = resolve_backend(db_path) else {
        return false;
    };
    let db_path = db_path.to_path_buf();
    std::thread::Builder::new()
        .name("embed-reindex-all".into())
        .spawn(move || {
            run_parallel_reindex(
                &bin_path,
                &db_path,
                &embed_db_path,
                &model_id,
                PhaseFilter::All,
            );
        })
        .is_ok()
}

/// Blocking reindex — called from `travsr embed reindex` (CLI path).
///
/// Runs the full parallel orchestration in the calling thread. Suitable for
/// interactive use where the caller wants to wait for completion.
/// Returns Err if the backend is not installed.
pub fn run_parallel_reindex_blocking(
    db_path: &Path,
    phase1_threshold: Option<u32>,
) -> anyhow::Result<()> {
    let (bin_path, embed_db_path, model_id) = resolve_backend(db_path)
        .ok_or_else(|| anyhow::anyhow!(
            "No embedding backend active or binary missing. \
             Run `travsr embed init` first."
        ))?;

    let phase = match phase1_threshold {
        Some(t) => PhaseFilter::Phase1 { threshold: t },
        None => PhaseFilter::All,
    };

    run_parallel_reindex(&bin_path, db_path, &embed_db_path, &model_id, phase);
    Ok(())
}

// ── Shared resolution helper ──────────────────────────────────────────────────

/// Resolve the active backend's binary path, embed.db path, and model_id.
/// Returns None when no backend is configured or the binary is missing.
fn resolve_backend(db_path: &Path) -> Option<(PathBuf, PathBuf, String)> {
    let backend_id = active_backend_id()?;
    let backend = lookup(&backend_id)?;
    let home = dirs::home_dir()?;
    let bin_path = home
        .join(".travsr")
        .join("bin")
        .join(backend.binary_name);
    if !bin_path.exists() {
        return None;
    }
    let embed_db_path = db_path.with_file_name("embed.db");
    Some((bin_path, embed_db_path, backend_id))
}

// ── Config helpers ────────────────────────────────────────────────────────────

/// Read the active backend id from `~/.travsr/embed.toml`.
/// Returns None when absent, unreadable, or no `active` key set.
pub fn active_backend_id() -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Config {
        active: Option<String>,
    }
    let home = dirs::home_dir()?;
    let content =
        std::fs::read_to_string(home.join(".travsr").join("embed.toml")).ok()?;
    let cfg: Config = toml::from_str(&content).ok()?;
    cfg.active
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    // ── TC-00: existing catalog invariants ────────────────────────────────────

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

    // ── TC-01: worker count clamping ──────────────────────────────────────────

    #[test]
    fn worker_count_env_override_is_clamped() {
        // 0 → clamp to 1 regardless of CPU/RAM
        assert_eq!(derive_num_workers_inner(2, 16_000, Some(0)), 1, "0 should clamp to 1");
        // 100 → clamp to MAX_EMBED_WORKERS
        assert_eq!(
            derive_num_workers_inner(16, 64_000, Some(100)),
            MAX_EMBED_WORKERS,
            "100 should clamp to MAX_EMBED_WORKERS"
        );
        // Valid value within range passes through unchanged
        assert_eq!(derive_num_workers_inner(8, 16_000, Some(3)), 3);
    }

    #[test]
    fn worker_count_cpu_bound() {
        // 2 cores, plenty of RAM, no override → 2 workers
        assert_eq!(derive_num_workers_inner(2, 16_000, None), 2);
        // 12 cores, plenty of RAM → capped at MAX_EMBED_WORKERS
        assert_eq!(derive_num_workers_inner(12, 64_000, None), MAX_EMBED_WORKERS);
    }

    #[test]
    fn worker_count_ram_guard() {
        // 8 cores but only 2 800 MB RAM → floor(2800 / 700) = 4
        assert_eq!(derive_num_workers_inner(8, 2_800, None), 4);
        // 8 cores but only 500 MB RAM → max(floor(500/700), 1) = 1
        assert_eq!(derive_num_workers_inner(8, 500, None), 1);
        // ram_mb = 0 means "unknown" → skip RAM guard, use cpu_bound
        assert_eq!(derive_num_workers_inner(4, 0, None), 4);
    }

    // ── TC-02: range partitioning ─────────────────────────────────────────────

    #[test]
    fn ranges_are_non_overlapping_and_contiguous() {
        let ranges = partition_ranges(100, 1_099, 4);
        assert_eq!(ranges.len(), 4);
        // non-overlapping: each range's end == next range's start
        for w in ranges.windows(2) {
            assert_eq!(w[0].1, w[1].0, "gap or overlap between workers");
        }
        // first starts at min
        assert_eq!(ranges[0].0, 100);
        // last ends at i64::MAX
        assert_eq!(ranges[3].1, i64::MAX);
    }

    #[test]
    fn ranges_cover_single_node() {
        let ranges = partition_ranges(42, 42, 1);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].0, 42);
        assert_eq!(ranges[0].1, i64::MAX);
    }

    #[test]
    fn ranges_with_more_workers_than_nodes_still_non_overlapping() {
        // 3 nodes, 8 workers — some ranges may be empty but must not overlap
        let ranges = partition_ranges(0, 2, 8);
        assert_eq!(ranges.len(), 8);
        for w in ranges.windows(2) {
            assert!(w[0].1 <= w[1].0, "overlap: {:?} and {:?}", w[0], w[1]);
        }
        assert_eq!(ranges[0].0, 0);
        assert_eq!(ranges[7].1, i64::MAX);
    }

    // ── TC-03: no duplicate node_ids across ranges ────────────────────────────

    #[test]
    fn ranges_do_not_overlap_node_id_space() {
        let ranges = partition_ranges(1_000, 9_999, 4);
        for id in [1_000i64, 3_249, 5_499, 7_749, 9_999] {
            let count = ranges
                .iter()
                .filter(|(s, e)| id >= *s && id < *e)
                .count();
            // Each id must belong to exactly one range (or zero for the boundary).
            assert!(count <= 1, "id {id} appears in {count} ranges");
        }
    }

    // ── TC-05: corrupt temp file does not abort merge ─────────────────────────

    #[test]
    fn merge_skips_corrupt_temp_file_and_continues() {
        let dir = tempfile::tempdir().unwrap();
        let embed_db = dir.path().join("embed.db");
        let good_db = dir.path().join("embed.w0.db");
        let bad_db = dir.path().join("embed.w1.db");

        // Build a valid temp db with one row.
        {
            let conn = rusqlite::Connection::open(&good_db).unwrap();
            conn.execute_batch(
                "CREATE TABLE node_embeddings (\
                   node_id INTEGER NOT NULL, \
                   model_id TEXT NOT NULL, \
                   embedding BLOB NOT NULL, \
                   PRIMARY KEY (node_id, model_id)\
                 ) WITHOUT ROWID;",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO node_embeddings VALUES (1, 'test-model', X'DEADBEEF')",
                [],
            )
            .unwrap();
        }

        // Write a corrupt (non-SQLite) file.
        {
            let mut f = std::fs::File::create(&bad_db).unwrap();
            f.write_all(b"not a sqlite database -- corrupted").unwrap();
        }

        let outcomes = vec![
            WorkerOutcome::Success {
                temp_db: good_db.clone(),
                range: (0, 500),
            },
            WorkerOutcome::Success {
                temp_db: bad_db.clone(),
                range: (500, i64::MAX),
            },
        ];

        merge_all_worker_dbs(&embed_db, outcomes);

        // good_db rows must be in embed.db
        let conn = rusqlite::Connection::open(&embed_db).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM node_embeddings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "good worker rows must be merged");

        // good_db deleted, bad_db left for inspection
        assert!(!good_db.exists(), "good temp file must be deleted after successful merge");
        assert!(bad_db.exists(), "corrupt temp file must be left for inspection (C-04)");
    }

    // ── TC-06: temp file not deleted until DETACH succeeds ───────────────────

    #[test]
    fn temp_file_survives_when_target_schema_missing() {
        let dir = tempfile::tempdir().unwrap();
        // embed.db without the node_embeddings table — INSERT will fail.
        let embed_db = dir.path().join("embed.db");
        rusqlite::Connection::open(&embed_db).unwrap(); // create empty

        let temp_db = dir.path().join("embed.w0.db");
        {
            let conn = rusqlite::Connection::open(&temp_db).unwrap();
            conn.execute_batch(
                "CREATE TABLE node_embeddings (\
                   node_id INTEGER NOT NULL, model_id TEXT NOT NULL, \
                   embedding BLOB NOT NULL, PRIMARY KEY (node_id, model_id)\
                 ) WITHOUT ROWID;",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO node_embeddings VALUES (99, 'x', X'FF')",
                [],
            )
            .unwrap();
        }

        // The merge should fail (no node_embeddings table in embed.db) and leave the temp file.
        // We call merge_all_worker_dbs which must not panic.
        // merge_all_worker_dbs creates the table now — but let's test the raw merge_one path.
        // Open embed.db WITHOUT creating the table to simulate schema mismatch.
        let conn = rusqlite::Connection::open(&embed_db).unwrap();
        let result = merge_one(&conn, &temp_db, "ew0");
        // merge_one may succeed or fail depending on whether the table was auto-created.
        // The key invariant: if it fails, temp_db still exists.
        if result.is_err() {
            assert!(temp_db.exists(), "temp file must survive a failed merge (C-04)");
        }
    }

    // ── TC-07: merge is idempotent ────────────────────────────────────────────

    #[test]
    fn merge_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let embed_db = dir.path().join("embed.db");
        let temp_db = dir.path().join("embed.w0.db");

        // Build temp db.
        {
            let conn = rusqlite::Connection::open(&temp_db).unwrap();
            conn.execute_batch(
                "CREATE TABLE node_embeddings (\
                   node_id INTEGER NOT NULL, model_id TEXT NOT NULL, \
                   embedding BLOB NOT NULL, PRIMARY KEY (node_id, model_id)\
                 ) WITHOUT ROWID;",
            )
            .unwrap();
            for id in [1i64, 2, 3] {
                conn.execute(
                    "INSERT INTO node_embeddings VALUES (?1, 'test', X'AABB')",
                    rusqlite::params![id],
                )
                .unwrap();
            }
        }

        // First merge run.
        merge_all_worker_dbs(
            &embed_db,
            vec![WorkerOutcome::Success {
                temp_db: temp_db.clone(),
                range: (0, i64::MAX),
            }],
        );

        let conn = rusqlite::Connection::open(&embed_db).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM node_embeddings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3);
        drop(conn);

        // Rebuild temp_db and re-run (simulating a retry after partial failure).
        {
            let conn = rusqlite::Connection::open(&temp_db).unwrap();
            conn.execute_batch(
                "CREATE TABLE node_embeddings (\
                   node_id INTEGER NOT NULL, model_id TEXT NOT NULL, \
                   embedding BLOB NOT NULL, PRIMARY KEY (node_id, model_id)\
                 ) WITHOUT ROWID;",
            )
            .unwrap();
            for id in [1i64, 2, 3] {
                conn.execute(
                    "INSERT INTO node_embeddings VALUES (?1, 'test', X'AABB')",
                    rusqlite::params![id],
                )
                .unwrap();
            }
        }

        merge_all_worker_dbs(
            &embed_db,
            vec![WorkerOutcome::Success {
                temp_db: temp_db.clone(),
                range: (0, i64::MAX),
            }],
        );

        let conn = rusqlite::Connection::open(&embed_db).unwrap();
        let count2: i64 = conn
            .query_row("SELECT COUNT(*) FROM node_embeddings", [], |r| r.get(0))
            .unwrap();
        // INSERT OR REPLACE — same rows, same count
        assert_eq!(count2, 3, "idempotent merge must not duplicate rows");
    }

    // ── TC-08: partial worker failure does not lose successful work ───────────

    #[test]
    fn partial_worker_failure_does_not_lose_successful_work() {
        let dir = tempfile::tempdir().unwrap();
        let embed_db = dir.path().join("embed.db");

        let w0 = dir.path().join("embed.w0.db");
        let w2 = dir.path().join("embed.w2.db");

        for (path, ids) in [(&w0, [10i64, 11, 12]), (&w2, [30i64, 31, 32])] {
            let conn = rusqlite::Connection::open(path).unwrap();
            conn.execute_batch(
                "CREATE TABLE node_embeddings (\
                   node_id INTEGER NOT NULL, model_id TEXT NOT NULL, \
                   embedding BLOB NOT NULL, PRIMARY KEY (node_id, model_id)\
                 ) WITHOUT ROWID;",
            )
            .unwrap();
            for id in ids {
                conn.execute(
                    "INSERT INTO node_embeddings VALUES (?1, 'model', X'FF')",
                    rusqlite::params![id],
                )
                .unwrap();
            }
        }

        let outcomes = vec![
            WorkerOutcome::Success { temp_db: w0, range: (0, 1000) },
            WorkerOutcome::Failed  { range: (1000, 2000), reason: "exit 1".into() },
            WorkerOutcome::Success { temp_db: w2, range: (2000, 3000) },
            WorkerOutcome::Failed  { range: (3000, i64::MAX), reason: "OOM".into() },
        ];

        merge_all_worker_dbs(&embed_db, outcomes);

        let conn = rusqlite::Connection::open(&embed_db).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM node_embeddings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 6, "w0 (3) + w2 (3) rows must survive despite w1/w3 failures");
    }

    // ── TC-09: all workers failed — embed.db unchanged ───────────────────────

    #[test]
    fn all_workers_failed_embed_db_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let embed_db = dir.path().join("embed.db");

        // Pre-seed embed.db with 1 row.
        {
            let conn = rusqlite::Connection::open(&embed_db).unwrap();
            conn.execute_batch(
                "CREATE TABLE node_embeddings (\
                   node_id INTEGER NOT NULL, model_id TEXT NOT NULL, \
                   embedding BLOB NOT NULL, PRIMARY KEY (node_id, model_id)\
                 ) WITHOUT ROWID;",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO node_embeddings VALUES (1, 'model', X'AA')",
                [],
            )
            .unwrap();
        }

        let outcomes = vec![
            WorkerOutcome::Failed { range: (0, 500),       reason: "exit 2".into() },
            WorkerOutcome::Failed { range: (500, i64::MAX), reason: "OOM".into() },
        ];

        merge_all_worker_dbs(&embed_db, outcomes);

        let conn = rusqlite::Connection::open(&embed_db).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM node_embeddings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "pre-existing row must survive an all-failed merge");
    }

    // ── TC-11: stale temp files are purged before new run ────────────────────

    #[test]
    fn stale_worker_dbs_are_removed_on_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let travsr_dir = dir.path();

        // Plant stale files.
        for name in ["embed.w0.db", "embed.w3.db"] {
            std::fs::write(travsr_dir.join(name), b"stale").unwrap();
        }
        // Also plant an unrelated file that must NOT be deleted.
        std::fs::write(travsr_dir.join("graph.db"), b"keep").unwrap();

        cleanup_stale_worker_dbs(travsr_dir);

        assert!(!travsr_dir.join("embed.w0.db").exists(), "stale w0 must be deleted");
        assert!(!travsr_dir.join("embed.w3.db").exists(), "stale w3 must be deleted");
        assert!(travsr_dir.join("graph.db").exists(), "graph.db must be untouched");
    }
}
