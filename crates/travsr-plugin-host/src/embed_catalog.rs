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
use std::sync::Mutex;
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

/// L5: cross-process advisory lock on `<repo>/.travsr/embed.lock`, held for
/// the duration of a reindex sidecar run or an `embed gc` sweep.
///
/// `REINDEX_IN_FLIGHT` above is the cheap in-process fast path that keeps the
/// daemon's own Phase1/Phase2/post-commit spawn sites from racing each other
/// without touching the filesystem — it answers "is *this process* already
/// reindexing". This answers a different question: a CLI `travsr embed
/// reindex` runs in a separate OS process and never sees that flag at all
/// (#376 §18.7 observed both running at once during verification). Both
/// layers stay; removing either leaves a real gap.
///
/// Non-blocking `try_lock`: a second caller must not hang behind a pass that
/// can legitimately run for hours (a large repo's cold embed). `flock` is
/// released by the OS when the holding process dies — even a hard kill — so
/// a crashed holder can never wedge a future acquire. That is the reason this
/// uses `flock` rather than a hand-rolled PID file, which would need its own
/// liveness check and stale-file recovery.
#[derive(Debug)]
pub struct EmbedOpLock {
    file: std::fs::File,
}

impl EmbedOpLock {
    /// Try to claim the lock for `repo_root`, recording `op` (`"reindex"` /
    /// `"gc"`) and this process's PID so a blocked caller can name the
    /// holder. On failure, returns a ready-to-print message naming who holds
    /// it — every caller (CLI error, daemon log) can use it as-is.
    ///
    /// `Ok(None)` (rather than an error) **only** when `.travsr` itself is
    /// missing: that is "not an indexed repo yet", not "locked", and callers
    /// already handle a missing repo separately. Any *other* open failure
    /// (EACCES, read-only mount, `embed.lock` replaced by a directory, EMFILE)
    /// is an error: on an indexed repo `Ok(None)` reaches `run_parallel_reindex`
    /// as "no lock needed" and the pass runs completely unserialized, which is
    /// the exact race L5 exists to prevent.
    ///
    /// The pid/op the holder records lives in a separate, never-locked
    /// `embed.lock.info` file rather than inside `embed.lock` itself. Unlike
    /// POSIX `flock`, Windows' `LockFileEx` is mandatory: once one handle
    /// holds the exclusive lock, a *second* handle's read of that same file
    /// also fails, not just its own lock attempt — so a losing caller could
    /// never read the holder's pid/op back out of the locked file to report
    /// it. Reading an adjacent, always-unlocked file sidesteps that.
    pub fn try_acquire(repo_root: &Path, op: &str) -> anyhow::Result<Option<Self>> {
        use anyhow::Context as _;
        use fs2::FileExt as _;
        let dir = repo_root.join(".travsr");
        let lock_path = dir.join("embed.lock");
        let info_path = dir.join("embed.lock.info");
        let file = match std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
        {
            Ok(f) => f,
            // No `.travsr` at all → not an indexed repo. Fail *open* only here.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !dir.exists() => {
                return Ok(None);
            }
            Err(e) => {
                return Err(anyhow::Error::new(e).context(format!(
                    "opening the embed lock at {} \u{2014} refusing to run an embed operation \
                     without it",
                    lock_path.display()
                )));
            }
        };
        if file.try_lock_exclusive().is_err() {
            let held = std::fs::read_to_string(&info_path).unwrap_or_default();
            let (pid, held_op) = held
                .trim()
                .split_once('\t')
                .map(|(p, o)| (p.to_string(), o.to_string()))
                .unwrap_or_else(|| ("unknown".to_string(), "an embed operation".to_string()));
            return Err(anyhow::Error::new(Contended(format!(
                "Another embed operation is already running for this repo \
                 ({held_op}, pid {pid}).\n\
                 Wait for it to finish, or stop it with: travsr daemon stop"
            ))));
        }
        std::fs::write(&info_path, format!("{}\t{op}", std::process::id()))
            .context("writing embed.lock.info")?;
        Ok(Some(EmbedOpLock { file }))
    }

    /// Like [`Self::try_acquire`], but absorbs a *brief* overlap before
    /// giving up, instead of failing on the first collision.
    ///
    /// Every reindex spawn (this repo's daemon auto-triggers included) funnels
    /// through this lock, so a repo with a running daemon has a real case a
    /// bare `try_acquire` gets wrong: `embed switch`/`embed init` writes the
    /// repo's embed config, and the daemon's own next tick notices it and
    /// fires its own Phase 1 catch-up — racing the explicit command's own
    /// foreground reindex (#376 §18.7 observed exactly this). Before L5 that
    /// race was silent (both sides eventually finished, wastefully). L5 makes
    /// the loser fail loudly, which is worse for this case specifically: the
    /// user's own explicit, foreground command now errors out on what is
    /// normally a brief overlap.
    ///
    /// Retrying for a short, bounded window fixes that without reintroducing
    /// the thing L5 exists to prevent: this still gives up promptly (a few
    /// seconds, not minutes) against a pass that is genuinely still running,
    /// so a real conflict still surfaces the "another operation is running"
    /// message rather than hanging.
    ///
    /// Only *contention* is retried. An unopenable lock file or an unwritable
    /// `embed.lock.info` will not resolve itself by waiting, so those surface
    /// immediately instead of after the full budget.
    pub fn try_acquire_with_retry(repo_root: &Path, op: &str) -> anyhow::Result<Option<Self>> {
        const RETRY_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);
        const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);
        let deadline = std::time::Instant::now() + RETRY_BUDGET;
        loop {
            match Self::try_acquire(repo_root, op) {
                Ok(lock) => return Ok(lock),
                Err(e)
                    if e.downcast_ref::<Contended>().is_some()
                        && std::time::Instant::now() < deadline =>
                {
                    tracing::debug!("embed lock busy, retrying: {e}");
                    std::thread::sleep(POLL_INTERVAL);
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// "Someone else holds the lock" — the one `try_acquire` failure that is worth
/// waiting on. Carries the full user-facing message, so `{e}` is unchanged for
/// every caller that just prints it.
#[derive(Debug)]
struct Contended(String);

impl std::fmt::Display for Contended {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Contended {}

impl Drop for EmbedOpLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
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

/// WS3 (#420): when true, the daemon's `spawn_background_reindex_*` chokepoints
/// refuse to launch a new reindex. Set by `stop-embed`, cleared by `resume-embed`.
/// In-memory for the daemon's lifetime (§4.5) — a restart resumes normal behavior.
static EMBED_PAUSED: AtomicBool = AtomicBool::new(false);

/// WS3: absolute path of the cancel sentinel for the in-flight reindex, set at
/// spawn time in `run_parallel_reindex`. `terminate_inflight_reindex` reads it to
/// signal a graceful drain. `None` when no reindex has ever run this process.
static REINDEX_SENTINEL_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);

/// WS3: grace window the daemon waits for the sidecar to drain and exit after the
/// cancel sentinel is written, before falling back to a hard kill (E1/H4).
const CANCEL_GRACE_SECS: u64 = 10;

/// RAII: clears `REINDEX_SENTINEL_PATH` and removes the sentinel file on every
/// exit path of `run_parallel_reindex`, so a stale path/file never leaks to the
/// next run (E5). Best-effort removal — the sidecar already deletes it on cancel.
struct SentinelGuard;
impl Drop for SentinelGuard {
    fn drop(&mut self) {
        if let Some(p) = REINDEX_SENTINEL_PATH
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Pause the daemon's auto-reindex (WS3 `stop-embed`).
pub fn pause_embed() {
    EMBED_PAUSED.store(true, Ordering::SeqCst);
}

/// Resume the daemon's auto-reindex (WS3 `resume-embed`).
pub fn resume_embed() {
    EMBED_PAUSED.store(false, Ordering::SeqCst);
}

/// True while auto-reindex is paused by `stop-embed`.
pub fn embed_paused() -> bool {
    EMBED_PAUSED.load(Ordering::SeqCst)
}

/// Cancel sentinel path co-located with graph.db: `<repo>/.travsr/embed.cancel`.
/// `db_path` is `<repo>/.travsr/graph.db`, so the sentinel is its sibling. Both
/// the sidecar (which polls it) and the terminate path (which writes it) agree on
/// this convention, so no extra plumbing is needed for the CLI-with-no-daemon case.
pub fn cancel_sentinel_path(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .map(|d| d.join("embed.cancel"))
        .unwrap_or_else(|| PathBuf::from("embed.cancel"))
}

/// Feature-probe whether the installed sidecar understands `--cancel-sentinel`.
/// Runs `<bin> --version` (added in travsr-embed 1.1.0) with a short deadline; an
/// old sidecar prints nothing parseable → we return false and fall back to a hard
/// kill on cancel (F5/H4). Best-effort: any probe failure is treated as "no".
fn sidecar_supports_cancel(bin_path: &Path) -> bool {
    // First release with `--cancel-sentinel`.
    const MIN_MAJOR: u64 = 1;
    const MIN_MINOR: u64 = 1;
    let Ok(out) = Command::new(bin_path)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // Expected form: "travsr-embed <MAJOR>.<MINOR>.<PATCH>".
    let Some(ver) = text.split_whitespace().last() else {
        return false;
    };
    let mut parts = ver.split('.').filter_map(|p| p.parse::<u64>().ok());
    let (Some(major), Some(minor)) = (parts.next(), parts.next()) else {
        return false;
    };
    (major, minor) >= (MIN_MAJOR, MIN_MINOR)
}

/// True if a process with `pid` is currently alive (no signal sent).
#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
/// #500: real liveness probe (OpenProcess + GetExitCodeProcess, no signal).
/// The previous hardcoded `false` made `!pid_alive(pid)` in the grace poll
/// true on its first iteration, so `terminate_inflight_reindex` logged
/// "drained gracefully" immediately and returned — skipping both the grace
/// wait and the `kill_pid` fallback (which the same `false` also made dead
/// code). The sidecar was never terminated on Windows.
#[cfg(target_os = "windows")]
fn pid_alive(pid: u32) -> bool {
    crate::sandbox::windows::pid_alive(pid)
}

/// Conservative stub for platforms with neither probe: report alive, so
/// shutdown waits the full grace window and always runs the kill fallback.
#[cfg(not(any(unix, target_os = "windows")))]
fn pid_alive(_pid: u32) -> bool {
    true
}

/// Gracefully cancel an in-flight reindex sidecar, else best-effort terminate it.
///
/// Called from the daemon shutdown path (L8) and from `stop-embed` (WS3 B4).
/// Writes the cancel sentinel so a cancel-aware sidecar drains its in-flight batch
/// and exits 0 (embedded rows preserved); polls up to [`CANCEL_GRACE_SECS`] for it
/// to exit, then falls back to `kill_pid` (SIGTERM/taskkill) for an old or wedged
/// sidecar. No-op when no reindex is running.
pub fn terminate_inflight_reindex() {
    let pid = REINDEX_CHILD_PID.load(Ordering::SeqCst);
    if pid == 0 {
        return; // idempotent: nothing running (E6)
    }
    let sentinel = REINDEX_SENTINEL_PATH
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();

    if let Some(ref path) = sentinel {
        tracing::info!(pid, sentinel = %path.display(), "embed: writing cancel sentinel — graceful drain");
        // Empty marker file; the sidecar polls for its existence.
        if let Err(e) = std::fs::write(path, b"") {
            tracing::warn!(err = %e, "embed: failed to write cancel sentinel — will hard-kill");
        } else {
            // Grace poll: wait for the sidecar (any spawn surface clears
            // REINDEX_CHILD_PID via ReindexPidGuard on exit) to drain and exit.
            let deadline = std::time::Instant::now() + Duration::from_secs(CANCEL_GRACE_SECS);
            while std::time::Instant::now() < deadline {
                if REINDEX_CHILD_PID.load(Ordering::SeqCst) == 0 || !pid_alive(pid) {
                    tracing::info!(pid, "embed: reindex drained gracefully after cancel");
                    let _ = std::fs::remove_file(path);
                    return;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            tracing::warn!(
                pid,
                grace_secs = CANCEL_GRACE_SECS,
                "embed: sidecar did not drain within grace window — force-killing"
            );
        }
    }

    // Fallback / no sentinel: hard terminate.
    if pid_alive(pid) {
        tracing::info!(pid, "terminating in-flight embed reindex sidecar");
        crate::watchdog::kill_pid(pid);
    }
    REINDEX_CHILD_PID.store(0, Ordering::SeqCst);
    if let Some(path) = sentinel {
        let _ = std::fs::remove_file(path);
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

    /// On-disk file name of the sidecar under `~/.travsr/bin/`: `binary_name`
    /// plus `.exe` on Windows (#12 travsr-embed). Release *asset* names carry
    /// the target triple and are built by the CLI installer, not here.
    pub fn binary_filename(&self) -> String {
        if cfg!(windows) {
            format!("{}.exe", self.binary_name)
        } else {
            self.binary_name.clone()
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
        /// The sidecar's `family` (travsr-embed #6): which engines can execute
        /// this graph at all. Not cosmetic — `tract` runs standard BERT only, so
        /// a ModernBERT or nomic-bert model has to reach an ORT-capable engine,
        /// and the sidecar picks the engine from this tag. Without it the sidecar
        /// assumes `"bert"`, hands a non-BERT graph to tract, and fails at graph
        /// load partway through a reindex — the failure travsr-embed #6 exists to
        /// prevent.
        ///
        /// Skipped when the catalog entry has no `arch`, so the sidecar applies
        /// its own default instead of receiving an empty string it rejects.
        #[serde(skip_serializing_if = "str::is_empty")]
        family: &'a str,
    }
    let content = toml::to_string(&Descriptor {
        dim: b.dim,
        pooling: &b.pooling,
        query_prefix: &b.query_prefix,
        n_inputs: b.n_inputs,
        truncate_dim: b.truncate_dim,
        family: &b.arch,
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
        if let Some(n) = crate::sandbox::windows::windows_p_core_count() {
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
/// Windows: calls `GlobalMemoryStatusEx` → `ullAvailPhys` (via sandbox ffi.rs,
///          where ADR-017 A2 confines all unsafe in this crate).
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
    #[cfg(windows)]
    {
        if let Some(mb) = crate::sandbox::windows::available_physical_memory_mb() {
            return mb;
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

    // L5: cross-process lock, held for this function's whole lifetime (spawn
    // through wait). This is the single point every caller funnels through —
    // the three daemon auto-spawn sites and the CLI's blocking path alike —
    // so one acquisition here covers all of them. `REINDEX_IN_FLIGHT` above
    // already serializes the daemon's own three spawn sites within this
    // process; this additionally covers a concurrent CLI `embed reindex` in a
    // different process, and (via `EmbedOpLock::try_acquire`) `embed gc`.
    let _op_lock = match repo_root {
        Some(root) => EmbedOpLock::try_acquire_with_retry(root, "reindex")?,
        None => None,
    };

    let gov = governance::resolve_embed(repo_root, overrides);
    let n = derive_num_workers(model_ram_mb, gov.max_workers.value, gov.capacity_fraction());
    let priority = gov.priority.value;

    // WS3: set up the graceful-cancel sentinel. Remove any stale sentinel from a
    // prior crashed run so it cannot cancel this one on sight, record the path so
    // `terminate_inflight_reindex` can signal us, and pass it to the sidecar only
    // when it is new enough to understand the flag (else force-kill fallback, H4).
    let sentinel_path = cancel_sentinel_path(db_path);
    let _ = std::fs::remove_file(&sentinel_path);
    *REINDEX_SENTINEL_PATH
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(sentinel_path.clone());
    let _sentinel_guard = SentinelGuard;
    let cancel_supported = sidecar_supports_cancel(bin_path);

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

    if cancel_supported {
        cmd.arg("--cancel-sentinel").arg(&sentinel_path);
    }

    if let Some((flag, val)) = phase.sidecar_flag() {
        cmd.arg(flag).arg(val.to_string());
    }

    tracing::info!(
        n,
        phase = ?phase,
        capacity = %gov.capacity.value.label(),
        capacity_src = gov.capacity.source.label(),
        max_workers = ?gov.max_workers.value,
        priority = priority.as_str(),
        cancel_supported,
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
    // Stdout is Stdio::null() (quiet) or inherited — never a pipe. Stderr is
    // already draining on the StderrRing background thread (line above). No
    // pipe-buffer deadlock risk; run_with_drain (travsr-indexer) is not
    // available to this crate due to the dependency direction.
    #[allow(clippy::disallowed_methods)]
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

                // WS3 (E4): while a cancel is draining, treat it as progress so the
                // no-progress watchdog cannot force-kill the sidecar while it commits
                // its final batch. The grace window is enforced by the terminate path.
                if sentinel_path.exists() {
                    last_progress = now;
                }

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
    if embed_paused() {
        tracing::debug!("embed Phase 1: auto-reindex paused (stop-embed) — skipping");
        return false;
    }
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
            "graph centrality not computed yet, deferring the first embedding pass"
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
    if embed_paused() {
        tracing::debug!("embed Phase 2: auto-reindex paused (stop-embed) — skipping");
        return false;
    }
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
    if embed_paused() {
        tracing::debug!("embed reindex-all: auto-reindex paused (stop-embed) — skipping");
        return false;
    }
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
/// Reads the PER-REPO config from `<repo>/.travsr/embed.toml` first (derived
/// from `db_path`), falling back to the machine-global config when the repo
/// has none. The repo layer is what an explicit `embed switch`/`embed init`
/// inside a repo writes (#481); the global fallback matters when a backend
/// was installed before the repo was ever indexed — `embed init` cannot write
/// a repo config for a `graph.db` that doesn't exist yet (`cmd_init`), so
/// without this fallback the very next `embed reindex` had nothing to resolve
/// and failed outright even though a perfectly good active backend exists.
///
/// This is safe for the daemon's *automatic* background reindex: that path
/// has its own, separate opt-in gate on `repo_backend_id` alone
/// (`maybe_spawn_embed`, travsr-daemon) specifically so a repo the user never
/// ran `embed init` inside is never auto-embedded. This fallback only widens
/// what an *explicit* `travsr embed reindex`/`embed init` can resolve.
fn resolve_backend(db_path: &Path) -> Option<(PathBuf, PathBuf, String)> {
    let repo_root = db_path.parent().and_then(|p| p.parent())?;
    let backend_id = repo_backend_id(repo_root).or_else(active_backend_id)?;
    let backend = lookup(&backend_id)?;
    let home = dirs::home_dir()?;
    let bin_path = home
        .join(".travsr")
        .join("bin")
        .join(backend.binary_filename());
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
    let _ = sidecar.knn("warmup", 1, &model_id, travsr_plugin_protocol::Space::Code);

    let cosines = queries
        .iter()
        .map(|q| {
            sidecar
                .knn(q, 5, &model_id, travsr_plugin_protocol::Space::Code)
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

/// `(model_id, row_count, vector_bytes)`.
pub type ModelUsage = (String, u64, u64);

/// Row and vector-byte counts per model in a repo's `embed.db`, descending by
/// row count. Returns an empty vec when the file does not exist yet (no
/// reindex has run).
///
/// Shared by `embed switch` (the inactive-vector hint), `embed status` (the
/// reclaimable-space line) and `embed gc` (the sweep itself), so all three
/// report the same numbers for the same database.
pub fn embeddings_by_model(embed_db_path: &Path) -> anyhow::Result<Vec<ModelUsage>> {
    use anyhow::Context as _;
    if !embed_db_path.exists() {
        return Ok(Vec::new());
    }
    let conn = rusqlite::Connection::open_with_flags(
        embed_db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {}", embed_db_path.display()))?;
    let mut stmt = conn.prepare(
        "SELECT model_id, COUNT(*), COALESCE(SUM(LENGTH(embedding)), 0) \
         FROM node_embeddings GROUP BY model_id ORDER BY COUNT(*) DESC",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)? as u64,
                r.get::<_, i64>(2)? as u64,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Delete every embedding row whose `model_id` is not in `keep_ids`, then
/// `VACUUM` to actually shrink the file on disk. Returns the per-model
/// row/byte counts that were reclaimed (measured before the delete) and
/// whether `VACUUM` ran.
///
/// `VACUUM` needs free disk space roughly equal to the database's size. If
/// there isn't enough, the delete still lands — rows are gone, which is the
/// correctness-critical part — but the file stays its prior size on disk.
/// That is reported via the returned `Option<String>` — `None` for "shrank",
/// `Some(reason)` for "rows gone, file not shrunk" — not treated as a failure:
/// the delete already succeeded by the time `VACUUM` would run, and erroring
/// out after a successful delete would be the worst outcome (L2's own design
/// note). The reason is carried rather than inferred because disk space is not
/// the only cause: a concurrent reader (daemon, MCP server) denies `VACUUM` its
/// exclusive lock just as easily.
///
/// Policy — which models to keep — is the caller's job (`embed gc` resolves
/// it from the repo's active model plus any `--keep`). This function only
/// executes a keep-set it is handed, and refuses an empty one: an empty
/// keep-set would delete every vector in the repo, and that is a caller bug,
/// not a valid request.
pub fn gc_embeddings(
    embed_db_path: &Path,
    keep_ids: &[String],
) -> anyhow::Result<(Vec<ModelUsage>, Option<String>)> {
    use anyhow::Context as _;
    anyhow::ensure!(
        !keep_ids.is_empty(),
        "refusing to gc with an empty keep-set (would delete every vector in the repo)"
    );

    let reclaimed: Vec<ModelUsage> = embeddings_by_model(embed_db_path)?
        .into_iter()
        .filter(|(m, _, _)| !keep_ids.iter().any(|k| k == m))
        .collect();
    if reclaimed.is_empty() {
        return Ok((reclaimed, None));
    }

    let conn = rusqlite::Connection::open(embed_db_path)
        .with_context(|| format!("opening {}", embed_db_path.display()))?;
    let placeholders = keep_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!("DELETE FROM node_embeddings WHERE model_id NOT IN ({placeholders})");
    let params: Vec<&dyn rusqlite::ToSql> =
        keep_ids.iter().map(|k| k as &dyn rusqlite::ToSql).collect();
    conn.execute(&sql, params.as_slice())
        .context("deleting inactive-model embeddings")?;

    // VACUUM rewrites the whole file, so it needs room for a second copy. It
    // also needs an exclusive lock, which a daemon or MCP server holding a read
    // on embed.db will deny — this lock excludes reindex/gc, not readers. Both
    // are routine, so report *which* one happened rather than collapsing every
    // cause into "not enough disk space".
    let db_size = std::fs::metadata(embed_db_path).map(|m| m.len()).ok();
    let free_space = fs2::available_space(embed_db_path).ok();
    let vacuum_skipped: Option<String> = match (free_space, db_size) {
        (Some(free), Some(size)) if free <= size.saturating_mul(2) => Some(format!(
            "not enough free disk space (VACUUM needs room for a second copy: \
             embed.db is {size} bytes, {free} bytes free)"
        )),
        _ => conn.execute_batch("VACUUM").err().map(|e| e.to_string()),
    };

    Ok((reclaimed, vacuum_skipped))
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

    /// Serializes tests that mutate the process-global `REINDEX_IN_FLIGHT` /
    /// `REINDEX_CHILD_PID` statics. `cargo test` runs tests in parallel, so
    /// without this a guard held by one test makes another test's
    /// `try_acquire()` return `None` (observed as a macOS-only flake on PR #440:
    /// `reindex_guard_resets_flag_on_drop` panicking on "flag should be free").
    /// Poison is recovered because these tests may panic by design.
    static REINDEX_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    // ── TC-00: catalog invariants ─────────────────────────────────────────────

    // ── TC-L8: in-flight PID guard ───────────────────────────────────────────

    #[test]
    fn terminate_inflight_reindex_is_noop_when_idle() {
        let _serial = REINDEX_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // PID is 0 → no panic, no kill attempt.
        REINDEX_CHILD_PID.store(0, Ordering::SeqCst);
        terminate_inflight_reindex(); // must not panic
        assert_eq!(REINDEX_CHILD_PID.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn reindex_pid_guard_clears_pid() {
        let _serial = REINDEX_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        REINDEX_CHILD_PID.store(12345, Ordering::SeqCst);
        let guard = ReindexPidGuard;
        assert_eq!(REINDEX_CHILD_PID.load(Ordering::SeqCst), 12345);
        drop(guard);
        assert_eq!(REINDEX_CHILD_PID.load(Ordering::SeqCst), 0);
    }

    /// #500 regression: pid_alive must report real liveness on every platform.
    /// The Windows stub used to hardcode `false`, which made the shutdown
    /// grace poll claim "drained gracefully" on its first iteration and turned
    /// the `kill_pid` fallback into dead code — orphaning the sidecar.
    #[test]
    fn pid_alive_reports_running_and_exited_processes() {
        assert!(
            pid_alive(std::process::id()),
            "the current process must be reported alive"
        );

        let mut child = if cfg!(windows) {
            let mut c = std::process::Command::new("cmd");
            c.args(["/C", "exit 0"]);
            c
        } else {
            std::process::Command::new("true")
        }
        .spawn()
        .expect("spawn short-lived child");
        let pid = child.id();
        child.wait().expect("wait for child");
        assert!(
            !pid_alive(pid),
            "an exited, reaped child must be reported dead"
        );
    }

    // ── TC-FT-M4: RAII single-flight guard ───────────────────────────────────

    #[test]
    fn reindex_guard_resets_flag_on_drop() {
        let _serial = REINDEX_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        let _serial = REINDEX_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        let _serial = REINDEX_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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

    /// Serializes tests that override the process-global `HOME` env var
    /// (`dirs::home_dir()` reads it fresh on every call, so parallel tests
    /// mutating it race on the same override).
    static HOME_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Reproduces the exact sequence that broke `docs-lane-nightly.yml`:
    /// `embed init` runs before the repo has ever been indexed, so `cmd_init`
    /// (embed.rs) cannot write a repo config for a `graph.db` that doesn't
    /// exist yet — only the machine-global config gets written. The repo is
    /// then indexed, and `embed reindex` must still resolve the backend from
    /// the global config rather than failing "No embedding backend active."
    #[test]
    #[cfg_attr(
        windows,
        ignore = "dirs::home_dir() on Windows ignores HOME/USERPROFILE entirely (SHGetKnownFolderPath) - this test's isolation cannot work there, see crates/travsr-cli/tests/embed_switch.rs's module doc comment"
    )]
    fn resolve_backend_falls_back_to_global_when_repo_unconfigured() {
        let _guard = HOME_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());

        let backend = &backends()[0];
        let bin_dir = home.path().join(".travsr").join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::write(bin_dir.join(backend.binary_filename()), b"").unwrap();
        let model_dir = home.path().join(".travsr").join("models").join(&backend.id);
        std::fs::create_dir_all(&model_dir).unwrap();
        for f in &backend.model_files {
            std::fs::write(model_dir.join(&f.name), b"").unwrap();
        }
        // Global config only — no repo config anywhere.
        std::fs::write(
            home.path().join(".travsr").join("embed.toml"),
            format!("active = \"{}\"\n", backend.id),
        )
        .unwrap();

        let graph_db = repo.path().join(".travsr").join("graph.db");
        std::fs::create_dir_all(graph_db.parent().unwrap()).unwrap();
        std::fs::write(&graph_db, b"").unwrap();

        let resolved = resolve_backend(&graph_db);

        match old_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }

        let (_, _, model_id) = resolved.expect(
            "resolve_backend must fall back to the machine-global config \
             when the repo has no config of its own",
        );
        assert_eq!(model_id, backend.id);
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "dirs::home_dir() on Windows ignores HOME/USERPROFILE entirely (SHGetKnownFolderPath) - this test's isolation cannot work there, see crates/travsr-cli/tests/embed_switch.rs's module doc comment"
    )]
    fn resolve_backend_prefers_repo_config_over_global() {
        let _guard = HOME_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());

        let all = backends();
        let (repo_backend, global_backend) = (&all[0], &all[1]);
        for b in [repo_backend, global_backend] {
            let bin_dir = home.path().join(".travsr").join("bin");
            std::fs::create_dir_all(&bin_dir).unwrap();
            std::fs::write(bin_dir.join(b.binary_filename()), b"").unwrap();
            let model_dir = home.path().join(".travsr").join("models").join(&b.id);
            std::fs::create_dir_all(&model_dir).unwrap();
            for f in &b.model_files {
                std::fs::write(model_dir.join(&f.name), b"").unwrap();
            }
        }
        std::fs::write(
            home.path().join(".travsr").join("embed.toml"),
            format!("active = \"{}\"\n", global_backend.id),
        )
        .unwrap();

        let graph_db = repo.path().join(".travsr").join("graph.db");
        std::fs::create_dir_all(graph_db.parent().unwrap()).unwrap();
        std::fs::write(&graph_db, b"").unwrap();
        write_repo_backend_id(repo.path(), &repo_backend.id).unwrap();

        let resolved = resolve_backend(&graph_db);

        match old_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }

        let (_, _, model_id) = resolved.expect("resolve_backend must resolve");
        assert_eq!(
            model_id, repo_backend.id,
            "the repo's own config must win over the machine-global default"
        );
    }

    // ── L5: cross-process reindex/gc lock ─────────────────────────────────────

    #[test]
    fn two_processes_cannot_both_hold_the_embed_lock() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".travsr")).unwrap();

        // Simulate another process holding the lock: a second, independent
        // open() + flock on the same path. `flock` does not treat two opens
        // by the same process as compatible, so this is a faithful stand-in
        // for a real second process (the existing `daemon_lock_held` test
        // uses the same technique).
        let held = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(repo.path().join(".travsr").join("embed.lock"))
            .unwrap();
        fs2::FileExt::lock_exclusive(&held).unwrap();
        std::fs::write(
            repo.path().join(".travsr").join("embed.lock.info"),
            b"41822\treindex",
        )
        .unwrap();

        let err = EmbedOpLock::try_acquire(repo.path(), "gc")
            .expect_err("must not acquire while another holder has the lock");
        let msg = err.to_string();
        assert!(
            msg.contains("41822") && msg.contains("reindex"),
            "error must name the holder's recorded pid and operation, got: {msg}"
        );

        fs2::FileExt::unlock(&held).unwrap();
        drop(held);
        let free = EmbedOpLock::try_acquire(repo.path(), "gc").unwrap();
        assert!(
            free.is_some(),
            "must acquire once the other holder releases"
        );
    }

    /// `Ok(None)` means "not an indexed repo", and `run_parallel_reindex` reads
    /// it as "no lock needed, carry on". So it must be reachable *only* when
    /// `.travsr` is genuinely absent — an unopenable lock file on a real repo
    /// has to be an error, or the reindex runs completely unserialized and L5's
    /// whole guarantee evaporates silently.
    #[test]
    fn an_unopenable_lock_on_an_indexed_repo_is_an_error_not_a_skip() {
        let repo = tempfile::tempdir().unwrap();
        let travsr_dir = repo.path().join(".travsr");
        std::fs::create_dir_all(&travsr_dir).unwrap();
        // A directory where the lock file belongs: open(write) fails with a
        // kind that is not NotFound, on every platform.
        std::fs::create_dir(travsr_dir.join("embed.lock")).unwrap();

        let err = EmbedOpLock::try_acquire(repo.path(), "reindex")
            .expect_err("an unopenable lock on an indexed repo must not be reported as 'no repo'");
        assert!(
            err.to_string()
                .contains("refusing to run an embed operation"),
            "the error must say the operation was refused, got: {err}"
        );

        // The contrast case: no `.travsr` at all really is "not indexed yet".
        let bare = tempfile::tempdir().unwrap();
        assert!(
            EmbedOpLock::try_acquire(bare.path(), "reindex")
                .unwrap()
                .is_none(),
            "a repo with no .travsr directory must still yield Ok(None)"
        );
    }

    /// Only contention is worth waiting on. A lock file that cannot be opened
    /// will not become openable, so `try_acquire_with_retry` must surface it
    /// immediately rather than burning its full 10s budget first.
    #[test]
    fn retry_does_not_wait_out_its_budget_on_a_non_contention_error() {
        let repo = tempfile::tempdir().unwrap();
        let travsr_dir = repo.path().join(".travsr");
        std::fs::create_dir_all(&travsr_dir).unwrap();
        std::fs::create_dir(travsr_dir.join("embed.lock")).unwrap();

        let started = std::time::Instant::now();
        EmbedOpLock::try_acquire_with_retry(repo.path(), "reindex")
            .expect_err("an unopenable lock must not be retried into success");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "must fail fast, took {:?}",
            started.elapsed()
        );
    }

    /// Child-process helper for `lock_is_released_when_the_holder_is_killed`.
    /// A no-op in every normal test run; when re-exec'd as a subprocess with
    /// `EMBED_LOCK_HOLD_REPO` set, it holds the lock and blocks so the parent
    /// can SIGKILL it — the only way to exercise "the OS releases `flock` when
    /// its holder dies", which a same-process guard `Drop` can never prove.
    #[test]
    fn embed_lock_hold_child_helper() {
        let Ok(repo) = std::env::var("EMBED_LOCK_HOLD_REPO") else {
            return;
        };
        let repo = std::path::Path::new(&repo);
        // The parent polls readiness by attempting (and instantly releasing)
        // its own transient acquire, so a single collision with that probe is
        // expected, not fatal — retry briefly rather than failing on the
        // first miss.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let _lock = loop {
            if let Ok(Some(lock)) = EmbedOpLock::try_acquire(repo, "reindex") {
                break lock;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "child must acquire the lock"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        std::thread::sleep(std::time::Duration::from_secs(120));
    }

    #[test]
    fn lock_is_released_when_the_holder_is_killed() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".travsr")).unwrap();

        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "embed_catalog::tests::embed_lock_hold_child_helper",
                "--nocapture",
            ])
            .env("EMBED_LOCK_HOLD_REPO", repo.path())
            .spawn()
            .expect("spawn child holder");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if EmbedOpLock::try_acquire(repo.path(), "probe").is_err() {
                break; // child now holds it
            }
            if std::time::Instant::now() > deadline {
                let _ = child.kill();
                panic!("child never acquired the lock");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        // Hard-kill, not a graceful exit — this is the case a hand-rolled PID
        // file would get wrong (L5's stated reason for using `flock`).
        child.kill().expect("kill child");
        child.wait().expect("reap child");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let reacquired = loop {
            if matches!(
                EmbedOpLock::try_acquire(repo.path(), "reindex"),
                Ok(Some(_))
            ) {
                break true;
            }
            if std::time::Instant::now() > deadline {
                break false;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        assert!(
            reacquired,
            "lock must be re-acquirable once its holder is killed"
        );
    }

    // ── L2: embed gc ────────────────────────────────────────────────────────

    /// Builds a temp `embed.db` with `node_embeddings` rows for each
    /// `(model_id, row_count)` pair, using a fixed byte-length embedding blob
    /// per row so byte-count assertions are exact and readable.
    fn fake_embed_db(rows: &[(&str, u64)]) -> tempfile::TempPath {
        let file = tempfile::NamedTempFile::new().unwrap();
        let conn = rusqlite::Connection::open(file.path()).unwrap();
        conn.execute_batch(
            "CREATE TABLE node_embeddings (
                 node_id   INTEGER NOT NULL,
                 model_id  TEXT    NOT NULL,
                 embedding BLOB    NOT NULL,
                 text_hash TEXT,
                 PRIMARY KEY (node_id, model_id)
             ) WITHOUT ROWID;",
        )
        .unwrap();
        let blob = vec![0u8; 8];
        let mut node_id = 0i64;
        for (model_id, count) in rows {
            for _ in 0..*count {
                node_id += 1;
                conn.execute(
                    "INSERT INTO node_embeddings (node_id, model_id, embedding) VALUES (?1, ?2, ?3)",
                    rusqlite::params![node_id, model_id, blob],
                )
                .unwrap();
            }
        }
        drop(conn);
        file.into_temp_path()
    }

    fn model_counts(path: &Path) -> std::collections::BTreeMap<String, u64> {
        embeddings_by_model(path)
            .unwrap()
            .into_iter()
            .map(|(m, rows, _)| (m, rows))
            .collect()
    }

    #[test]
    fn gc_embeddings_refuses_empty_keep_set() {
        let db = fake_embed_db(&[("arctic-embed-m-v1.5", 5)]);
        let err = gc_embeddings(&db, &[]).expect_err("empty keep-set must be refused");
        assert!(err.to_string().contains("empty keep-set"));
        // Nothing was touched.
        assert_eq!(model_counts(&db).get("arctic-embed-m-v1.5"), Some(&5));
    }

    #[test]
    fn gc_embeddings_never_deletes_the_active_model() {
        let db = fake_embed_db(&[("arctic-embed-m-v1.5", 5), ("bge-large-en-v1.5", 3)]);
        let (reclaimed, _) = gc_embeddings(&db, &["arctic-embed-m-v1.5".to_string()]).unwrap();
        assert_eq!(reclaimed.len(), 1);
        assert_eq!(reclaimed[0].0, "bge-large-en-v1.5");
        let after = model_counts(&db);
        assert_eq!(after.get("arctic-embed-m-v1.5"), Some(&5));
        assert_eq!(after.get("bge-large-en-v1.5"), None);
    }

    #[test]
    fn gc_embeddings_with_keep_retains_the_named_model() {
        let db = fake_embed_db(&[
            ("arctic-embed-m-v1.5", 5),
            ("bge-large-en-v1.5", 3),
            ("bge-small-en-v1.5", 2),
        ]);
        let keep = vec![
            "arctic-embed-m-v1.5".to_string(),
            "bge-large-en-v1.5".to_string(),
        ];
        let (reclaimed, _) = gc_embeddings(&db, &keep).unwrap();
        assert_eq!(reclaimed.len(), 1);
        assert_eq!(reclaimed[0].0, "bge-small-en-v1.5");
        let after = model_counts(&db);
        assert_eq!(after.get("arctic-embed-m-v1.5"), Some(&5));
        assert_eq!(after.get("bge-large-en-v1.5"), Some(&3));
        assert_eq!(after.get("bge-small-en-v1.5"), None);
    }

    #[test]
    fn gc_embeddings_is_idempotent() {
        let db = fake_embed_db(&[("arctic-embed-m-v1.5", 5), ("bge-large-en-v1.5", 3)]);
        let keep = vec!["arctic-embed-m-v1.5".to_string()];
        let (first, _) = gc_embeddings(&db, &keep).unwrap();
        assert_eq!(first.len(), 1);
        let (second, _) = gc_embeddings(&db, &keep).unwrap();
        assert!(
            second.is_empty(),
            "second run must find nothing left to reclaim, got {second:?}"
        );
        assert_eq!(model_counts(&db).get("arctic-embed-m-v1.5"), Some(&5));
    }

    // ── model.toml descriptor: the sidecar contract ──────────────────────────

    fn backend_with_arch(arch: &str) -> EmbedBackend {
        EmbedBackend {
            id: "test-model".into(),
            description: String::new(),
            dim: 768,
            params_m: 109,
            mteb: 65.0,
            ram_mb: 450,
            init_secs: 47,
            binary_name: "travsr-embed".into(),
            github_repo: "Travsr-com/travsr-embed".into(),
            version_fallback: "v1.0.0".into(),
            pooling: "cls".into(),
            query_prefix: "q: ".into(),
            n_inputs: 2,
            truncate_dim: 0,
            arch: arch.into(),
            model_files: vec![],
        }
    }

    /// The catalog's `arch` must reach the sidecar as `family`. Without it the
    /// sidecar assumes "bert" and hands a non-BERT graph to tract, which fails at
    /// graph load partway through a reindex (travsr-embed #6).
    #[test]
    fn descriptor_carries_arch_as_family() {
        let dir = tempfile::tempdir().unwrap();
        write_model_descriptor(dir.path(), &backend_with_arch("modernbert")).unwrap();
        let toml = std::fs::read_to_string(dir.path().join("model.toml")).unwrap();
        assert!(toml.contains(r#"family = "modernbert""#), "{toml}");
        // The rest of the contract must survive the addition.
        assert!(toml.contains("dim = 768"), "{toml}");
        assert!(toml.contains(r#"pooling = "cls""#), "{toml}");
        assert!(toml.contains("n_inputs = 2"), "{toml}");
    }

    /// An entry with no `arch` must omit the key entirely rather than write an
    /// empty string: the sidecar defaults absent to "bert" but rejects "".
    #[test]
    fn descriptor_omits_family_when_arch_is_unset() {
        let dir = tempfile::tempdir().unwrap();
        write_model_descriptor(dir.path(), &backend_with_arch("")).unwrap();
        let toml = std::fs::read_to_string(dir.path().join("model.toml")).unwrap();
        assert!(!toml.contains("family"), "{toml}");
    }

    /// Every bundled entry must declare an arch, or its model.toml silently
    /// defaults to bert — fine today, wrong the moment a non-BERT model lands.
    #[test]
    fn bundled_catalog_entries_all_declare_arch() {
        for b in backends() {
            assert!(!b.arch.trim().is_empty(), "{} has no arch", b.id);
        }
    }
}
