# RFC-020: Parallel Reindex Workers

**Status:** Draft  
**Author:** Tech Lead / SWE / QA  
**Date:** 2026-06-24  
**Crates affected:** `travsr-plugin-host`, `travsr-daemon`, `travsr-cli`  
**Sidecar affected:** `travsr-embed-<backend>` (external — `Travsr-com/travsr-embed`)

---

## Summary

Replace the current single-sidecar fire-and-forget reindex with N parallel sidecar
workers, each operating on a non-overlapping node-id range and writing to its own
isolated temp database. A merge step consolidates all temp databases into `embed.db`
after all workers complete. This eliminates write contention entirely and brings
CPU-only throughput from 32 nodes/sec to the ~1 000 nodes/sec target.

---

## Motivation

Metal and CoreML are unavailable in the current environment. The only remaining levers
for throughput are:

| Fix | Factor | Owner |
|---|---|---|
| `candle-accelerate` (Accelerate BLAS on M-series CPU) | ~3× | sidecar build |
| Internal reindex batch size 32 → 256 | ~2.5× | sidecar |
| 2-thread pipeline (DB read ↔ inference) | ~1.5× | sidecar |
| N parallel workers (this RFC) | ~3.5× (N = 4) | **this repo + sidecar** |

Combined: 3 × 2.5 × 1.5 × 3.5 ≈ **39×** → ~1 250 nodes/sec.  
This RFC owns the parallel-worker and merge-orchestrator concerns only. The
sidecar-internal fixes (Accelerate, batch size, pipeline) are tracked separately.

---

## Constraints Being Formalised

This section is the normative core. Each constraint has a rationale, a failure mode
if violated, and a test identifier owned by QA.

---

### C-01 — Worker Count Must Be Runtime-Derived

**Rule:**
```
num_workers = clamp(p_cores, 1, MAX_EMBED_WORKERS)
```
where:
- `p_cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)`
- `MAX_EMBED_WORKERS = 8` (compile-time constant, configurable via
  `TRAVSR_EMBED_WORKERS` env var)

**Rationale:**  
Hardcoding 4 workers is wrong on M2 Pro (6–8 P-cores) and wasteful on CI
runners (2 cores). `available_parallelism()` is already used for the parse-worker
pool in `daemon/src/lib.rs:729` — same pattern applies here. We clamp at 8 to
prevent OOM on machines with many cores but constrained RAM (each sidecar loads
~548 MB of model weights + ~128 MB SQLite page cache).

**RAM guard:**  
Before spawning, compute `available_memory_mb()` (via `sysctl hw.memsize` on
macOS, `/proc/meminfo` on Linux). If `available_memory_mb < num_workers * 700`,
reduce `num_workers = floor(available_memory_mb / 700)`. Never reduce below 1.

**Failure mode if violated:**  
Fixed 4 workers on a 2-core CI runner → 4 processes compete for 2 cores → 2×
slower than a single worker. Fixed 4 workers on a 16-core M3 Max → 4× slower
than optimal.

**QA test:** `TC-01`

---

### C-02 — Ranges Must Be Non-Overlapping and Cover All Nodes

**Rule:**  
Before spawning, query graph.db for the node-id bounds of embeddable nodes:
```sql
SELECT MIN(id), MAX(id)
FROM nodes
WHERE kind NOT IN ('file','file-module','import','module','field','variable')
  AND NOT EXISTS (
    SELECT 1 FROM edb.node_embeddings
    WHERE node_id = nodes.id AND model_id = ?
  )
```
Partition `[min_id, max_id]` into exactly `num_workers` contiguous, non-overlapping
half-open ranges `[start, end)`. The last range uses `end = i64::MAX` to capture
any rows inserted between the range computation and the sidecar SELECT.

```rust
fn partition_ranges(min_id: i64, max_id: i64, n: usize) -> Vec<(i64, i64)> {
    // O(n), n ≤ 8
    let span = max_id.saturating_sub(min_id).saturating_add(1);
    let chunk = span / n as i64;
    (0..n).map(|i| {
        let start = min_id + i as i64 * chunk;
        let end = if i + 1 == n { i64::MAX } else { start + chunk };
        (start, end)
    }).collect()
}
```

The sidecar adds this filter to its reindex SELECT:
```sql
AND id >= ?row_start AND id < ?row_end
```

**Failure mode if violated:**  
Without ranges, two workers SELECT the same batch (the `NOT EXISTS` check sees
neither has committed yet), do duplicate inference, and race on write. Correct
but wastes ~30–50% of inference work in the first few batches.

**QA test:** `TC-02`, `TC-03`

---

### C-03 — Each Worker Writes to an Isolated Temp Database

**Rule:**  
Worker `i` receives `--embed-db .travsr/embed.w<i>.db` instead of
`.travsr/embed.db`. The main `embed.db` is never opened by any worker during the
parallel phase. Workers never see each other's files.

Temp file naming: `embed.w<i>.db` where `i` is zero-indexed. Example for 4 workers:
```
.travsr/embed.w0.db   (node_ids [min, q1))
.travsr/embed.w1.db   (node_ids [q1,  q2))
.travsr/embed.w2.db   (node_ids [q2,  q3))
.travsr/embed.w3.db   (node_ids [q3, i64::MAX))
```

The sidecar creates, migrates, and writes its temp database identically to how it
would write `embed.db`. The schema is identical — the file path is the only
difference. This means the sidecar requires zero changes to its write path; only
the `--embed-db` and `--row-start/--row-end` flags are new.

**Failure mode if violated:**  
Multiple writers on the same SQLite file without `busy_timeout` → `SQLITE_BUSY`
→ lost embedding batches (the losing writer's work is silently dropped).
`busy_timeout` is not set in the current `configure()` function (confirmed in
`travsr-store/src/lib.rs:573`).

**QA test:** `TC-04`

---

### C-04 — The Merge Step Must Be Atomic Per Source File

**Rule:**  
After all workers exit (success or failure), the orchestrator merges surviving
temp files into `embed.db` one at a time. For each temp file `embed.wN.db`:

```rust
// Pseudocode — real implementation in embed_catalog.rs
fn merge_one(conn: &Connection, worker_db: &Path) -> Result<MergeResult> {
    let path_str = worker_db.to_str().ok_or(/* ... */)?;
    // Step 1: ATTACH (can fail on corruption)
    conn.execute_batch(&format!("ATTACH DATABASE '{path_str}' AS wN"))?;
    // Step 2: bulk copy
    conn.execute_batch(
        "INSERT OR REPLACE INTO node_embeddings \
         SELECT node_id, model_id, embedding FROM wN.node_embeddings"
    )?;
    // Step 3: DETACH — must succeed before deleting the file
    conn.execute_batch("DETACH DATABASE wN")?;
    // Step 4: delete the temp file ONLY after DETACH succeeds
    std::fs::remove_file(worker_db)?;
    Ok(MergeResult::Success)
}
```

**Atomic guarantee:**  
- The temp file is **never deleted** until `DETACH` returns `Ok`.
- If `ATTACH` fails (corruption): log the error, skip this file, continue with
  remaining workers. The unembedded nodes in that range are handled by C-06.
- If `INSERT OR REPLACE` fails: `DETACH`, leave the file, log the path for
  manual inspection. Do not delete.
- If `DETACH` fails: leave the file, retry `DETACH` once after 100 ms. If the
  second attempt also fails, leave the file and log.

**QA test:** `TC-05`, `TC-06`, `TC-07`

---

### C-05 — Worker Failure Must Not Abort the Merge

**Rule:**  
A worker is considered failed if its exit code is non-zero OR if its temp file
does not exist when the merge begins. Both cases are non-fatal to the overall
reindex:

```rust
enum WorkerOutcome {
    Success { temp_db: PathBuf },
    Failed  { range: (i64, i64), reason: String },
    // temp file absent = sidecar crashed before writing anything
}
```

The orchestrator collects all outcomes, then:
1. Merges temp files from `Success` workers (C-04).
2. Logs `Failed` workers with their ranges at `WARN` level.
3. Returns `Ok(())` to the caller regardless — partial embedding is not an error.

Failed workers' ranges are implicitly handled by C-06 (the daemon's `embed_tick`
will re-emit a full reindex on the next tick, picking up the missed nodes via the
`NOT EXISTS` check).

**Failure mode if violated:**  
Aborting the merge on first worker failure loses all work from successful workers.
With 4 workers and a 25% failure rate (e.g., OOM kill of one worker), the entire
batch is discarded and throughput collapses to 0.

**QA test:** `TC-08`, `TC-09`

---

### C-06 — Self-Healing via embed_tick

**Rule:**  
The `embed_tick` in `daemon/src/lib.rs` fires every 60 seconds and calls
`embed_progress()`. This query compares `COUNT(*) FROM edb.node_embeddings` against
`COUNT(*) FROM nodes WHERE <kind_filter>`. Any gap (nodes without embeddings,
including those from failed workers) triggers a new `spawn_background_reindex_*`
call.

This constraint requires **no code change** — it is an existing invariant that must
not be broken by this RFC. Specifically:

- The merge step must write to the canonical `embed.db` path (not leave rows only
  in temp files), so `embed_progress()` counts correctly.
- Failed worker ranges leave a gap in `embed.db` that `embed_progress()` detects
  on the next tick.
- The next reindex pass uses the same `NOT EXISTS` guard and will embed the missing
  nodes without re-embedding the already-present ones.

**Failure mode if violated:**  
If the merge writes to a different path (e.g., `embed.merged.db`) and `embed.db`
is not updated, `embed_progress()` always returns 0 embedded → daemon keeps
spawning reindex processes in an infinite loop.

**QA test:** `TC-10`

---

### C-07 — Stale Temp Files Must Be Cleaned Up on Next Run

**Rule:**  
At the start of every `spawn_background_reindex_phase1` or
`spawn_background_reindex_all` call, glob for `embed.wN.db` files in the
`.travsr/` directory and delete any that exist. These are always leftover artifacts
from a previous crashed orchestrator — a valid previous run always deletes them in
C-04.

```rust
fn cleanup_stale_worker_dbs(travsr_dir: &Path) {
    let pattern = travsr_dir.join("embed.w*.db");
    // glob expand, remove each
}
```

**Failure mode if violated:**  
A stale `embed.w0.db` from a previous crashed run contains outdated embeddings
for node-ids that may have since been deleted (C-03 in v17 tombstones covers
graph.db, but embed.db tombstone processing happens at reindex start inside the
sidecar — a stale file bypasses this). Merging stale data into `embed.db` can
insert embeddings for deleted nodes. The KNN index then returns dead node-ids.

**QA test:** `TC-11`

---

### C-08 — `--row-start` / `--row-end` Are Required Sidecar Flags (External Contract)

**Rule:**  
The sidecar binary (`travsr-embed-<backend>`) MUST implement `--row-start <i64>`
and `--row-end <i64>` flags. The reindex SELECT filter becomes:

```sql
SELECT id, signature, path, kind, language
FROM nodes
WHERE kind NOT IN ('file','file-module','import','module','field','variable')
  AND NOT EXISTS (
    SELECT 1 FROM node_embeddings WHERE node_id = nodes.id AND model_id = ?
  )
  AND id >= ?row_start
  AND id <  ?row_end
ORDER BY shell_number DESC NULLS LAST
LIMIT ?batch_size
```

When `--row-start` and `--row-end` are absent, the sidecar behaves as today
(full range = backward-compatible single-worker mode).

This is the **external API contract** between this repo and `travsr-embed`. Both
sides must ship together. This RFC is a blocking dependency on the sidecar's
release.

**Failure mode if violated:**  
Parallel workers without range flags → duplicate inference on the same nodes (~50%
wasted CPU in early batches) → effective throughput drops toward single-worker
speed despite using N processes.

**QA test:** `TC-02`, `TC-03`

---

## Detailed Design

### Orchestrator Flow

```
spawn_background_reindex_phase1(db_path, threshold)
│
├── 1. cleanup_stale_worker_dbs()            [C-07]
│
├── 2. query MIN/MAX pending node ids        [C-02]
│        → if no pending nodes: return early (fast no-op)
│
├── 3. derive num_workers                    [C-01]
│        = clamp(available_parallelism, 1, MAX_EMBED_WORKERS)
│        → apply RAM guard
│
├── 4. partition_ranges(min, max, n)         [C-02]
│
├── 5. for each worker i in 0..n:            [C-03]
│        temp_db = .travsr/embed.wN.db
│        spawn sidecar --reindex <db>
│                       --embed-db <temp_db>
│                       --phase1 <threshold>
│                       --row-start <start>
│                       --row-end   <end>
│                       (detached, stdout/stderr null)
│
├── 6. wait_for_all_workers()
│        → collect WorkerOutcome for each
│
├── 7. open embed.db (writable)
│        for each Success outcome:            [C-04, C-05]
│            merge_one(embed.db, temp_db)
│        for each Failed outcome:
│            log WARN with range             [C-05]
│
└── 8. return Ok(())                         [C-06 — embed_tick handles gaps]
```

### State Machine for a Single Worker

```
SPAWNED
  │
  ├─[sidecar exits 0]──→ SUCCESS { temp_db exists }
  │                          │
  │                          └──→ MERGED into embed.db → temp_db deleted
  │
  ├─[sidecar exits non-0]──→ FAILED
  │                             │
  │                             └──→ temp_db may or may not exist
  │                                      if exists: attempt merge (partial ok)
  │                                      if absent: log, skip
  │
  └─[sidecar killed / timeout]──→ FAILED (same path)
```

### New Constants and Env Vars

```rust
// embed_catalog.rs
pub const MAX_EMBED_WORKERS: usize = 8;
pub const WORKER_RAM_BUDGET_MB: u64 = 700; // model (~548) + page cache (~128) + headroom

// TRAVSR_EMBED_WORKERS=N overrides clamp ceiling at runtime
// TRAVSR_EMBED_BATCH_SIZE=N passes --batch-size to sidecar (future)
```

---

## Sidecar Contract (External — `Travsr-com/travsr-embed`)

New flags required. All are optional; when absent, the sidecar behaves as v1.0.0:

| Flag | Type | Default | Description |
|---|---|---|---|
| `--row-start` | `i64` | `i64::MIN` | Inclusive lower bound on `nodes.id` |
| `--row-end` | `i64` | `i64::MAX` | Exclusive upper bound on `nodes.id` |
| `--batch-size` | `u32` | 100 | Internal reindex batch size (SELECT LIMIT) |
| `--busy-timeout-ms` | `u32` | 5000 | SQLite busy_timeout on embed.db connection |

The `--embed-db` flag already exists (confirmed in `embed_catalog.rs:110`).

---

## QA Test Specification

### TC-01 — Worker count is clamped correctly

```rust
#[test]
fn worker_count_respects_parallelism_and_ceiling() {
    // Simulate 2-core machine
    let n = derive_num_workers_for_test(2, 16_000);
    assert_eq!(n, 2);
    // Simulate 12-core machine within RAM
    let n = derive_num_workers_for_test(12, 16_000);
    assert_eq!(n, 8); // clamped at MAX_EMBED_WORKERS
    // Simulate 8-core machine, RAM-constrained (4 × 700 = 2800 MB)
    let n = derive_num_workers_for_test(8, 2_800);
    assert_eq!(n, 4); // RAM guard kicks in: floor(2800 / 700)
}
```

### TC-02 — Ranges are non-overlapping and contiguous

```rust
#[test]
fn ranges_cover_full_span_with_no_gaps_or_overlaps() {
    let ranges = partition_ranges(100, 1_099, 4);
    // non-overlapping
    for w in ranges.windows(2) {
        assert_eq!(w[0].1, w[1].0, "gap or overlap between workers");
    }
    // first starts at min
    assert_eq!(ranges[0].0, 100);
    // last ends at i64::MAX
    assert_eq!(ranges[3].1, i64::MAX);
    // all 1000 node-ids covered (mod i64::MAX edge)
    let covered: i64 = ranges.iter()
        .map(|(s, e)| e.saturating_sub(*s).min(1_000))
        .sum();
    assert_eq!(covered, 1_000);
}
```

### TC-03 — No duplicate embeddings when workers run concurrently (integration)

```
Given: graph.db with 400 nodes, 4 workers with correct ranges
When:  all 4 workers complete successfully
Then:  COUNT(DISTINCT node_id) in embed.db == 400
       No node_id appears in more than one worker's temp file
```

### TC-04 — Write contention is zero (no SQLITE_BUSY during parallel phase)

```
Given: 4 workers each writing to embed.wN.db
When:  all run concurrently for the full reindex duration
Then:  zero SQLITE_BUSY errors in any sidecar stderr
       (verified by redirecting stderr to a temp file per worker and scanning it)
```

### TC-05 — Merge skips corrupted temp file and continues

```rust
#[test]
fn merge_continues_on_corrupt_worker_db() {
    // Create embed.w0.db with valid rows
    // Corrupt embed.w1.db (truncate to 10 bytes)
    // Create embed.w2.db with valid rows
    let result = merge_all_worker_dbs(&embed_db, &[w0, w1_corrupt, w2]);
    assert!(result.is_ok());
    // w0 and w2 rows present in embed.db
    assert_eq!(count_embeddings(&embed_db), count_embeddings(&w0) + count_embeddings(&w2));
    // w1_corrupt not deleted (left for inspection)
    assert!(w1_corrupt.exists());
}
```

### TC-06 — Temp file not deleted until DETACH succeeds

```rust
#[test]
fn temp_file_survives_failed_insert() {
    // Simulate INSERT OR REPLACE failing (e.g., schema mismatch)
    // inject failure via mock connection
    let result = merge_one(&broken_conn, &worker_db);
    assert!(result.is_err());
    // temp file must still exist
    assert!(worker_db.exists(), "temp file deleted before successful DETACH");
}
```

### TC-07 — Merge is idempotent (re-running merge after partial failure is safe)

```
Given: merge ran, merged w0 and w2, failed on w1 (left on disk)
When:  merge is re-run (e.g., daemon retried)
Then:  w0 and w2 rows are INSERT OR REPLACE'd (no duplicates — same PK)
       w1 merge attempt is retried
       Final embed.db has correct row count
```

### TC-08 — One worker failure does not abort successful workers' merge

```rust
#[test]
fn partial_worker_failure_does_not_lose_successful_work() {
    // workers: w0=success(100 nodes), w1=failed(exit 1), w2=success(100 nodes), w3=failed(OOM)
    let outcomes = vec![
        WorkerOutcome::Success { temp_db: w0 },
        WorkerOutcome::Failed  { range: (q1, q2), reason: "exit 1".into() },
        WorkerOutcome::Success { temp_db: w2 },
        WorkerOutcome::Failed  { range: (q3, i64::MAX), reason: "OOM".into() },
    ];
    merge_outcomes(&embed_db, outcomes).unwrap();
    // 200 nodes from w0 + w2 are present
    assert_eq!(count_embeddings(&embed_db), 200);
}
```

### TC-09 — All workers failed — returns Ok, embed.db unchanged

```
Given: all 4 workers exit non-zero, no temp files exist
When:  orchestrator runs merge
Then:  returns Ok(())
       embed.db row count is unchanged (0 new rows)
       WARN logs emitted for each failed range
       embed_tick detects gap on next tick
```

### TC-10 — embed_tick self-heals after partial failure

```
Given: 400 nodes total, workers covered 300, 100 nodes in failed range
When:  embed_tick fires (simulated 60s interval)
Then:  embed_progress() returns (400, 300, ...)
       maybe_spawn_embed_phase2 (or phase1) is triggered
       After that reindex: embed_progress() returns (400, 400, ...)
```

### TC-11 — Stale temp files from crashed orchestrator are purged on next run

```rust
#[test]
fn stale_worker_dbs_are_deleted_before_new_run() {
    // Plant embed.w0.db and embed.w3.db in .travsr/
    // (simulating crash before merge from previous run)
    let _ = std::fs::write(travsr_dir.join("embed.w0.db"), b"stale");
    let _ = std::fs::write(travsr_dir.join("embed.w3.db"), b"stale");

    // Trigger reindex
    spawn_background_reindex_phase1(&db_path, 3);

    // Stale files must be gone before workers start
    assert!(!travsr_dir.join("embed.w0.db").exists());
    assert!(!travsr_dir.join("embed.w3.db").exists());
}
```

---

## Alternatives Considered

### A1 — Shared `embed.db` with `busy_timeout`

Set `PRAGMA busy_timeout = 30000` on each sidecar's embed.db connection. Workers
serialize on the write lock. Simpler (no merge step), but:
- `busy_timeout` is not configurable from this repo (sidecar controls its own
  connection setup)
- Write duty cycle is ~5% per worker, but collision probability grows with N:
  at N=8, expected contention is ~40% per batch
- A sidecar that does not set `busy_timeout` loses data silently — undetectable
  from this repo

**Rejected:** data-loss risk is unacceptable; requires trusting sidecar internals
we cannot verify from this repo.

### A2 — In-process embedding (skip sidecar)

Link `candle` and `sqlite-vec` into `travsr-daemon` directly via feature flag.
Eliminates IPC, enables Tokio-based async batching. Much faster per-call.

**Rejected for now:** violates RFC-018's compile-time-zero-deps principle; adds
`~548 MB` model weights to daemon RSS for all users regardless of whether embedding
is active. Deferred to a future RFC if the sidecar architecture proves insufficient.

### A3 — Single sidecar with internal thread pool

Keep one sidecar process, add `--num-threads N` flag. Sidecar manages its own
worker threads internally.

**Rejected:** `candle` model inference is not `Send + Sync` in its current form
(model state is not safely shareable across threads without Arc<Mutex<>>). Each
thread would need its own model copy, making memory usage identical to A3 but with
more complex sidecar internals.

---

## Drawbacks

1. **Merge step adds latency** — after all workers finish, a sequential merge adds
   `O(total_embeddings × 1KB / disk_bandwidth)` latency before `embed.db` is
   queryable. At 100k nodes × 1KB = 100MB merge: ~50–100 ms on NVMe. Acceptable.

2. **Temp files consume 2× disk space** during reindex — 100k nodes × 1KB × N
   workers (but each worker only holds ~25k rows) = same total as `embed.db`.
   Peak disk: `embed.db` + all temp files = 2×. On a 50 GB block volume with a
   ~100 MB embed.db, this is negligible.

3. **Stale temp files on crash** — handled by C-07. On a first-ever run with no
   `.travsr/` directory, C-07 is a no-op (glob returns empty set).

4. **Sidecar release dependency** — this RFC cannot ship until the sidecar
   implements `--row-start/--row-end`. The single-worker path remains the fallback
   when those flags are absent (backward-compatible).

---

## Unresolved Questions

1. **Worker timeout:** Should the orchestrator kill a worker that has been running
   for > T minutes? T is unclear — a large repo's Phase 1 could legitimately take
   20+ minutes. Suggest: no timeout for now; OS-level OOM killer handles runaway
   processes. Re-evaluate after first production run.

2. **Phase 2 parallelism:** This RFC targets Phase 1 (high-centrality, `shell_number
   >= 3`). Phase 2 (background, low-centrality) intentionally runs at low priority
   with one worker. Should Phase 2 also parallelize? Decision deferred — Phase 2
   throughput is not on the critical path.

3. **`available_memory_mb()` implementation:** macOS uses `sysctl hw.physmem`;
   Linux uses `/proc/meminfo MemAvailable`. We should use `available` (free +
   reclaimable) not `total`. The exact syscall is platform-specific and adds a
   build dependency on `sysinfo` or a raw `sysctl` call. Decision: use
   `sysinfo = "0.30"` (already evaluating for other uses) or accept the
   conservative `MAX_EMBED_WORKERS = 8` ceiling without a RAM guard if adding a
   dep is undesirable.

4. **Merge connection:** Should the merge connection be opened via `SqliteStore::open`
   (which runs migrations) or via a raw `rusqlite::Connection`? Prefer raw
   connection: migrations may modify the schema and should only run once (already
   done by the first sidecar that created `embed.db`). The merge connection only
   needs `ATTACH/INSERT OR REPLACE/DETACH` — no migrations.

---

## Implementation Checklist

### This repo (`travsr-plugin-host`, `travsr-daemon`, `travsr-cli`)

- [ ] `embed_catalog.rs`: `cleanup_stale_worker_dbs()`
- [ ] `embed_catalog.rs`: `derive_num_workers()` with `available_parallelism` + RAM guard
- [ ] `embed_catalog.rs`: `partition_ranges()` 
- [ ] `embed_catalog.rs`: update `spawn_background_reindex_phase1/2/all` to spawn N workers
- [ ] `embed_catalog.rs`: `wait_for_all_workers()` → `Vec<WorkerOutcome>`
- [ ] `embed_catalog.rs`: `merge_all_worker_dbs()` per C-04/C-05
- [ ] `travsr-daemon/src/lib.rs`: `MAX_EMBED_WORKERS`, `WORKER_RAM_BUDGET_MB` constants
- [ ] Tests: TC-01 through TC-11

### Sidecar (`Travsr-com/travsr-embed`) — external, blocking

- [ ] `--row-start <i64>` flag
- [ ] `--row-end <i64>` flag  
- [ ] `--batch-size <u32>` flag (targets 256)
- [ ] `--busy-timeout-ms <u32>` flag (safety net, default 5000)
- [ ] `candle-accelerate` feature flag in `Cargo.toml`
- [ ] Internal pipeline: DB-read thread + inference thread

### Release

- [ ] Both this repo and sidecar ship together (single release tag)
- [ ] `travsr embed init` downloads the new sidecar binary automatically
- [ ] Upgrade path: existing `embed.db` is not invalidated; new run adds to it
