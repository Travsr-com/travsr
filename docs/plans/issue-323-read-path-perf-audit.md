# Issue #323 — Read-Path Perf Audit Implementation Plan

> **Superseded in part (#457):** Kùzu was dropped as a storage backend; SQLite+WAL is the only backend. The Kùzu migration-path notes below are kept for historical context. See ADR-018.

> **Status:** Draft — awaiting sign-off
> **Author:** Engineering (Tech Lead + Senior SWE personas)
> **Date:** 2026-06-15
> **Branch:** `feature/travsr-store-323-read-path-perf`
> **Crates affected:** `travsr-mcp`, `travsr-store`, `travsr-cli`, `travsr-daemon`
> **Issue:** https://github.com/Travsr-com/travsr/issues/323
> **Labels:** enhancement, debt, retrieval, performance

---

## 1. Goal

Eliminate seven read-path bottlenecks identified in the SQLite store audit so that
core MCP tools (`get_callers`, `get_dependencies`, `get_blast_radius`,
`get_execution_path`, `get_context`) hold their p95 latency budgets at MVP scale
(75M nodes / 1.5B edges) **and** degrade gracefully toward the production
(Kùzu / OCI A1) target.

This is a **read-path-only** change set. No graph semantics, edge construction, or
node identity changes. Algorithms-first principle is preserved: every fix is a
storage/query/transport optimization — none touch how edges or relationships are
determined.

---

## 2. Ground-Truth Verification (done before planning)

The issue was filed 2026-06-12; the codebase has moved since. Verified against
`master @ a659495`:

| Claim in issue | Verified state | Note |
|---|---|---|
| `Store` reads `get_node/get_nodes/iter_edges_*` | `lib.rs:285–307` — all take **`&self`** | Constrains R5/R6 design (see §4) |
| `get_nodes(&[NodeId])` batch API exists | `lib.rs:307, 2588` | R1 reuses it directly |
| `get_context` already batches | `tools.rs:1363` (`store.get_nodes`) | Reference pattern for R1 |
| N+1 in core tools | Confirmed `tools.rs:82,124,208,541,605,~1961,~1994,~2023,~2043` | **Line numbers drifted** from issue — key on the *pattern*, not the line |
| `idx_edges_dst_kind` non-covering | `v1_initial.sql:27` | R2 target |
| `idx_edges_src_kind_cov` covering exists | `v4_edges_src_kind_idx.sql:14` | R2 symmetric counterpart |
| Single `conn: Connection` | `lib.rs:336` | R5 target |
| `cache_size = -65536` (64 MB) | `lib.rs:465` (write), `lib.rs:400` (read-only) | R4 — **two** sites |
| `mmap_size = 0` | `lib.rs:478` (write), `lib.rs:404` (read-only) | R7 |
| `lru` crate dependency | **Absent** from workspace | R6 needs a new, vetted dep |
| `~/.config/travsr/config.toml` loader | **Does not exist** | R4/R7 assume infra that isn't there — see §4.4 |
| `open_read_only()` already sets `query_only=ON` | `lib.rs:384–420` | Partially pre-implements R5's read connection |

> **Rule for implementers:** re-grep the N+1 sites at edit time (`grep -n
> "store.get_node(" crates/travsr-mcp/src/tools.rs`). Do not trust the line
> numbers in the issue or this doc.

---

## 3. Design Decisions (resolve before coding)

### D1 — R4/R7 config surface: env var now, `config.toml` deferred
The issue's snippets reference `config.store.cache_size_mb` from
`~/.config/travsr/config.toml`. **No config-file loader exists today** — the
project uses env vars exclusively (`TRAVSR_*`, see `cli/install.rs`,
`cli/lang.rs`). Introducing a TOML config format is its own scope (schema,
precedence, discovery, docs, tests) and should not be smuggled into a perf PR.

**Decision:** expose R4 and R7 as env-var overrides consistent with the existing
pattern, with sane defaults baked in:
- `TRAVSR_STORE_CACHE_MB` (default `128`) → R4
- `TRAVSR_STORE_MMAP_GB`  (default `0`)   → R7

A future `config.toml` (tracked as a separate issue) can layer on top without
breaking these. This keeps the PR small and avoids premature config infra.

### D2 — R5 reader/writer split: scoped to the daemon, not the `Store` trait
The `Store` trait already separates reads (`&self`) from writes (`&mut self`),
and `open_read_only()` already produces a `query_only=ON` snapshot connection.
A full dual-connection `SqliteStore { writer, reader }` struct change ripples
through every caller and every `&mut self`/`&self` boundary in 4 crates.

**Decision:** Do **not** rewrite `SqliteStore` to hold two connections in this
issue. The cleaner, lower-risk realization of R5 is: the **daemon** opens one
writable `SqliteStore` for the git-hook/index path and a separate
`open_read_only()` `SqliteStore` for the MCP query path — two connections to the
same WAL file, exactly what WAL concurrency wants. Verify/whether this separation
already exists in the daemon wiring; if the MCP server currently shares the
writable handle, switch it to `open_read_only()`. This delivers R5's benefit
(reads never block on hook writes) with near-zero blast radius.
R5 is therefore **tiered MEDIUM-but-mostly-wiring**, not a struct refactor.

### D3 — R6 LRU under `&self`: interior mutability or skip for now
The issue's snippet uses `iter_edges_from(&mut self, …)`, but the trait method is
`&self`. An adjacency cache on `SqliteStore` must therefore use interior
mutability: `Mutex<LruCache<NodeId, Arc<[Edge]>>>` (single `Mutex` lock per
lookup; `parking_lot::Mutex` preferred — already common in the tree). Cache stores
`Arc<[Edge]>` (not `Vec`) so clones are pointer-bumps. Invalidation hooks into the
existing `&mut self` write methods (`write_file_graphs_batch`,
`delete_nodes_for_path*`, `put_edge*`) — under the single-writer model a coarse
`clear()` on any write is correct and simplest; per-node eviction is a later
refinement.

**Decision:** R6 is **gated on profiling** (see §6 Phase 4). Land R1–R5 first,
measure hot-node reuse with the bench harness, and only add the LRU if the data
shows repeated `iter_edges_*` on the same `NodeId` within a query. Ship it behind
the measurement, not ahead of it. This matches the issue's own recommended order.

### D4 — Migration is additive and online-safe
R2 adds an index and drops a now-redundant one. `CREATE INDEX` on a 1.5B-row
`edges` table is **not** instant at production scale. The migration must be
idempotent (`IF NOT EXISTS` / `IF EXISTS`) and the `DROP` must come **after** the
`CREATE` so a crash mid-migration never leaves the reverse direction unindexed.

---

## 4. The Seven Fixes — Detailed Design

### R2 — Covering reverse index (HIGH, ship first: zero app-code risk)
New migration `v14_covering_reverse_idx.sql`:
```sql
-- Symmetric counterpart to v4 idx_edges_src_kind_cov.
-- Covers: SELECT src FROM edges WHERE dst=?1 AND kind=?2  (index-only scan)
-- Used by every get_callers / get_blast_radius reverse traversal.
CREATE INDEX IF NOT EXISTS idx_edges_dst_kind_cov ON edges(dst, kind, src);
DROP   INDEX IF EXISTS idx_edges_dst_kind;
```
- Register in the migration runner; bump `latest_version()` → v14.
- `open_read_only()` asserts `current == latest`, so this auto-forces a writable
  reopen on existing DBs — correct, just note it in the changelog (first query
  after upgrade triggers one migration pass).
- **Test:** assert `EXPLAIN QUERY PLAN SELECT src FROM edges WHERE dst=? AND
  kind=?` reports `USING COVERING INDEX idx_edges_dst_kind_cov`.

### R1 — Kill N+1 `get_node` loops (CRITICAL)
Apply the `get_context` batch pattern (`tools.rs:1363`) to all six tools. Shape:
```rust
// Collect the node ids the loop needs, batch-fetch once, index by id.
let ids: Vec<NodeId> = edges.iter().map(|e| e.src).collect();
let node_map: HashMap<NodeId, Node> = store
    .get_nodes(&ids)?
    .into_iter()
    .map(|n| (n.id, n))
    .collect();
for edge in &edges {
    let Some(node) = node_map.get(&edge.src) else { continue };
    // ...
}
```
Sites (re-verify lines at edit time):
- `get_dependencies_transitive` — `iter_edges_from` → `get_node(edge.dst)`
- `get_dependencies` — same
- `get_callers` — `iter_edges_to` → `get_node(edge.src)`
- `get_blast_radius` (main + import path) — `iter_edges_to` → `get_node(edge.src)`
- `get_execution_path` — triple-nested; batch each level's ids before descending.

`get_execution_path` is the highest-value and trickiest: collect ids per BFS
level, batch-fetch, then expand — do **not** fetch inside the innermost loop.
Preserve existing ordering/dedup semantics (use `IndexMap` or sort if the current
output order is asserted by tests).

**Behavior must be identical** — this is a pure latency fix. Snapshot-test each
tool's output before/after on a fixture repo to prove byte-identical results.

### R3 — `PRAGMA optimize` so the planner has real cardinalities (HIGH)
```rust
// travsr-store: new pub fn on SqliteStore
pub fn run_pragma_optimize(&self) -> Result<(), StoreError> {
    self.conn
        .execute_batch("PRAGMA analysis_limit = 1000; PRAGMA optimize;")
        .map_err(/* → StoreError::Database */)?;
    Ok(())
}
```
Call sites:
1. End of `SqliteStore::open()` (after migrations + backfills) — cheap, runs only
   on stale stats.
2. End of a full `travsr init` in the daemon (after the bulk write completes) so
   the first real query sees populated `sqlite_stat1`.
- Guard: `analysis_limit = 1000` bounds the cost; safe on the hook path but we
  only call it post-init, not per-commit.

### R4 — Configurable `cache_size`, default 128 MB (MEDIUM)
Replace the two hard-coded `-65536` sites (`configure()` write path and
`open_read_only()`):
```rust
fn cache_size_kib() -> i64 {
    std::env::var("TRAVSR_STORE_CACHE_MB")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|mb| *mb > 0)
        .unwrap_or(128) * 1024
}
// conn.pragma_update(None, "cache_size", -cache_size_kib())?;
```
- Bulk-init mode keeps its temporary 256 MB override (don't regress that).
- Document the env var in CLI help / README store section.

### R5 — Reader/writer connection separation (MEDIUM — wiring, per D2)
- Audit daemon ↔ MCP wiring: confirm the MCP query server uses an
  `open_read_only()` handle distinct from the writable indexing handle.
- If shared today, route MCP reads through `open_read_only()`.
- `open_read_only()` already sets `query_only=ON` and skips the migration/backfill
  write work — no new code needed beyond the call-site swap.
- **Do not** convert `SqliteStore` into a two-connection struct in this issue
  (deferred; would be its own RFC if ever needed).
- **Test:** integration test — start a long write txn on the writer, assert a
  concurrent read on the read-only handle returns the pre-write snapshot without
  blocking (WAL reader isolation).

### R6 — Adjacency LRU cache (MEDIUM — GATED on profiling, per D3)
Only if Phase-4 profiling shows hot-node reuse:
```rust
use parking_lot::Mutex;
use lru::LruCache;

pub struct SqliteStore {
    conn: Connection,
    edge_cache: Mutex<LruCache<NodeId, Arc<[Edge]>>>, // cap 10_000 ≈ 12 MB
}

fn iter_edges_from(&self, id: NodeId) -> Result<Vec<Edge>, StoreError> {
    if let Some(hit) = self.edge_cache.lock().get(&id) {
        return Ok(hit.to_vec());
    }
    let edges: Arc<[Edge]> = self.fetch_edges_from_db(id)?.into();
    self.edge_cache.lock().put(id, edges.clone());
    Ok(edges.to_vec())
}
```
- New deps: `lru` + `parking_lot` — must clear `cargo deny` (license + advisory)
  and `cargo audit`; flag for Principal Security Engineer dependency vetting.
- Invalidation: coarse `edge_cache.lock().clear()` inside every `&mut self` write
  method. Correct under single-writer; refine to per-node eviction later if needed.
- Applies to both `iter_edges_from` and `iter_edges_to`.
- **Read-only handle:** the cache is also valid on the read-only connection (its
  snapshot is immutable for the connection's lifetime) — but keep it simple and
  only enable on the long-lived daemon handle.

### R7 — `mmap_size` env override, off by default (LOW — production only)
Mirror R4 with `TRAVSR_STORE_MMAP_GB` (default `0`, preserving current MVP
behavior). Only meaningful on a dedicated single-instance OCI A1 deployment;
keep it disabled for the multi-process local default to avoid RSS bloat
(the original reason `mmap_size=0` exists — see `configure()` comment).

---

## 5. Rollout Order & Risk Tiers

| Phase | Fixes | Risk | App-code change | Gate |
|---|---|---|---|---|
| **1** | R2 (migration) | Low | None | EXPLAIN QUERY PLAN covering-index test |
| **2** | R1 (batch get_nodes) | Low–Med | 6 tools | Per-tool output snapshot parity + bench |
| **3** | R3 + R4 + R7 (pragmas/env) | Low | store + init | Pragma applied test; default unchanged |
| **4** | R5 (reader wiring) | Med | daemon/mcp wiring | Concurrent read-vs-write isolation test |
| **5** | R6 (LRU) — **gated** | Med | store + new deps | Only if profiling shows reuse; deny/audit clean |

Rationale (matches issue's recommended order): R2 first because it's pure SQL with
immediate effect and zero app risk; R1 next for the biggest latency win; pragmas/env
are two-line low-risk; R5 is wiring; R6 is the only speculative one and is held
behind measurement.

**PR strategy:** Phases 1–3 in one PR (`[travsr-store] #323 R1+R2+R3+R4+R7 read-path
perf`), Phase 4 (R5) in a second PR if wiring proves non-trivial, Phase 5 (R6) in a
third PR only after the benchmark justifies it. Each PR independently green.

---

## 6. Test & Benchmark Strategy (quality gates)

**Correctness (must pass before any merge):**
- R1 parity: golden-output snapshot test per tool on a fixture graph — byte
  identical pre/post. This is the single most important gate (N+1 → batch must not
  reorder, drop, or dedup differently).
- R2: `EXPLAIN QUERY PLAN` assertion for both forward and reverse covering scans.
- R3: after `open()`, assert `sqlite_stat1` is populated for `edges`/`nodes`.
- R4/R7: assert pragma reflects env override and the unchanged default.
- R5: WAL snapshot-isolation integration test (writer txn open, reader unblocked).
- R6 (if landed): cache hit returns identical edges to a cold DB read; write
  invalidates; property test `cached == uncached` for random query sequences.

**Performance (regression guard, future-proofing):**
- Add a Criterion bench in `travsr-store`/`travsr-retrieval`: synthetic graph
  (≥100k nodes, fan-in/out ~20), measure `get_callers`/`get_blast_radius` query
  latency and **query count** (instrument `iter_edges_*`/`get_node` call counts).
- R1 success metric: query count for a 100-caller symbol drops from ~101 → ~2.
- Wire this bench into the **nightly perf CI** (the still-open follow-up from #295
  T6) so these wins don't silently regress.

**Standard gates (per `feedback_ci_local_before_push`):** run locally before every
commit/push — `cargo fmt --check`, `clippy -D warnings`, `cargo test -p
travsr-store -p travsr-mcp`, `cargo deny check`, MSRV. Scope tests to changed
crates (`feedback_test_scope`); escalate to cross-crate only if the `Store` trait
surface changes. Build the release binary and exercise `get_callers` /
`get_blast_radius` end-to-end on the Travsr repo itself before committing
(`feedback_test_locally_before_commit`, `feedback_test_before_commit` — let user
test first).

---

## 7. Future-Proofing Notes

- **Kùzu migration path:** R1 (batch fetch) and R5 (read/write separation) are the
  patterns Kùzu wants too — batch node materialization and reader snapshots carry
  forward. R2/R3/R4/R7 are SQLite-specific and become no-ops under Kùzu; isolate
  them behind the `Store` impl so the trait contract is unchanged.
- **Config evolution:** the `TRAVSR_STORE_*` env vars are forward-compatible with a
  future `config.toml` (env overrides file). File a follow-up issue for the config
  loader rather than blocking this PR.
- **R6 generality:** if the LRU lands, keep it inside `SqliteStore` (not
  `travsr-retrieval`) so the cache benefits every caller and respects the crate
  dependency rules (retrieval depends on store, never the reverse).
- **Trait stability:** none of these fixes change the `Store` trait signature
  (R6 uses interior mutability precisely to preserve `&self`). The public read API
  is unchanged — downstream crates need no edits.

---

## 8. Out of Scope / Deferred

- `~/.config/travsr/config.toml` loader (D1) — separate issue.
- `SqliteStore { writer, reader }` two-connection struct refactor (D2) — only if
  the daemon-wiring realization of R5 proves insufficient; would need an RFC.
- Per-node (vs coarse-clear) cache invalidation for R6 — refinement after it lands.
- Production `mmap_size` tuning values — DevOps to set on the OCI A1 deployment.

---

## 9. Open Questions (for sign-off)

1. **D1:** Agree on env-var (`TRAVSR_STORE_CACHE_MB` / `_MMAP_GB`) for R4/R7 now,
   defer `config.toml`? *(Recommended: yes.)*
2. **D2:** Agree R5 = daemon read-only-handle wiring, **not** a `SqliteStore`
   struct rewrite? *(Recommended: yes.)*
3. **D3:** Agree R6 is gated on the Phase-4 benchmark and may not ship in this
   issue if reuse isn't demonstrated? *(Recommended: yes.)*
4. PR packaging: Phases 1–3 as one PR, R5/R6 as follow-ups — acceptable?
