# Plan — `get_context` Remaining PRs

> Status: Ready for implementation
> Author: Tech Lead + Solution Architect + Software Engineer (travsr personas)
> Date: 2026-06-18
> Branch: `feature/travsr-retrieval-embeddings`
> Related: RFC-012-ADDENDUM-02 (L2-B), docs/plans/get-context-enhancements.md (history)

---

## Context

`include_snippets` on `get_context` (Phase 2 from the original plan) is **fully
implemented and merged** — 5 tests green, both shared and separate budget modes
work, k-core shell boost wired in. Two deliverables remain:

| # | Deliverable | Status |
|---|---|---|
| PR-1 | Edge-relationship annotation in `get_context` output | Unblocked |
| PR-2 | L2-B ONNX embeddings — Step 4 in `search_nodes_fuzzy` | Blocked on dep pinning |

Ship PR-1 first. While PR-1 is in review, resolve the dep open questions for PR-2.

---

## PR-1 — Edge-Relationship Annotation

**Goal:** Every node returned by `get_context` gets a `[via: seed|caller|dependency|context]`
tag so the AI agent knows *why* that node was included — whether it directly
matched the query, calls one of the matched symbols, is depended on by one, or
was surfaced by PPR structural relevance alone.

### Output change (additive, backward-compatible)

```
// Before
fn:PaymentService.charge (method) — src/payments.rs [package: payments]

// After
fn:PaymentService.charge (method) — src/payments.rs [package: payments] [via: seed]
fn:PaymentProcessor.validate (method) — src/processor.rs [package: core] [via: caller]
fn:Database.begin_tx (method) — src/db.rs [package: store] [via: dependency]
fn:ConnectionPool.get (method) — src/pool.rs [package: store] [via: context]
```

### NodeRole semantics

| Role | Condition |
|---|---|
| `seed` | `node.id ∈ seeds` (direct text match, up to 5 seeds) |
| `caller` | `node → RefCall/RefImports → any seed` (this node calls a seed) |
| `dependency` | `any seed → Depends/Exports/RefCall → node` (seed uses this node) |
| `context` | PPR-only; no direct 1-hop edge to/from any seed |

Algorithm: O(S × avg_degree) — one `iter_edges_from` + `iter_edges_to` call per
seed. Seeds are already capped at 5 in `get_context_body`, so worst case is ~5 ×
avg_degree edge reads, which is negligible.

### Data flow

```
get_context_body()                        tools.rs:1409
  └─ search_nodes_fuzzy → seeds           ← exists
  └─ ppr(seeds) → ppr_scores              ← exists
  └─ knapsack(items) → selected           ← exists
  └─ store.get_node_roles_for_seeds(      ← NEW (one call, after knapsack)
       seeds, selected_ids
     ) → HashMap<NodeId, NodeRole>
  └─ format header + [via: role]          ← CHANGED (both snippet paths)
```

### Files to change

#### `crates/travsr-store/src/lib.rs`

Add method to `SqliteStore` near `get_shell_numbers_batch` (~line 1346):

```rust
/// Classify selected nodes by their 1-hop structural relationship to the seed set.
/// O(S × avg_degree) — seeds are capped at 5 so this is fast in practice.
pub fn get_node_roles_for_seeds(
    &self,
    seeds: &[NodeId],
    candidates: &[NodeId],
) -> Result<HashMap<NodeId, NodeRole>, StoreError> {
    let seed_set: HashSet<NodeId> = seeds.iter().copied().collect();
    let mut roles: HashMap<NodeId, NodeRole> = candidates
        .iter()
        .map(|&id| (id, NodeRole::Context))
        .collect();

    for &seed in seeds {
        // Forward edges from seed → mark targets as Dependency
        for edge in self.iter_edges_from(seed)? {
            if roles.contains_key(&edge.dst) {
                roles.entry(edge.dst).and_modify(|r| {
                    if *r == NodeRole::Context { *r = NodeRole::Dependency; }
                });
            }
        }
        // Reverse edges to seed → mark sources as Caller
        for edge in self.iter_edges_to(seed)? {
            if roles.contains_key(&edge.src) {
                roles.entry(edge.src).and_modify(|r| {
                    if *r == NodeRole::Context { *r = NodeRole::Caller; }
                });
            }
        }
        // Seeds override everything
        if roles.contains_key(&seed) {
            roles.insert(seed, NodeRole::Seed);
        }
    }

    Ok(roles)
}
```

#### `crates/travsr-mcp/src/tools.rs`

1. Add `NodeRole` enum above `get_context_body`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeRole { Seed, Caller, Dependency, Context }

impl NodeRole {
    fn label(self) -> &'static str {
        match self {
            NodeRole::Seed       => "seed",
            NodeRole::Caller     => "caller",
            NodeRole::Dependency => "dependency",
            NodeRole::Context    => "context",
        }
    }
}
```

2. After the knapsack call (~line 1503), extract `selected_ids` and call
   `get_node_roles_for_seeds`. Propagate gracefully — if the call errors, fall
   back to `Context` for all nodes (never crash, never block output).

3. Change the header format in **both** the `include_snippets=true` path
   (line ~1547) and the legacy `include_snippets=false` path (line ~1589):

```rust
// Before
format!("{} ({}) — {} [package: {}]", n.vname.signature, n.kind, n.vname.path, n.package)

// After
let role = roles.get(&n.id).copied().unwrap_or(NodeRole::Context).label();
format!("{} ({}) — {} [package: {}] [via: {}]",
    n.vname.signature, n.kind, n.vname.path, n.package, role)
```

4. The `repo_root`-missing degrade path (line ~1517) also gets annotations —
   pass the role map through.

### Tests (add to `tools.rs` `#[cfg(test)]` block)

- `get_context_annotation_labels_seed` — seed node gets `[via: seed]`
- `get_context_annotation_labels_caller` — node with a RefCall to the seed gets `[via: caller]`
- `get_context_annotation_labels_dependency` — node the seed Depends-on gets `[via: dependency]`
- `get_context_annotation_labels_context` — PPR-only node (no direct edge) gets `[via: context]`

### Acceptance criteria

- [ ] All 4 `[via: X]` labels appear correctly
- [ ] Legacy path (`include_snippets=false`) also annotated
- [ ] `repo_root`-missing degrade path also annotated
- [ ] Error in role-lookup falls back to `[via: context]` — never panics, never blocks output
- [ ] 4 new tests green
- [ ] Existing `get_context_include_snippets_*` tests unchanged (backward compat)

### CI checklist before commit

```
cargo fmt -p travsr-store -p travsr-mcp
cargo clippy -p travsr-store -p travsr-mcp -- -D warnings
cargo test -p travsr-store
cargo test -p travsr-mcp
cargo deny check
```

Build and exercise end-to-end with the release binary before committing.

---

## PR-2 — L2-B ONNX Embeddings

**Goal:** Add Step 4 to `search_nodes_fuzzy` — a semantic ANN fallback that fires
only when Steps 1–3 all miss. Feature-gated behind `--features embeddings`; zero
behavioral change for the default build.

### Open questions — resolve before writing any code

These must be answered before P2-A (dep pinning). Start the investigation while
PR-1 is in review.

| # | Question | Action |
|---|---|---|
| Q1 | Does `ort 2.0` link cleanly on ARM64 macOS (dev) and ARM64 Linux (OCI A1)? | `cargo add ort@2.0 --optional` on a scratch branch, `cargo build --features embeddings`, test on both platforms |
| Q2 | What is the exact `sqlite-vec` crate name and version on crates.io? Does it expose a rusqlite-compatible extension init function? | Check crates.io + the Alex Garcia sqlite-vec Rust repo |
| Q3 | Is `nomic-embed-text-v1.5` ONNX export publicly available without a Hugging Face token? | `curl -I https://huggingface.co/nomic-ai/nomic-embed-text-v1.5/resolve/main/onnx/model.onnx` |
| Q4 | RaBitQ scope — store full FLOAT[256] vectors (vec0 built-in ANN) for the first implementation, defer 1-bit quantisation? | Confirm with user; DEBT(travsr-#259) if deferred |

### Sub-phases (must land in order)

---

#### P2-A — Dep pinning + vec0 extension loader (S, ~3h)

**`crates/travsr-store/Cargo.toml`**

```toml
[features]
embeddings = ["dep:ort", "dep:sqlite-vec"]

[dependencies]
ort        = { version = "2.0", optional = true, features = ["ndarray"] }
sqlite-vec = { version = "X.Y", optional = true }   # pin after Q2 resolved
```

**`crates/travsr-store/src/lib.rs`** — in `SqliteStore::open()`, before the
migration runner, load the vec0 extension:

```rust
#[cfg(feature = "embeddings")]
{
    // Load sqlite-vec so the v12 CREATE VIRTUAL TABLE USING vec0 succeeds.
    // Safety: extension loading is an SQLite API; only called once per connection.
    sqlite_vec::load(&conn)
        .map_err(|e| StoreError::Database(format!("sqlite-vec load failed: {e}")))?;
}
```

Exact call signature depends on Q2 resolution.

**Acceptance criteria:**
- [ ] `cargo build -p travsr-store` (no features) — zero `ort`/`sqlite-vec` in dep tree
- [ ] `cargo build -p travsr-store --features embeddings` — compiles clean
- [ ] Migration v12 runs without error with embeddings feature on

---

#### P2-B — Store write path: model + maintenance hooks (M, ~1 day)

**New file: `crates/travsr-store/src/embeddings.rs`** (entire file `#[cfg(feature = "embeddings")]`)

Key pieces:

```rust
// Global model — loads once per process, lazy-initialised on first embed call.
static MODEL: OnceLock<ort::Session> = OnceLock::new();

pub fn init_model(model_path: &Path) -> Result<(), StoreError> { ... }

/// Embed text → MRL-256 float vector (first 256 dims of the model output).
fn embed_text(text: &str) -> Result<[f32; 256], StoreError> { ... }

pub fn put_node_embedding(
    conn: &Connection,
    node_id: NodeId,
    sig: &str,
    kind: &str,
    path: &str,
) -> Result<(), StoreError> {
    let text = format!("{sig} {kind} {path}");
    let vec = embed_text(&text)?;
    conn.execute(
        "INSERT OR REPLACE INTO node_embeddings(node_id, embedding) VALUES (?1, ?2)",
        (node_id.0, vec.as_slice()),
    )?;
    Ok(())
}

pub fn delete_node_embedding(conn: &Connection, node_id: NodeId) -> Result<(), StoreError> { ... }
```

**`crates/travsr-store/src/lib.rs`** — wire under `#[cfg(feature = "embeddings")]`:
- `put_node_fts` → call `put_node_embedding` after FTS insert
- `delete_node_fts` → call `delete_node_embedding`
- `delete_nodes_for_path` → call `delete_node_embedding` for each deleted node
- `delete_nodes_for_path_prefix` → same

**Acceptance criteria:**
- [ ] `put_node_fts` with feature on writes a row to `node_embeddings`
- [ ] Deleting a node via any path removes its `node_embeddings` row
- [ ] `node_embeddings` row count == `nodes` row count after a full index

---

#### P2-C — Store read path: Step 4 in `search_nodes_fuzzy` (M, ~1 day)

**`crates/travsr-store/src/embeddings.rs`** — add:

```rust
/// ANN search via vec0 KNN. Returns NodeIds ordered by distance (closest first).
pub fn vec_search(
    conn: &Connection,
    query: &str,
    k: usize,
) -> Result<Vec<NodeId>, StoreError> {
    let query_vec = embed_text(query)?;
    let sql = "SELECT node_id FROM node_embeddings
               WHERE embedding MATCH ?1 AND k = ?2";
    // bind query_vec as blob, k as integer; collect node_ids
}
```

**`crates/travsr-store/src/lib.rs`** — append Step 4 at the end of
`search_nodes_fuzzy` (~line 2280, after Step 3's `map_err` return):

```rust
// Step 4 — L2-B semantic ANN (opt-in, RFC-012 A2 F2).
// Only fires when Steps 1–3 combined return empty.
#[cfg(feature = "embeddings")]
{
    let ids = crate::embeddings::vec_search(&self.conn, query, 20)
        .map_err(|e| StoreError::Database(e.to_string()))?;
    if !ids.is_empty() {
        tracing::debug!(layer = "vec_ann", nodes_returned = ids.len());
        return self.get_nodes(&ids);
    }
}
```

**Acceptance criteria:**
- [ ] Step 4 does not fire when Steps 1-3 return results (verified via tracing)
- [ ] Step 4 returns nodes for a query with no text match but semantic overlap
- [ ] Zero behavioral change for default builds (no-feature)

---

#### P2-D — `travsr embed` CLI subcommand (L, ~1.5 days)

**New file: `crates/travsr-cli/src/embed.rs`**

Subcommands:
- `travsr embed init [--model <path>]`
- `travsr embed status`

`init` logic:
1. Resolve model path: `--model` arg or `~/.travsr/models/nomic-embed-text-v1.5.onnx`
2. If model file missing: download with progress bar (URL from Q3)
3. Call `embeddings::init_model(&model_path)?`
4. Open store; check meta `rabitq_rotation_seed` — if missing, generate 32-byte
   random seed via `rand::thread_rng()`, hex-encode, `store.set_meta("rabitq_rotation_seed", &hex)?`
5. Idempotency: if `node_embeddings` count == `nodes` count → print
   "Already fully indexed ({n} nodes)" and return `Ok(())`
6. Batch-embed all nodes in chunks of 100:
   `SELECT id, signature, kind, path FROM nodes` → `put_node_embedding` per node
   → progress bar via existing `crates/travsr-cli/src/progress.rs`

`status` logic: show `node_embeddings` count / `nodes` count, model path, first 8 chars of seed.

**`crates/travsr-cli/src/main.rs`** — wire `embed` subcommand under
`#[cfg(feature = "embeddings")]`; gate the CLI arg at parse time so the subcommand
doesn't appear in `--help` for default builds.

**Acceptance criteria:**
- [ ] `travsr embed init` runs end-to-end on a real indexed repo
- [ ] Second `travsr embed init` is a no-op with "Already fully indexed" output
- [ ] `travsr embed status` shows correct row counts and seed fingerprint
- [ ] Missing model triggers download (or a clear error if network unavailable)
- [ ] `rabitq_rotation_seed` is stable across two consecutive `init` calls on the same store

---

#### P2-E — CI gate (XS, ~1h)

**`.github/workflows/ci.yml`** — add step to the existing TL2 gate job:

```yaml
- name: TL2 gate — zero embedding deps in default build
  run: |
    cargo metadata --no-deps --format-version 1 \
      | python3 -c "
import sys, json
meta = json.load(sys.stdin)
deps = {d['name'] for pkg in meta['packages'] for d in pkg.get('dependencies', [])}
assert 'ort' not in deps and 'sqlite-vec' not in deps, \
  f'Embedding dep leaked into default build: {deps & {\"ort\",\"sqlite-vec\"}}'
"
```

**Acceptance criteria:**
- [ ] CI fails if `ort` or `sqlite-vec` appear in the default build dep tree

---

### Full CI checklist before each sub-phase commit

```
cargo fmt -p travsr-store -p travsr-cli
cargo clippy -p travsr-store -p travsr-cli -- -D warnings
cargo test -p travsr-store --features embeddings
cargo test -p travsr-store                            # no-feature parity
cargo test -p travsr-cli   --features embeddings
cargo deny check
```

Build release binary with `--features embeddings` and run `travsr embed init`
on a real indexed repo before committing P2-D.

---

## Delivery sequence

```
[now]          PR-1: edge annotation           1.5d  no blockers
               ├── investigate Q1–Q4 in parallel
               ↓
[after PR-1]   P2-A: dep pinning + loader      3h    Q1+Q2 resolved
               ↓
               P2-B: write path (model + hooks) 1d
               ↓
               P2-C: read path (Step 4)         1d
               ↓
               P2-D: travsr embed init          1.5d  Q3+Q4 resolved
               ↓
               P2-E: CI gate                   1h
               ↓
[~6d total]    PR-2: L2-B embeddings           squash-merge
```

## Effort summary

| PR | Phases | Effort | Crates touched |
|---|---|---|---|
| PR-1 | Edge annotation | ~1.5d | `travsr-store`, `travsr-mcp` |
| PR-2 | P2-A through P2-E | ~4.5d + dep unknowns | `travsr-store`, `travsr-cli`, `.github/workflows` |
