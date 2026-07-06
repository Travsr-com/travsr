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
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use crate::governance::{self, EmbedOverrides};
use crate::stderr_ring::StderrRing;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Hard ceiling on parallel reader threads inside the sidecar.
/// Override with TRAVSR_EMBED_WORKERS env var.
pub const MAX_EMBED_WORKERS: usize = 8;

/// FT-H2: no-progress watchdog window.
///
/// Rather than a fixed wall-clock ceiling (which is fatal for large repos —
/// a 2 M-node monorepo legitimately takes >30 min), we watch the embed.db row
/// count and only kill the sidecar when it has made *zero* progress for this
/// many seconds. A healthy sidecar that keeps writing embeddings is never
/// killed, regardless of total runtime.
const NO_PROGRESS_SECS: u64 = 600;

/// Optional hard wall-clock ceiling. Unset by default (the no-progress
/// watchdog is the liveness check). Set TRAVSR_REINDEX_TIMEOUT_SECS to impose
/// an absolute upper bound on a single reindex pass regardless of progress.
fn reindex_hard_ceiling_secs() -> Option<u64> {
    std::env::var("TRAVSR_REINDEX_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&x: &u64| x > 0)
}

/// Count embeddings written for `model_id` in the sidecar's embed.db.
///
/// Used by the no-progress watchdog to detect a stalled sidecar. Opens
/// read-only + no-mutex so it never blocks the writer (embed.db is WAL,
/// synchronous=OFF). Returns None on any error (missing db, locked) so the
/// caller treats an unreadable count as "no observation this tick" rather than
/// a stall.
fn count_model_embeddings(embed_db_path: &Path, model_id: &str) -> Option<u64> {
    let conn = rusqlite::Connection::open_with_flags(
        embed_db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    conn.query_row(
        "SELECT COUNT(*) FROM node_embeddings WHERE model_id = ?1",
        [model_id],
        |r| r.get::<_, i64>(0),
    )
    .ok()
    .map(|n| n as u64)
}

/// FT-H1: process-level single-flight guard.
/// Prevents Phase1/Phase2/post-commit from launching ≥2 reindex sidecars that
/// write to the same embed.db concurrently (no temp-db isolation in RFC-021).
static REINDEX_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// RAII reset for the process-level single-flight guard (FT-M4).
///
/// Holding one of these keeps REINDEX_IN_FLIGHT = true; dropping it (on normal
/// return, `?`, an early `return`, OR a panic unwinding out of the reindex
/// closure) resets the flag. This replaces the hand-written tail `store(false)`
/// which a panic in `run_parallel_reindex` would skip, permanently wedging all
/// future reindex passes.
struct ReindexInFlightGuard;

impl ReindexInFlightGuard {
    /// Claim the single-flight slot. `None` if a reindex is already running.
    fn try_acquire() -> Option<Self> {
        REINDEX_IN_FLIGHT
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| ReindexInFlightGuard)
    }
}

impl Drop for ReindexInFlightGuard {
    fn drop(&mut self) {
        REINDEX_IN_FLIGHT.store(false, Ordering::SeqCst);
    }
}

/// OS PID of the in-flight reindex sidecar, or 0 when none is running (L8).
/// Set immediately after spawn; cleared (RAII) on any exit path of
/// `run_parallel_reindex`. Read by `terminate_inflight_reindex()` at daemon
/// shutdown to SIGTERM an otherwise-orphaned sidecar.
static REINDEX_CHILD_PID: AtomicU32 = AtomicU32::new(0);

/// RAII: clears REINDEX_CHILD_PID on drop so a panic/`?`/return inside the
/// reindex loop never leaves a stale PID that a later shutdown would kill by
/// mistake (PID reuse).
struct ReindexPidGuard;

impl Drop for ReindexPidGuard {
    fn drop(&mut self) {
        REINDEX_CHILD_PID.store(0, Ordering::SeqCst);
    }
}

/// Best-effort terminate an in-flight reindex sidecar. Called from the daemon
/// shutdown path (L8). No-op when no reindex is running.
pub fn terminate_inflight_reindex() {
    let pid = REINDEX_CHILD_PID.load(Ordering::SeqCst);
    if pid != 0 {
        tracing::info!(
            pid,
            "terminating in-flight embed reindex sidecar on shutdown"
        );
        crate::watchdog::kill_pid(pid);
        REINDEX_CHILD_PID.store(0, Ordering::SeqCst);
    }
}

/// Memory overhead per reader thread inside the sidecar (batch buffers, tokenizer state).
/// The model weights are loaded ONCE by the single sidecar (RFC-021), so only this
/// per-thread cost scales with worker count.
const PER_READER_THREAD_MB: u64 = 50;

/// Reserved headroom for the OS, daemon, and SQLite page cache.
const OS_RESERVE_MB: u64 = 256;

/// Conservative per-slot estimate used when the model's RAM cost is unknown.
/// Preserves the old behaviour for future or unrecognised model ids.
const FALLBACK_RAM_PER_SLOT_MB: u64 = 500;

/// Fraction of symbol nodes targeted by Phase 1 (eager) embedding.
/// The shell_number threshold is derived so that nodes with
/// shell_number >= threshold cover at most this fraction of total symbol nodes.
/// Smaller repos where this fraction covers nearly all nodes skip the phase split.
const PHASE1_COVERAGE_FRACTION: f64 = 0.25;

// ── Catalog types ─────────────────────────────────────────────────────────────

/// One model file the CLI must download from HuggingFace.
/// Catalog DATA, deserialized from `embed_catalog.toml` — not hardcoded in Rust.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct EmbedModelFile {
    /// Filename placed under `~/.travsr/models/<backend_id>/`.
    pub name: String,
    /// Path component after the HuggingFace base URL, e.g. `"onnx/model_int8.onnx"`.
    pub url_path: String,
    /// HuggingFace repo slug, e.g. `"BAAI/bge-small-en-v1.5"`.
    pub hf_repo: String,
    /// Approximate download size in MiB — shown in `travsr embed init` progress.
    pub size_hint_mb: u32,
}

/// One downloadable embedding backend. Every field is catalog DATA — nothing about a
/// model (dim, pooling, prefix, input count) is hardcoded in Rust logic.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct EmbedBackend {
    pub id: String,
    pub description: String,
    pub dim: u32,
    /// Model parameter count in millions (e.g. 33 for bge-small).
    pub params_m: u32,
    /// MTEB retrieval score (higher = better, max ~68).
    pub mteb: f32,
    /// Approximate peak RAM during reindex in MB.
    pub ram_mb: u32,
    /// Approximate reindex time in seconds for a 3.5k-node repo.
    pub init_secs: u32,
    pub binary_name: String,
    pub github_repo: String,
    pub version_fallback: String,
    /// Pooling mode written into the sidecar descriptor: `"cls"` | `"mean"`.
    pub pooling: String,
    /// Query-only prefix (documents get none). May be empty.
    #[serde(default)]
    pub query_prefix: String,
    /// ONNX input tensor count: 2 (no `token_type_ids`) or 3 (classic BERT).
    pub n_inputs: u32,
    /// Matryoshka truncation: 0 = store the full native `dim`; N > 0 = truncate +
    /// re-normalize each vector to its first N values (must be <= `dim`). Lets one
    /// model ship both a full-fidelity and a smaller-storage catalog entry.
    #[serde(default)]
    pub truncate_dim: u32,
    /// Informational architecture tag (e.g. `"bert"`).
    #[serde(default)]
    pub arch: String,
    pub model_files: Vec<EmbedModelFile>,
}

impl EmbedBackend {
    /// Stored embedding dimension shown to users and used for the HNSW index:
    /// `truncate_dim` when truncating, else native `dim`.
    pub fn output_dim(&self) -> u32 {
        if self.truncate_dim > 0 {
            self.truncate_dim
        } else {
            self.dim
        }
    }
}

/// The merged model catalog: bundled defaults + optional user override, loaded once.
///
/// The bundled defaults live in `embed_catalog.toml` (a data file, compiled in via
/// `include_str!`). A user override at `~/.travsr/embed_catalog.toml` lets a model be
/// added or tweaked WITHOUT recompiling: an override entry whose `id` matches a
/// bundled one replaces it; a new `id` is appended.
///
/// Returned as `&'static [EmbedBackend]` (backed by a `OnceLock`) so existing callers
/// that hold `&'static EmbedBackend` keep working unchanged.
pub fn backends() -> &'static [EmbedBackend] {
    static CATALOG: std::sync::OnceLock<Vec<EmbedBackend>> = std::sync::OnceLock::new();
    CATALOG.get_or_init(load_catalog).as_slice()
}

#[derive(serde::Deserialize)]
struct CatalogFile {
    #[serde(default)]
    backend: Vec<EmbedBackend>,
}

fn load_catalog() -> Vec<EmbedBackend> {
    // Bundled default catalog — data, not Rust literals.
    let bundled: CatalogFile = toml::from_str(include_str!("embed_catalog.toml"))
        .expect("bundled embed_catalog.toml must be valid");
    let mut list = bundled.backend;

    // Optional user override — never fatal.
    if let Some(home) = dirs::home_dir() {
        let path = home.join(".travsr").join("embed_catalog.toml");
        if let Ok(text) = std::fs::read_to_string(&path) {
            match toml::from_str::<CatalogFile>(&text) {
                Ok(user) => {
                    for ub in user.backend {
                        match list.iter_mut().find(|b| b.id == ub.id) {
                            Some(existing) => *existing = ub,
                            None => list.push(ub),
                        }
                    }
                }
                Err(e) => tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "ignoring invalid ~/.travsr/embed_catalog.toml"
                ),
            }
        }
    }
    list
}

/// Look up a backend by its stable id string.
pub fn lookup(id: &str) -> Option<&'static EmbedBackend> {
    backends().iter().find(|b| b.id == id)
}

/// Write the per-model descriptor (`model.toml`) next to the downloaded model files.
///
/// This is the bridge to the sidecar: `travsr embed init` calls it after downloading a
/// model so the sidecar reads dim/pooling/prefix/inputs from config, never hardcoded.
pub fn write_model_descriptor(model_dir: &Path, b: &EmbedBackend) -> anyhow::Result<()> {
    use anyhow::Context as _;
    #[derive(serde::Serialize)]
    struct Descriptor<'a> {
        dim: u32,
        pooling: &'a str,
        query_prefix: &'a str,
        n_inputs: u32,
        truncate_dim: u32,
    }
    let content = toml::to_string(&Descriptor {
        dim: b.dim,
        pooling: &b.pooling,
        query_prefix: &b.query_prefix,
        n_inputs: b.n_inputs,
        truncate_dim: b.truncate_dim,
    })
    .context("serialize model descriptor")?;
    let path = model_dir.join("model.toml");

    // Atomic write: the sidecar reads `model.toml` at startup and hard-errors on a
    // malformed descriptor, so a concurrent spawn (or the `ensure_model_descriptor`
    // migration path) must never observe a torn/partial file. Write to a uniquely
    // named temp in the same directory, then rename into place (atomic on a single
    // filesystem). `NamedTempFile` guarantees a distinct name — parallel writers in
    // the same process can't collide — and drops/cleans up the temp on any error.
    use std::io::Write as _;
    let mut tmp = tempfile::NamedTempFile::new_in(model_dir).with_context(|| {
        format!(
            "creating temp for model descriptor in {}",
            model_dir.display()
        )
    })?;
    tmp.write_all(content.as_bytes())
        .context("writing model descriptor to temp file")?;
    tmp.persist(&path)
        .map_err(|e| anyhow::Error::new(e.error))
        .with_context(|| format!("persisting model descriptor to {}", path.display()))?;
    Ok(())
}

/// Self-heal a pre-descriptor install: write `model.toml` iff the model is
/// installed (`model.onnx` present) but the descriptor is absent.
///
/// Pre-4da54c9 installs (e.g. `bge-small-en-v1.5` from before the descriptor
/// requirement existed) have model weights but no `model.toml`; the sidecar
/// (>= Jul 2026) then exits code 1 on every spawn. Calling this on the spawn
/// path migrates them transparently without a re-`travsr embed init`.
///
/// Never fatal: a write failure is logged and the caller proceeds, so the sidecar
/// still surfaces the canonical "run `travsr embed init`" error rather than a
/// silent no-op. No-op when the descriptor already exists or the model is not
/// installed.
fn ensure_model_descriptor(model_dir: &Path, backend: &EmbedBackend) {
    let onnx = model_dir.join("model.onnx");
    let toml = model_dir.join("model.toml");
    if !onnx.exists() || toml.exists() {
        return;
    }
    match write_model_descriptor(model_dir, backend) {
        Ok(()) => tracing::info!(
            model = %backend.id,
            path = %toml.display(),
            "embed: migrated pre-descriptor install — wrote missing model.toml"
        ),
        Err(e) => tracing::warn!(
            model = %backend.id,
            error = %e,
            "embed: could not write missing model.toml (spawn will surface the descriptor error)"
        ),
    }
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

/// Best-effort performance-core count — excludes efficiency cores and hyperthreads
/// so the inference loop isn't time-sliced onto slower silicon.
///
/// macOS (Apple Silicon):  `sysctl hw.perflevel0.logicalcpu`
///                         Falls back to `available_parallelism()` on Intel Macs
///                         (no perf-level split → sysctl returns an error).
///
/// Linux (hybrid CPUs):    reads `/sys/devices/system/cpu/cpu{N}/cpufreq/cpuinfo_max_freq`
///                         and counts cores at the highest frequency group (P-cores).
///                         Falls back to `available_parallelism()` when sysfs is absent
///                         (containers, VMs, OCI A1 Ampere Altra — homogeneous anyway).
///
/// Windows:                uses `GetLogicalProcessorInformationEx(RelationProcessorCore)`
///                         and counts cores whose `EfficiencyClass` equals the maximum
///                         observed value (= P-cores on hybrid, all cores on homogeneous).
///                         Falls back to `available_parallelism()` on API failure.
fn p_core_count() -> usize {
    let fallback = || {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    };

    #[cfg(target_os = "macos")]
    {
        if let Ok(out) = Command::new("sysctl")
            .args(["-n", "hw.perflevel0.logicalcpu"])
            .output()
        {
            if let Ok(n) = String::from_utf8_lossy(&out.stdout).trim().parse::<usize>() {
                if n > 0 {
                    return n;
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        if let Some(n) = linux_p_core_count() {
            return n;
        }
    }

    #[cfg(windows)]
    {
        if let Some(n) = windows_p_core_count() {
            return n;
        }
    }

    fallback()
}

/// Parse `/sys/devices/system/cpu/cpu{N}/cpufreq/cpuinfo_max_freq` entries and
/// return the count of CPUs at the highest observed frequency (= P-cores).
/// Returns `None` when sysfs is absent or no frequency data is readable.
#[cfg(target_os = "linux")]
fn linux_p_core_count() -> Option<usize> {
    let cpu_dir = std::path::Path::new("/sys/devices/system/cpu");
    let entries = std::fs::read_dir(cpu_dir).ok()?;

    let mut max_freq: u64 = 0;
    // (freq_khz, count) pairs — avoids HashMap allocation on hot path.
    let mut freq_slots: Vec<(u64, usize)> = Vec::new();

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Match "cpu0", "cpu1", … — skip "cpufreq", "cpuidle", "cpuN/…" etc.
        if !name.starts_with("cpu") {
            continue;
        }
        if name[3..].parse::<u32>().is_err() {
            continue;
        }
        let freq_path = entry.path().join("cpufreq/cpuinfo_max_freq");
        if let Ok(content) = std::fs::read_to_string(&freq_path) {
            if let Ok(freq) = content.trim().parse::<u64>() {
                if freq > max_freq {
                    max_freq = freq;
                }
                match freq_slots.iter_mut().find(|(f, _)| *f == freq) {
                    Some((_, c)) => *c += 1,
                    None => freq_slots.push((freq, 1)),
                }
            }
        }
    }

    if max_freq == 0 {
        return None;
    }
    let p_count = freq_slots
        .iter()
        .find(|(f, _)| *f == max_freq)
        .map(|(_, c)| *c)
        .unwrap_or(0);
    if p_count > 0 {
        Some(p_count)
    } else {
        None
    }
}

/// Walk the `GetLogicalProcessorInformationEx(RelationProcessorCore)` buffer and
/// count processor cores whose `EfficiencyClass` equals the maximum observed value.
/// On homogeneous systems every core has the same class → returns total physical cores.
/// On Intel hybrid (12th gen+) P-cores have a higher class than E-cores.
#[cfg(windows)]
#[allow(unsafe_code)]
fn windows_p_core_count() -> Option<usize> {
    use windows_sys::Win32::System::SystemInformation::{
        GetLogicalProcessorInformationEx, RelationProcessorCore,
        SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
    };

    // First call: discover required buffer size.
    let mut buf_size: u32 = 0;
    // SAFETY: passing null buffer is the documented way to query size.
    unsafe {
        GetLogicalProcessorInformationEx(
            RelationProcessorCore,
            std::ptr::null_mut(),
            &mut buf_size,
        );
    }
    if buf_size == 0 {
        return None;
    }

    let mut buf: Vec<u8> = vec![0u8; buf_size as usize];
    // SAFETY: buf is correctly sized from the previous call.
    let ok = unsafe {
        GetLogicalProcessorInformationEx(
            RelationProcessorCore,
            buf.as_mut_ptr() as *mut SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
            &mut buf_size,
        )
    };
    if ok == 0 {
        return None;
    }

    // Walk the variable-length buffer.  Each entry starts with a `Size` field
    // that gives its own byte length (entries are not fixed-size).
    let mut offset = 0usize;
    let mut max_class: u8 = 0;
    let mut class_counts: Vec<(u8, usize)> = Vec::new();

    while offset + std::mem::size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>()
        <= buf_size as usize
    {
        // SAFETY: we bounds-check before casting.
        let entry = unsafe {
            &*(buf.as_ptr().add(offset) as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX)
        };
        let entry_size = entry.Size as usize;
        if entry_size == 0 || offset + entry_size > buf_size as usize {
            break;
        }
        // SAFETY: Relationship == RelationProcessorCore, so the union field is Processor.
        let efficiency_class = unsafe { entry.Anonymous.Processor.EfficiencyClass };
        if efficiency_class > max_class {
            max_class = efficiency_class;
        }
        match class_counts
            .iter_mut()
            .find(|(c, _)| *c == efficiency_class)
        {
            Some((_, n)) => *n += 1,
            None => class_counts.push((efficiency_class, 1)),
        }
        offset += entry_size;
    }

    let p_count = class_counts
        .iter()
        .find(|(c, _)| *c == max_class)
        .map(|(_, n)| *n)
        .unwrap_or(0);
    if p_count > 0 {
        Some(p_count)
    } else {
        None
    }
}

/// Derive the number of parallel reader threads for the sidecar (C-01), after
/// applying the WS2 governance knobs.
///
/// `max_workers` is the resolved absolute cap (`-j` / `TRAVSR_EMBED_WORKERS`): when
/// set it wins outright and `capacity` is ignored. Otherwise the count is derived
/// from P-cores + the RAM guard and then scaled by `capacity` (a `[0.01, 1.0]`
/// fraction). `model_ram_mb` is the fixed RAM cost of loading the model once (0 =
/// unknown → conservative slot estimate).
fn derive_num_workers(model_ram_mb: u64, max_workers: Option<usize>, capacity: f32) -> usize {
    derive_num_workers_inner(
        p_core_count(),
        available_memory_mb(),
        model_ram_mb,
        max_workers,
        capacity,
    )
}

/// Pure inner function — takes all inputs explicitly so tests never touch env/syscalls.
///
/// RAM formula (RFC-021 single-sidecar):
///   headroom = available_ram − model_fixed_ram − OS_RESERVE_MB
///   derived  = min(p_cores, MAX_EMBED_WORKERS, headroom / PER_READER_THREAD_MB)
///   workers  = max(1, floor(derived × capacity))
///
/// `max_workers` (absolute `-j` / env override) short-circuits the whole formula
/// and, per the epic, wins over `capacity`. When `model_ram_mb == 0` (unknown
/// model) the RAM guard falls back to `available / FALLBACK_RAM_PER_SLOT_MB`.
fn derive_num_workers_inner(
    p_cores: usize,
    available_ram_mb: u64,
    model_ram_mb: u64,
    max_workers: Option<usize>,
    capacity: f32,
) -> usize {
    // Absolute cap wins outright; the capacity governor is ignored (INV: -j > capacity).
    if let Some(n) = max_workers {
        return n.clamp(1, MAX_EMBED_WORKERS);
    }
    let cpu_bound = p_cores.min(MAX_EMBED_WORKERS);
    let derived = if available_ram_mb > 0 {
        let ram_bound = if model_ram_mb > 0 {
            let headroom = available_ram_mb.saturating_sub(model_ram_mb + OS_RESERVE_MB);
            ((headroom / PER_READER_THREAD_MB) as usize).max(1)
        } else {
            ((available_ram_mb / FALLBACK_RAM_PER_SLOT_MB) as usize).max(1)
        };
        cpu_bound.min(ram_bound)
    } else {
        cpu_bound
    };
    // Governor only reduces: floor(derived × capacity), never below 1 worker.
    ((derived as f32 * capacity.clamp(0.01, 1.0)).floor() as usize).max(1)
}

/// Best-effort AVAILABLE RAM in MiB (not total physical RAM).
///
/// macOS:   parses `vm_stat` for free + inactive pages × page_size.
/// Linux:   reads `MemAvailable` from `/proc/meminfo`.
/// Windows: calls `GlobalMemoryStatusEx` → `ullAvailPhys`.
/// Returns 0 when unavailable — caller skips the RAM guard.
#[allow(unsafe_code)]
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
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut mem = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            dwMemoryLoad: 0,
            ullTotalPhys: 0,
            ullAvailPhys: 0,
            ullTotalPageFile: 0,
            ullAvailPageFile: 0,
            ullTotalVirtual: 0,
            ullAvailVirtual: 0,
            ullAvailExtendedVirtual: 0,
        };
        // SAFETY: mem is zero-initialised with dwLength correctly set.
        if unsafe { GlobalMemoryStatusEx(&mut mem) } != 0 {
            return mem.ullAvailPhys / (1024 * 1024);
        }
    }
    0
}

// ── Per-repo threshold derivation ────────────────────────────────────────────

/// Public re-export for `travsr embed reindex` so it can show the resolved worker count.
///
/// Looks up the per-repo model id from `<db_path>/../embed.toml` so the RAM guard
/// uses the correct model's fixed RAM cost. Pass any `.travsr/graph.db` path.
/// Falls back to the conservative slot estimate when the model is unconfigured.
pub fn derive_num_workers_for_cli(db_path: &Path, overrides: &EmbedOverrides) -> usize {
    let model_ram_mb = db_path
        .parent()
        .and_then(|p| p.parent())
        .and_then(repo_backend_id)
        .and_then(|id| lookup(&id))
        .map(|b| b.ram_mb as u64)
        .unwrap_or(0);
    let gov = resolve_governance_for_db(db_path, overrides);
    derive_num_workers(model_ram_mb, gov.max_workers.value, gov.capacity_fraction())
}

/// Resolve the effective embed governance for a repo's `graph.db`, combining the
/// layered config (per-repo + global + env) with one-shot CLI `overrides`.
/// Exposed so the CLI can report the active capacity/priority and their source.
pub fn resolve_governance_for_db(
    db_path: &Path,
    overrides: &EmbedOverrides,
) -> governance::EmbedGovernance {
    let repo_root = db_path.parent().and_then(|p| p.parent());
    governance::resolve_embed(repo_root, overrides)
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
    model_id: &str,
    phase: PhaseFilter,
    quiet: bool,
    overrides: &EmbedOverrides,
) -> anyhow::Result<()> {
    let model_ram_mb = lookup(model_id).map(|b| b.ram_mb as u64).unwrap_or(0);

    // WS2: resolve capacity / max_workers / priority from CLI overrides + the
    // layered config (per-repo + global + env). Governance changes speed only.
    let repo_root = db_path.parent().and_then(|p| p.parent());
    let gov = governance::resolve_embed(repo_root, overrides);
    let n = derive_num_workers(model_ram_mb, gov.max_workers.value, gov.capacity_fraction());
    let priority = gov.priority.value;

    let mut cmd = priority.reindex_command(bin_path);
    cmd.arg("--model-id")
        .arg(model_id)
        .arg("--reindex")
        .arg(db_path)
        .arg("--embed-db")
        .arg(embed_db_path)
        .arg("--parallel")
        .arg(n.to_string())
        .stdin(Stdio::null())
        .stderr(Stdio::piped()); // FT-M2: capture for error surfacing
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
        capacity_pct = gov.capacity.value,
        capacity_src = gov.capacity.source.label(),
        max_workers = ?gov.max_workers.value,
        priority = priority.as_str(),
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
    // L8: register PID so the daemon shutdown path can SIGTERM this sidecar
    // instead of orphaning it. Cleared by ReindexPidGuard on every exit path.
    REINDEX_CHILD_PID.store(child.id(), Ordering::SeqCst);
    let _pid_guard = ReindexPidGuard; // clears PID on return, `?`, or panic

    // FT-M2: capture sidecar stderr so model-load errors are surfaced on failure.
    let stderr_ring = if let Some(stderr) = child.stderr.take() {
        StderrRing::spawn(stderr)
    } else {
        StderrRing::spawn_empty()
    };

    // No-progress watchdog: only kill the sidecar when the embed.db row count
    // for this model has not grown for NO_PROGRESS_SECS. A healthy sidecar that
    // keeps writing is never killed, so large repos are no longer capped by a
    // fixed wall-clock limit. An optional env-set hard ceiling still applies.
    let start = std::time::Instant::now();
    let hard_ceiling = reindex_hard_ceiling_secs();
    let mut last_count = count_model_embeddings(embed_db_path, model_id).unwrap_or(0);
    let mut last_progress = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(s)) if s.success() => {
                tracing::info!(n, "embed: reindex completed");
                return Ok(());
            }
            Ok(Some(s)) => {
                let tail = stderr_ring.tail();
                tracing::warn!(exit = ?s.code(), stderr = %tail, "embed: sidecar exited with failure");
                anyhow::bail!("embed sidecar exited with code {:?}: {tail}", s.code());
            }
            Ok(None) => {
                let now = std::time::Instant::now();

                // Observe progress. An unreadable count (None) is treated as "no
                // observation" — it neither resets nor trips the watchdog.
                if let Some(cur) = count_model_embeddings(embed_db_path, model_id) {
                    if cur > last_count {
                        last_count = cur;
                        last_progress = now;
                    }
                }

                let stalled = now.duration_since(last_progress).as_secs();
                if stalled >= NO_PROGRESS_SECS {
                    let tail = stderr_ring.tail();
                    tracing::warn!(
                        stalled_secs = stalled,
                        embedded = last_count,
                        stderr = %tail,
                        "embed: reindex sidecar made no progress — killing"
                    );
                    let _ = child.kill();
                    let _ = child.wait();
                    anyhow::bail!(
                        "embed sidecar stalled: no new embeddings for {stalled}s \
                         (stuck at {last_count}): {tail}"
                    );
                }

                if let Some(ceiling) = hard_ceiling {
                    let elapsed = now.duration_since(start).as_secs();
                    if elapsed >= ceiling {
                        let tail = stderr_ring.tail();
                        tracing::warn!(
                            ceiling_secs = ceiling,
                            embedded = last_count,
                            stderr = %tail,
                            "embed: reindex sidecar hit hard wall-clock ceiling — killing"
                        );
                        let _ = child.kill();
                        let _ = child.wait();
                        anyhow::bail!(
                            "embed sidecar reindex exceeded hard ceiling of {ceiling}s: {tail}"
                        );
                    }
                }

                std::thread::sleep(Duration::from_millis(500));
            }
            Err(e) => {
                anyhow::bail!("embed sidecar wait error: {e}");
            }
        }
    }
}

/// Returns true when a background reindex sidecar is currently running.
///
/// Used by the daemon's embed_tick to skip Phase 1 catch-up when a reindex
/// is already in progress (avoids double-spawning after a daemon restart).
pub fn embed_reindex_in_flight() -> bool {
    REINDEX_IN_FLIGHT.load(Ordering::SeqCst)
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
    let Some(guard) = ReindexInFlightGuard::try_acquire() else {
        tracing::debug!("embed Phase 1: reindex already in-flight — skipping");
        return false;
    };
    let Some((bin_path, embed_db_path, model_id)) = resolve_backend(db_path) else {
        return false; // guard drops here → flag reset
    };
    let Some(threshold) = derive_phase1_threshold(db_path, PHASE1_COVERAGE_FRACTION) else {
        tracing::warn!(
            db = %db_path.display(),
            "embed Phase 1: k-core data not ready — skipping (will retry after next Phase B)"
        );
        return false; // guard drops here → flag reset
    };
    tracing::info!(threshold, "embed Phase 1: derived shell_number threshold");
    let db_path = db_path.to_path_buf();
    std::thread::Builder::new()
        .name("embed-reindex-phase1".into())
        .spawn(move || {
            let _guard = guard; // moved in; drops on return OR panic → flag reset
            if let Err(e) = run_parallel_reindex(
                &bin_path,
                &db_path,
                &embed_db_path,
                &model_id,
                PhaseFilter::Phase1 { threshold },
                false,
                &EmbedOverrides::default(),
            ) {
                tracing::warn!("embed Phase 1 failed: {e:#}");
            }
        })
        .is_ok()
}

/// Spawn reindex for Phase 2 (shell_number < derived threshold) as a detached
/// background thread. Returns true if the orchestrator thread was launched.
///
/// Uses the same threshold derivation as Phase 1 so the two phases are
/// complementary and together cover all symbol nodes.
pub fn spawn_background_reindex_phase2(db_path: &Path) -> bool {
    let Some(guard) = ReindexInFlightGuard::try_acquire() else {
        tracing::debug!("embed Phase 2: reindex already in-flight — skipping");
        return false;
    };
    let Some((bin_path, embed_db_path, model_id)) = resolve_backend(db_path) else {
        return false; // guard drops here → flag reset
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
            let _guard = guard; // moved in; drops on return OR panic → flag reset
            if let Err(e) = run_parallel_reindex(
                &bin_path,
                &db_path,
                &embed_db_path,
                &model_id,
                phase,
                false,
                &EmbedOverrides::default(),
            ) {
                tracing::warn!("embed Phase 2 failed: {e:#}");
            }
        })
        .is_ok()
}

/// Spawn reindex for all pending nodes (used after Phase B completes)
/// as a detached background thread. Returns true if launched.
pub fn spawn_background_reindex_all(db_path: &Path) -> bool {
    let Some(guard) = ReindexInFlightGuard::try_acquire() else {
        tracing::debug!("embed reindex-all: reindex already in-flight — skipping");
        return false;
    };
    let Some((bin_path, embed_db_path, model_id)) = resolve_backend(db_path) else {
        return false; // guard drops here → flag reset
    };
    let db_path = db_path.to_path_buf();
    std::thread::Builder::new()
        .name("embed-reindex-all".into())
        .spawn(move || {
            let _guard = guard; // moved in; drops on return OR panic → flag reset
            if let Err(e) = run_parallel_reindex(
                &bin_path,
                &db_path,
                &embed_db_path,
                &model_id,
                PhaseFilter::All,
                false,
                &EmbedOverrides::default(),
            ) {
                tracing::warn!("embed reindex-all failed: {e:#}");
            }
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
    overrides: &EmbedOverrides,
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

    run_parallel_reindex(
        &bin_path,
        db_path,
        &embed_db_path,
        &model_id,
        phase,
        false,
        overrides,
    )
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
        &EmbedOverrides::default(),
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
    let bin_path = home.join(".travsr").join("bin").join(&backend.binary_name);
    if !bin_path.exists() {
        return None;
    }

    // #391 Fix 1A: every sidecar spawn funnels through here, so this is the single
    // chokepoint to self-heal pre-descriptor installs (model.onnx present, model.toml
    // absent) that would otherwise make the sidecar exit code 1 on every spawn.
    let model_dir = home.join(".travsr").join("models").join(&backend_id);
    ensure_model_descriptor(&model_dir, backend);

    let embed_db_path = db_path.with_file_name("embed.db");
    Some((bin_path, embed_db_path, backend_id))
}

/// Probe the repo's active embedding model with `queries` against its freshly-built
/// HNSW and return each query's **top-1 cosine** (the maximum similarity of its
/// nearest neighbour), in input order.
///
/// Used by the CLI to auto-calibrate the model-relative semantic floors (see
/// `travsr-mcp` `seed::Calibration`): the caller measures the model's own
/// query↔passage cosine scale on this corpus, label-free, so *any* model — bundled
/// or user-added — self-calibrates on reindex with no hand-tuned constants.
///
/// Store-free by design (this crate does not depend on `travsr-store`): the caller
/// supplies the query strings and applies the percentile policy. Spawns exactly one
/// sidecar (single model load), warms it, then issues one KNN per query.
///
/// Returns `None` when no backend/binary is configured (calibration then skipped →
/// the reader falls back to the reference-model identity). A query that errors or
/// has no neighbour contributes `0.0`, keeping the output aligned with the input so
/// the caller can split it. Best-effort: never panics; bounded by the sidecar's own
/// per-call watchdog.
pub fn probe_top1_cosines(db_path: &Path, queries: &[String]) -> Option<Vec<f32>> {
    if queries.is_empty() {
        return Some(Vec::new());
    }
    // `db_path` is graph.db; the sidecar derives embed.db from it (see EmbedSidecar).
    let (bin_path, _embed_db, model_id) = resolve_backend(db_path)?;
    let sidecar = crate::embed_sidecar::EmbedSidecar::spawn(&bin_path, db_path, &model_id).ok()?;

    // Warm the model + HNSW once so the first real probe isn't charged cold-start
    // latency inside its watchdog window.
    let _ = sidecar.knn("warmup", 1, &model_id);

    let cosines = queries
        .iter()
        .map(|q| {
            sidecar
                .knn(q, 5, &model_id)
                .ok()
                .and_then(|pairs| {
                    pairs
                        .into_iter()
                        .map(|(_, c)| c)
                        .fold(None, |acc: Option<f32>, c| {
                            Some(acc.map_or(c, |a| a.max(c)))
                        })
                })
                .unwrap_or(0.0)
        })
        .collect();
    Some(cosines)
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

    // ── TC-L8: in-flight PID guard ───────────────────────────────────────────

    #[test]
    fn terminate_inflight_reindex_is_noop_when_idle() {
        // PID is 0 → no panic, no kill attempt.
        REINDEX_CHILD_PID.store(0, Ordering::SeqCst);
        terminate_inflight_reindex(); // must not panic
        assert_eq!(REINDEX_CHILD_PID.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn reindex_pid_guard_clears_pid() {
        REINDEX_CHILD_PID.store(12345, Ordering::SeqCst);
        let guard = ReindexPidGuard;
        assert_eq!(REINDEX_CHILD_PID.load(Ordering::SeqCst), 12345);
        drop(guard);
        assert_eq!(REINDEX_CHILD_PID.load(Ordering::SeqCst), 0);
    }

    // ── TC-FT-M4: RAII single-flight guard ───────────────────────────────────

    #[test]
    fn reindex_guard_resets_flag_on_drop() {
        // Acquire then drop; flag must return to false.
        let guard = ReindexInFlightGuard::try_acquire().expect("flag should be free");
        assert!(
            REINDEX_IN_FLIGHT.load(Ordering::SeqCst),
            "flag must be true while held"
        );
        drop(guard);
        assert!(
            !REINDEX_IN_FLIGHT.load(Ordering::SeqCst),
            "flag must reset on drop"
        );
    }

    #[test]
    fn reindex_guard_is_single_flight() {
        let g1 = ReindexInFlightGuard::try_acquire().expect("first acquire");
        assert!(
            ReindexInFlightGuard::try_acquire().is_none(),
            "second acquire must fail while first is held"
        );
        drop(g1);
        let g2 = ReindexInFlightGuard::try_acquire();
        assert!(g2.is_some(), "acquire must succeed after first is dropped");
        drop(g2);
    }

    #[test]
    fn reindex_guard_resets_on_panic() {
        // Regression for FT-M4: a panic inside the reindex closure must not leave
        // REINDEX_IN_FLIGHT permanently set.
        let handle = std::thread::spawn(|| {
            let _guard = ReindexInFlightGuard::try_acquire().expect("should acquire");
            panic!("simulated reindex panic");
        });
        let _ = handle.join(); // expect Err (panicked)
        assert!(
            !REINDEX_IN_FLIGHT.load(Ordering::SeqCst),
            "flag must reset after panic-induced drop"
        );
    }

    #[test]
    fn catalog_ids_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for b in backends() {
            assert!(seen.insert(b.id.as_str()), "duplicate backend id: {}", b.id);
        }
    }

    // ── #391: model.toml descriptor migration ─────────────────────────────────

    fn touch(path: &std::path::Path) {
        std::fs::write(path, b"x").expect("touch test file");
    }

    #[test]
    fn ensure_descriptor_writes_when_onnx_present_and_toml_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = lookup("bge-small-en-v1.5").expect("bge in catalog");
        touch(&dir.path().join("model.onnx"));

        ensure_model_descriptor(dir.path(), b);

        let toml_path = dir.path().join("model.toml");
        assert!(toml_path.exists(), "descriptor must be created");
        // Round-trips to exactly what a fresh `travsr embed init` would write.
        let text = std::fs::read_to_string(&toml_path).unwrap();
        assert!(
            text.contains(&format!("dim = {}", b.dim)),
            "dim mismatch: {text}"
        );
        assert!(
            text.contains(&format!("n_inputs = {}", b.n_inputs)),
            "n_inputs mismatch: {text}"
        );
    }

    #[test]
    fn ensure_descriptor_noop_when_toml_already_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = lookup("bge-small-en-v1.5").expect("bge in catalog");
        touch(&dir.path().join("model.onnx"));
        let toml_path = dir.path().join("model.toml");
        std::fs::write(&toml_path, b"# hand-edited sentinel\n").unwrap();

        ensure_model_descriptor(dir.path(), b);

        // Existing descriptor is never clobbered.
        assert_eq!(
            std::fs::read_to_string(&toml_path).unwrap(),
            "# hand-edited sentinel\n"
        );
    }

    #[test]
    fn ensure_descriptor_noop_when_model_not_installed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = lookup("bge-small-en-v1.5").expect("bge in catalog");
        // No model.onnx → model isn't installed → nothing to migrate.
        ensure_model_descriptor(dir.path(), b);
        assert!(
            !dir.path().join("model.toml").exists(),
            "must not write a descriptor for an uninstalled model"
        );
    }

    #[test]
    fn write_descriptor_is_atomic_and_leaves_no_temp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = lookup("bge-small-en-v1.5").expect("bge in catalog");
        write_model_descriptor(dir.path(), b).expect("write descriptor");

        // The dir must contain exactly `model.toml` — no NamedTempFile leftover of
        // any name survives a successful persist/rename.
        let entries: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            entries,
            vec!["model.toml".to_string()],
            "only model.toml must remain (no temp litter), found: {entries:?}"
        );
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

    // ── TC-01: worker count ───────────────────────────────────────────────────
    //
    // Signature: derive_num_workers_inner(p_cores, available_ram_mb, model_ram_mb,
    //                                     max_workers, capacity)
    // A `1.0` capacity is the full-speed default (governor disabled), so the RAM/CPU
    // cases below match pre-WS2 behaviour. `max_workers = Some(n)` is the absolute
    // override that wins over capacity (was `env_override`).
    //
    // RAM formula (when model_ram_mb > 0):
    //   headroom = available_ram − model_ram − OS_RESERVE_MB(256)
    //   ram_bound = max(1, headroom / PER_READER_THREAD_MB(50))
    //   derived = min(p_cores, MAX_EMBED_WORKERS, ram_bound)
    //   result = max(1, floor(derived × capacity))
    //
    // When model_ram_mb == 0 (unknown):
    //   ram_bound = max(1, available_ram / FALLBACK_RAM_PER_SLOT_MB(500))

    #[test]
    fn worker_count_absolute_override_is_clamped() {
        assert_eq!(
            derive_num_workers_inner(2, 16_000, 200, Some(0), 1.0),
            1,
            "0 should clamp to 1"
        );
        assert_eq!(
            derive_num_workers_inner(16, 64_000, 200, Some(100), 1.0),
            MAX_EMBED_WORKERS,
            "100 should clamp to MAX_EMBED_WORKERS"
        );
        assert_eq!(derive_num_workers_inner(8, 16_000, 200, Some(3), 1.0), 3);
        // Absolute override wins over the capacity governor.
        assert_eq!(
            derive_num_workers_inner(8, 16_000, 200, Some(4), 0.25),
            4,
            "max_workers must ignore capacity"
        );
    }

    #[test]
    fn worker_count_cpu_bound() {
        // 2 P-cores, ample RAM → capped by CPU, not RAM
        // headroom = 16000 - 200 - 256 = 15544 → ram_bound = 310 → min(2, 310) = 2
        assert_eq!(derive_num_workers_inner(2, 16_000, 200, None, 1.0), 2);
        // 12 P-cores → capped at MAX_EMBED_WORKERS
        assert_eq!(
            derive_num_workers_inner(12, 64_000, 200, None, 1.0),
            MAX_EMBED_WORKERS
        );
    }

    #[test]
    fn worker_count_ram_guard_model_aware() {
        // bge-small (200 MB) on a machine with 1 000 MB available:
        //   headroom = 1000 - 200 - 256 = 544 → 544/50 = 10 → min(8, 10) = 8
        // Previously: 1000/500 = 2  ← the original bug
        assert_eq!(derive_num_workers_inner(8, 1_000, 200, None, 1.0), 8);

        // bge-base (450 MB), 1 500 MB available:
        //   headroom = 1500 - 450 - 256 = 794 → 794/50 = 15 → min(8, 15) = 8
        assert_eq!(derive_num_workers_inner(8, 1_500, 450, None, 1.0), 8);

        // bge-large (1 400 MB), 1 500 MB available — very tight:
        //   headroom = 1500 - 1400 - 256 = -156 → saturating_sub = 0 → max(1) = 1
        assert_eq!(derive_num_workers_inner(8, 1_500, 1_400, None, 1.0), 1);

        // bge-large (1 400 MB), 2 000 MB available:
        //   headroom = 2000 - 1400 - 256 = 344 → 344/50 = 6 → min(8, 6) = 6
        assert_eq!(derive_num_workers_inner(8, 2_000, 1_400, None, 1.0), 6);

        // ram_mb = 0 means "unavailable" → skip RAM guard entirely, use p_cores
        assert_eq!(derive_num_workers_inner(4, 0, 200, None, 1.0), 4);
    }

    #[test]
    fn worker_count_unknown_model_uses_fallback() {
        // model_ram_mb = 0 → fallback: available / FALLBACK_RAM_PER_SLOT_MB(500)
        // 8 cores, 2 000 MB → 2000/500 = 4
        assert_eq!(derive_num_workers_inner(8, 2_000, 0, None, 1.0), 4);
        // 8 cores, 300 MB → max(1, 300/500) = 1
        assert_eq!(derive_num_workers_inner(8, 300, 0, None, 1.0), 1);
    }

    #[test]
    fn worker_count_4p_cores_1gb_bge_small() {
        // Reproduces the original reported scenario:
        //   4 P-cores, ~1 000 MB available, bge-small (200 MB)
        //   headroom = 1000 - 200 - 256 = 544 → 544/50 = 10 → min(4, 10) = 4
        // Previously: 1000/500 = 2
        assert_eq!(derive_num_workers_inner(4, 1_000, 200, None, 1.0), 4);
    }

    #[test]
    fn worker_count_capacity_governor_scales_derived() {
        // 8 P-cores, ample RAM → derived = 8. Capacity scales it, floor, min 1.
        assert_eq!(derive_num_workers_inner(8, 64_000, 200, None, 1.0), 8);
        assert_eq!(
            derive_num_workers_inner(8, 64_000, 200, None, 0.5),
            4,
            "50%"
        );
        assert_eq!(
            derive_num_workers_inner(8, 64_000, 200, None, 0.25),
            2,
            "25%"
        );
        // floor(8 × 0.10) = 0 → clamped up to the 1-worker minimum.
        assert_eq!(
            derive_num_workers_inner(8, 64_000, 200, None, 0.10),
            1,
            "min 1"
        );
        // Reduce-only: a 4-core box at 100% is 4, never more.
        assert_eq!(derive_num_workers_inner(4, 64_000, 200, None, 1.0), 4);
        // Capacity applies after the RAM guard: derived = min(8, ram=6) = 6 → ×0.5 = 3.
        assert_eq!(derive_num_workers_inner(8, 2_000, 1_400, None, 0.5), 3);
    }
}
