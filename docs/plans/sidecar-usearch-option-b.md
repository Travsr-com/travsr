# Plan — travsr-embed-nomic: Replace hnsw_rs with usearch (Option B)

> Status: Draft for sign-off
> Author: Tech Lead + Senior Software Engineer (travsr personas)
> Date: 2026-06-19
> Repo: travsr-embed (sidecar binary, separate from main travsr workspace)
> Related: RFC-018 (embedding plugin architecture), docs/plans/issue-323-read-path-perf-audit.md

---

## 1. Why

The current `hnsw_rs` in-memory HNSW in `src/index.rs` has three structural problems that
make the sidecar unusable on any local machine for non-trivial repos:

| Problem | Root cause | Impact |
|---|---|---|
| Full dataset in RAM always | `load_embeddings` materialises every BLOB before first insert | 6.8 GB at 500k nodes, OOM at ~800k |
| Full rebuild on every process start | No persistence — hnsw_rs index lives in heap only | Seconds of blocking on first KNN call |
| New DB connection on every KNN query | `maybe_rebuild()` calls `Connection::open()` per call | 2–10 ms latency tax on every query |

`usearch 2.25.3` (Unum's HNSW) solves all three: the index is a single file on disk,
loaded via `mmap` so only pages touched during a search land in RAM. It also supports
`index.add()` for incremental inserts so the write path can update the index in-place
without a full rebuild.

The C++ build dependency (usearch is a C++ library with Rust bindings) is a non-issue
because the sidecar is distributed as a pre-built binary via `travsr embed init`. End
users never compile it; only CI does.

---

## 2. What changes and what does not

### Does not change
- `node_embeddings` SQLite schema (v16) — untouched
- IPC protocol — `EmbedPluginRequest` / `EmbedPluginResponse` — untouched
- `embed_batch` handler — untouched
- ONNX inference pipeline in `src/model.rs` — untouched (minus two minor fixes bundled in)
- `travsr-plugin-protocol`, `travsr-plugin-sdk`, `travsr-store` in the main repo — untouched
- Binary distribution mechanism (`travsr embed init` download) — untouched

### Changes entirely
- `Cargo.toml` — remove `hnsw_rs`, add `usearch`
- `src/index.rs` — full rewrite; `VecIndex` now wraps `usearch::Index`
- `src/main.rs` — `reindex()` write path + `NomicPlugin` struct + `knn_impl()`

### Minor fixes bundled
- `src/model.rs` — remove `attn_mask_flat.clone()` (unnecessary allocation)
- `src/model.rs` — set ORT intra-op thread count explicitly
- `src/main.rs` — move `conn.prepare(INSERT)` outside chunk loop
- `src/main.rs` — open connection with WAL + `synchronous=NORMAL` pragmas
- `src/main.rs` — increase `MAX_BATCH` from 32 → 64

---

## 3. Index file

**Location:** `~/.travsr/models/nomic-v1.5-int8/hnsw.usearch`

Sits alongside `model_int8.onnx` and `tokenizer.json` in the model directory that
`model_dir()` already resolves. Model-scoped: switching backends writes to a different
directory under `~/.travsr/models/<backend-id>/`.

**Lifecycle:**

```
First run (no index file yet):
  travsr embed reindex
    → writes BLOBs to node_embeddings
    → creates hnsw.usearch from scratch
    → saves file

Subsequent reindex (incremental):
  travsr embed reindex
    → writes new BLOBs to node_embeddings
    → loads hnsw.usearch, calls index.add() per new node
    → saves updated file

Daemon startup (IPC mode):
  NomicPlugin::load()
    → mmaps hnsw.usearch  (microseconds, no data read yet)
    → ready to serve KNN immediately

Daemon running, background reindex fires:
  --reindex updates hnsw.usearch on disk
  Next KNN call: stat(hnsw.usearch).mtime > last_loaded → reload (mmap again)

Index file missing or corrupted:
  Daemon KNN returns empty with a tracing::warn — user must run travsr embed reindex
  --reindex always rebuilds from node_embeddings regardless
```

**usearch construction parameters:**

```rust
IndexOptions {
    dimensions:       256,             // MRL-256 matches model output
    metric:           MetricKind::Cos, // vectors are L2-normalised → cosine = dot product
    quantization:     ScalarKind::F32, // full precision in index file
    connectivity:     16,              // M — graph fan-out per node
    expansion_add:    128,             // ef_construction (was 400 in hnsw_rs — halved)
    expansion_search: 64,              // ef_search (dynamic; callers can override)
    ..Default::default()
}
```

`expansion_add = 128` replaces the `EF_CONSTRUCTION = 400` from hnsw_rs. Standard
production value; saves ~2× build time with no meaningful recall difference at 256-dim
cosine for code symbols.

---

## 4. Work items

### Step 1 — `Cargo.toml` (XS)

```toml
# Remove
hnsw_rs = "0.3"

# Add
usearch = "2.25.3"
```

No other dependency changes. `rusqlite`, `ort`, `tokenizers`, `dirs`, `anyhow`,
`tracing`, `tracing-subscriber` all stay as-is.

---

### Step 2 — `src/index.rs` — full rewrite (M)

Replace the entire file. New public surface:

```rust
pub struct VecIndex { ... }   // wraps usearch::Index

impl VecIndex {
    /// Load from file (mmap). Falls back to None if file does not exist yet.
    /// Call from NomicPlugin::load() at daemon startup.
    pub fn try_load(index_path: &Path) -> Result<Option<Self>>

    /// Full rebuild from node_embeddings in graph.db.
    /// Called by --reindex when the index file does not yet exist.
    /// Streams rows — does not materialise all BLOBs at once.
    pub fn build_from_db(
        db_path: &Path,
        model_id: &str,
        index_path: &Path,
        expected_count: usize,
    ) -> Result<Self>

    /// Incremental add. Called per-node during --reindex write loop.
    /// node_id: the i64 from nodes.id cast to u64 (safe — SQLite ids are positive)
    /// vec: 256 f32 values (already decoded from blob by caller)
    pub fn add(&self, node_id: i64, vec: &[f32]) -> Result<()>

    /// Persist current index to self.index_path.
    pub fn save(&self) -> Result<()>

    /// KNN search. Decodes query_blob → f32, checks mtime staleness, searches.
    /// Returns up to k (node_id, cosine_similarity) pairs.
    pub fn knn(&mut self, query_blob: &[u8], k: u32) -> Result<Vec<(i64, f32)>>

    pub fn count(&self) -> usize
}
```

**Removed entirely from old `index.rs`:**
- `load_embeddings()` — full-materialisation SELECT replaced by streaming in `build_from_db`
- `maybe_rebuild()` — replaced by mtime check inside `knn()`
- `REBUILD_THRESHOLD` constant
- `on_disk_count` field
- `db_path` field on struct (index is self-contained; DB path no longer needed)
- `hnsw_rs` imports

**Staleness detection inside `knn()`:**

```rust
// Cheap stat() — no DB connection, no COUNT(*)
let mtime = std::fs::metadata(&self.index_path)?.modified()?;
if mtime > self.last_modified {
    tracing::info!("index file updated — reloading");
    *self = Self::try_load(&self.index_path)?
        .ok_or_else(|| anyhow::anyhow!("index file vanished after mtime change"))?;
}
```

One `stat()` syscall per KNN call (~200 ns) replaces the current `Connection::open()`
+ `SELECT COUNT(*)` + `drop(conn)` per KNN call (~5 ms).

**`build_from_db` streaming pattern (no full materialisation):**

```rust
pub fn build_from_db(db_path, model_id, index_path, expected_count) -> Result<Self> {
    let conn = Connection::open(db_path)?;
    // count for reserve()
    let n: usize = conn.query_row(
        "SELECT COUNT(*) FROM node_embeddings WHERE model_id = ?1",
        [model_id], |r| r.get(0)
    ).unwrap_or(expected_count);

    let options = IndexOptions { dimensions: 256, metric: MetricKind::Cos, ... };
    let index = Index::new(&options)?;
    index.reserve(n)?;

    let mut stmt = conn.prepare(
        "SELECT node_id, embedding FROM node_embeddings WHERE model_id = ?1"
    )?;
    let mut rows = stmt.query([model_id])?;
    while let Some(row) = rows.next()? {
        let node_id: i64 = row.get(0)?;
        let blob: Vec<u8> = row.get(1)?;
        let vec = blob_to_f32(&blob);          // decode 1024 bytes → 256 f32
        index.add(node_id as u64, &vec)?;     // usearch internal clone — blob dropped
        // blob is dropped here, never accumulates in a Vec
    }

    let last_modified = save_and_stat(&index, index_path)?;
    Ok(Self { inner: index, index_path: index_path.to_path_buf(), last_modified })
}
```

Peak RAM during `build_from_db`: one row's BLOB (1,024 bytes) + usearch internal vector
copy (1,024 bytes) at a time. The old `load_embeddings` held all N BLOBs simultaneously.

---

### Step 3 — `src/main.rs` — `reindex()` write path (S)

**3a — Connection pragmas (trivial)**

```rust
// Before
let conn = Connection::open(db_path).context("open graph.db")?;

// After
let conn = Connection::open(db_path).context("open graph.db")?;
conn.execute_batch(
    "PRAGMA journal_mode = WAL;
     PRAGMA synchronous = NORMAL;
     PRAGMA cache_size = -16384;"   // 16 MB page cache
).context("configure SQLite pragmas")?;
```

**3b — `conn.prepare(INSERT)` outside the loop (trivial)**

```rust
// Before: conn.prepare() called once per chunk inside the loop

// After: prepare once, reuse across all chunks
let mut ins = conn.prepare(
    "INSERT OR REPLACE INTO node_embeddings (node_id, model_id, embedding) \
     VALUES (?1, ?2, ?3)",
)?;
```

**3c — Transaction strategy (one per 5k rows instead of one per 32 rows)**

```rust
// Before: BEGIN / COMMIT wrapping every 32-row chunk = 313 fsyncs for 10k nodes
// After: one transaction per 5k rows = 2 fsyncs for 10k nodes

const TX_BATCH: usize = 5_000;
let mut tx_count = 0usize;
conn.execute("BEGIN", [])?;

for chunk in pending.chunks(MAX_BATCH) {
    // ... embed, insert, add to index ...
    tx_count += chunk.len();
    if tx_count >= TX_BATCH {
        conn.execute("COMMIT", [])?;
        conn.execute("BEGIN", [])?;
        tx_count = 0;
    }
}
conn.execute("COMMIT", [])?;
```

**3d — usearch index update alongside SQLite write (core of Option B)**

Open or create the index before the loop. Add each node's vector immediately after its
BLOB is written to SQLite. Save once after all chunks complete.

```rust
fn reindex(model_dir: &Path, db_path: &Path) -> Result<()> {
    let model = model::NomicModel::load(model_dir)?;
    let conn = Connection::open(db_path)?;
    // pragmas (3a)

    // --- find pending nodes (unchanged) ---
    let mut stmt = conn.prepare("SELECT n.id, n.kind, n.signature FROM nodes n \
         WHERE NOT EXISTS (SELECT 1 FROM node_embeddings e \
         WHERE e.node_id = n.id AND e.model_id = ?1)")?;
    let pending: Vec<(i64, String, String)> = stmt
        .query_map([MODEL_ID], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .filter_map(|r| r.ok()).collect();

    let total = pending.len();
    if total == 0 {
        println!("All nodes already embedded for {MODEL_ID}.");
        return Ok(());
    }

    // --- open or create usearch index ---
    let index_path = model_dir.join("hnsw.usearch");
    let mut index = if index_path.exists() {
        VecIndex::try_load(&index_path)?
            .expect("index file exists but failed to load")
    } else {
        VecIndex::new_empty(&index_path, total)?   // creates usearch::Index, reserves capacity
    };

    // --- prepare INSERT once (3b) ---
    let mut ins = conn.prepare(
        "INSERT OR REPLACE INTO node_embeddings (node_id, model_id, embedding) \
         VALUES (?1, ?2, ?3)"
    )?;

    // --- write loop ---
    let mut inserted = 0usize;
    let mut tx_count = 0usize;
    conn.execute("BEGIN", [])?;

    for chunk in pending.chunks(MAX_BATCH) {
        let texts: Vec<String> = chunk.iter()
            .map(|(_, kind, sig)| format!("{kind}: {sig}")).collect();
        let text_refs: Vec<&str> = texts.iter().map(String::as_str).collect();

        let blobs = model.embed_documents(&text_refs)?;

        for ((node_id, _, _), blob) in chunk.iter().zip(blobs.iter()) {
            ins.execute(rusqlite::params![node_id, MODEL_ID, blob])?;
            let vec = model::blob_to_f32(blob);
            index.add(*node_id, &vec)?;            // O(log N), microseconds per node
            tx_count += 1;
        }

        // Commit every TX_BATCH rows (3c)
        if tx_count >= TX_BATCH {
            conn.execute("COMMIT", [])?;
            conn.execute("BEGIN", [])?;
            tx_count = 0;
        }

        inserted += chunk.len();
        if inserted % 1000 == 0 || inserted == total {
            println!("  embedded {inserted}/{total}");
        }
    }

    conn.execute("COMMIT", [])?;
    index.save()?;                                 // one file write at the end
    println!("Done — {inserted} nodes embedded. Index saved to {}.", index_path.display());
    Ok(())
}
```

**3e — `MAX_BATCH` increase**

```rust
// Before
const MAX_BATCH: u32 = 32;

// After
const MAX_BATCH: u32 = 64;
```

Conservative increase. The ONNX output tensor at batch=64, worst-case seq=512:
`64 × 512 × 768 × 4 B = 134 MB` — within local machine limits. Token-budget batching
is deferred as a follow-up (tracked separately).

---

### Step 4 — `src/main.rs` — `NomicPlugin` + `knn_impl()` (S)

**New struct:**

```rust
struct NomicPlugin {
    model: model::NomicModel,
    index: Mutex<Option<VecIndex>>,
    index_path: PathBuf,
}
```

**Updated `NomicPlugin::load()`:**

```rust
fn load(model_dir: &Path) -> Result<Self> {
    let model = model::NomicModel::load(model_dir)?;
    let index_path = model_dir.join("hnsw.usearch");
    let index = VecIndex::try_load(&index_path)
        .unwrap_or_else(|e| {
            tracing::warn!("could not load HNSW index: {e:#} — KNN disabled until `travsr embed reindex` runs");
            None
        });
    Ok(Self {
        model,
        index: Mutex::new(index),
        index_path,
    })
}
```

**Updated `knn_impl()`:**

```rust
fn knn_impl(&self, req: &KnnRequest) -> Result<(Vec<i64>, Vec<f32>)> {
    let query_blob = self.model.embed_query(&req.query_text)?;

    let mut guard = self.index.lock()
        .map_err(|_| anyhow::anyhow!("index mutex poisoned"))?;

    // Try to load the index if it wasn't present at startup
    // (user ran `travsr embed reindex` after the daemon started)
    if guard.is_none() && self.index_path.exists() {
        *guard = VecIndex::try_load(&self.index_path)?;
    }

    let idx = match guard.as_mut() {
        None => {
            tracing::debug!("no HNSW index — run `travsr embed reindex`");
            return Ok((vec![], vec![]));
        }
        Some(i) => i,
    };

    // mtime-based staleness (inside knn() itself — one stat() syscall)
    let raw = idx.knn(&query_blob, req.k)?;
    let ids: Vec<i64> = raw.iter().map(|&(id, _)| id).collect();
    let scores: Vec<f32> = raw.iter().map(|&(_, d)| (1.0 - d).clamp(0.0, 1.0)).collect();
    Ok((ids, scores))
}
```

**What is gone from `knn_impl`:**
- `if guard.is_none() { VecIndex::build(...) }` — lazy build removed entirely
- Passing `req.db_path` anywhere — the index is self-contained on disk

---

### Step 5 — `src/model.rs` — minor bundled fixes (XS)

**5a — Remove `attn_mask_flat.clone()`**

The clone exists because `attn_mask_flat` is moved into the mask tensor but also used
later for mean pooling. Fix: keep a reference to the mask data before constructing the
tensor.

```rust
// Before
let mask_tensor = Tensor::from_array(
    ([batch, seq], attn_mask_flat.clone().into_boxed_slice())  // clone
)?;
// ... later uses attn_mask_flat for pooling

// After
let attn_mask_box = attn_mask_flat.clone().into_boxed_slice();  // still need clone
// BUT: restructure pooling to use a separate count derived during encoding
// rather than scanning attn_mask_flat post-hoc

// Actually simplest fix: compute n_real per encoding before building tensors
let n_real_per_item: Vec<usize> = encodings.iter()
    .map(|e| e.get_attention_mask().iter().filter(|&&m| m == 1).count().max(1))
    .collect();

// Then build mask tensor (move attn_mask_flat into it — no clone)
let mask_tensor = Tensor::from_array(
    ([batch, seq], attn_mask_flat.into_boxed_slice())   // no clone
)?;

// Use n_real_per_item[i] instead of scanning attn_mask_flat during pooling
```

**5b — ORT intra-op thread count**

```rust
// Before
let session = Session::builder()
    .context("ORT session builder")?
    .commit_from_file(model_dir.join("model_int8.onnx"))?;

// After
let parallelism = std::thread::available_parallelism()
    .map(|n| n.get())
    .unwrap_or(4);

let session = Session::builder()
    .context("ORT session builder")?
    .with_intra_threads(parallelism)
    .context("setting intra-op thread count")?
    .commit_from_file(model_dir.join("model_int8.onnx"))?;
```

No new dependency — `std::thread::available_parallelism` is stable since Rust 1.59.

---

## 5. RAM profile after this plan

| N nodes | Build peak (first reindex) | Steady-state (daemon) | Per-query working set |
|---|---|---|---|
| 50k | ~52 MB | ~52 MB (mmap, OS-managed) | ~14 MB page cache |
| 200k | ~206 MB | ~206 MB | ~14 MB |
| 500k | ~515 MB | ~515 MB | ~14 MB |
| 5M | ~5.1 GB | ~5.1 GB | ~18 MB |

"Build peak" = usearch index file size (all vectors on disk, usearch holds them in the
mmap). The process heap stays at ~90 MB (model) + ~10 MB (runtime). The `mmap` pages
are OS-managed: if another process needs RAM, unused index pages are evicted silently
and faulted back on next access.

Compare to current hnsw_rs: 12.1 GB process heap peak at 5M nodes (OOM before build
completes on any local machine).

---

## 6. Query latency profile

```
usearch HNSW KNN (EF_SEARCH=64, 256-dim cosine):
  N=50k:    < 1 ms
  N=200k:   < 1 ms
  N=500k:   ~ 1 ms
  N=5M:     ~ 3 ms

Plus per-query overhead:
  stat() for mtime check:          ~200 ns
  embed_query() ONNX inference:    ~5–15 ms (dominant cost, unchanged)
  blob_to_f32() decode:            ~1 µs
```

The ONNX inference is the bottleneck in all cases. The HNSW search itself is negligible.

---

## 7. Files changed

| File | Change type | Steps |
|---|---|---|
| `Cargo.toml` | Dep swap | 1 |
| `src/index.rs` | Full rewrite | 2 |
| `src/main.rs` | Rewrite reindex() + NomicPlugin | 3, 4 |
| `src/model.rs` | Minor fixes | 5 |

No changes to the main `travsr` workspace. No protocol changes.

---

## 8. Implementation order

1. `Cargo.toml` — swap deps. Verify `cargo build` compiles the usearch C++ layer.
2. `src/index.rs` — write `VecIndex` against usearch API. Add a `#[cfg(test)]` that
   creates an index, inserts 100 synthetic 256-dim vectors, saves, loads, and asserts
   KNN returns the correct nearest neighbour.
3. `src/main.rs` — update `reindex()`. Test locally: run `--reindex` against a real
   graph.db, verify `hnsw.usearch` is created and non-empty.
4. `src/main.rs` — update `NomicPlugin` + `knn_impl`. Start in daemon mode, fire a
   KNN request via the IPC protocol, verify results match a brute-force ground-truth
   check against the same graph.db.
5. `src/model.rs` — bundled fixes. Verify `cargo clippy` clean.
6. End-to-end: `travsr embed init` → `travsr embed reindex` → start daemon → query
   via `get_context`. Verify non-empty KNN results and no spike on first query.

---

## 9. Deferred (not in this plan)

| Item | Why deferred |
|---|---|
| Token-budget batching (replace fixed MAX_BATCH) | Needs measurement of actual signature lengths in real repos; current 64 is safe |
| `WITHOUT ROWID` fix on `node_embeddings` | v17 migration in main travsr repo, separate PR |
| Covering index `(node_id, model_id)` on `node_embeddings` | Same v17 migration |
| Stream `pending` nodes via cursor (avoid collect) | Low impact once TX batch is fixed; defer |
| usearch i8 quantization for index file | ~4× smaller file, ~0.5% recall tradeoff; measure first |
