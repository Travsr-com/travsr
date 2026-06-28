# RFC-018: Embedding Plugin Architecture

**Status:** Under Review
**Author:** Solution Architect + Tech Lead
**Date:** 2026-06-19
**Crate(s) affected:** `travsr-plugin-protocol` (extend), `travsr-plugin-sdk` (extend), `travsr-plugin-host` (extend), `travsr-store` (migration v12 revised), `travsr-daemon` (embed supervisor), `travsr-cli` (embed subcommand, no feature flag)
**Supersedes:** RFC-012-ADDENDUM-02 §F2 (L2-B ONNX embedding — compile-time feature approach)
**Related:** RFC-011 (two-transport plugin architecture), RFC-012 A2 (tiered retrieval), RFC-010 (knapsack budget), RFC-014 (Phase B unification)

---

## Summary

Replace the compile-time `--features embeddings` approach (RFC-012-ADDENDUM-02 §F2) with a
**downloadable plugin binary** that handles embedding generation and ANN search via the
existing sidecar transport from RFC-011.

The main binary gains zero new Rust dependencies. `travsr embed init` works out of the box
after `npm install -g travsr` — it downloads the prebuilt plugin binary from GitHub Releases,
performs the capabilities handshake, and batch-embeds all indexed nodes. Step 4 of
`search_nodes_fuzzy` fires through an IPC call to the warm plugin subprocess running inside
the daemon. Swapping the embedding model is a CLI command, not a recompile.

---

## Motivation

RFC-012-ADDENDUM-02 §F2 implemented L2-B semantic search behind a compile-time Cargo feature
(`--features embeddings`). This created four problems:

1. **User-facing UX gap.** The npm-published binary ships without the feature unless the
   release pipeline explicitly opts in. End users cannot run `travsr embed init` on the
   default binary.

2. **Heavy default build.** `ort` (ONNX Runtime) downloads ~200 MB of prebuilt native
   binaries at compile time. Every developer's `cargo build` pays this cost whether or not
   they use semantic search.

3. **No model swap without recompile.** Changing the embedding model (e.g. from
   `nomic-embed-text-v1.5` to `voyage-code-3`) requires patching source, rebuilding, and
   republishing the binary. Teams already using the CLI would need to re-install.

4. **`vec0` virtual table leaks into the main process.** The vec0 SQLite extension must be
   registered (`sqlite3_auto_extension`) in every process that opens a DB containing a
   `VIRTUAL TABLE USING vec0`. This pulls `sqlite-vec` into the main binary and violates the
   TL2 gate (zero embedding deps in default build) for any deployment that opens the store.

The plugin approach from RFC-011 already solves analogous problems for language toolchains
(Phase B SCIP tools). This RFC applies the same pattern to embeddings.

---

## Detailed Design

### 1. Storage — plain blob table (migration v12, revised)

Replace the `VIRTUAL TABLE USING vec0` schema with a plain SQLite table. The main binary
requires **no SQLite extension** to read, write, or count embedding rows.

```sql
-- migration v12 (replaces the vec0 virtual table)
CREATE TABLE IF NOT EXISTS node_embeddings (
    node_id  INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    model_id TEXT    NOT NULL,
    embedding BLOB   NOT NULL,   -- dim × 4 bytes, IEEE 754 LE float32
    PRIMARY KEY (node_id, model_id)
);
CREATE INDEX IF NOT EXISTS idx_node_embeddings_model
    ON node_embeddings(model_id);
```

`model_id` enables:
- Multiple models to co-exist (future: compare recall between models).
- Model-change detection: if `meta.current_embed_model` ≠ stored `model_id`, Step 4 degrades
  gracefully (logs at `tracing::debug!`, skips ANN rather than querying stale embeddings).
- Clean rebuild path: `travsr embed switch --backend <id>` re-embeds with the new model.

The main binary operations on this table are:

```sql
-- write path (no extension needed)
INSERT OR REPLACE INTO node_embeddings(node_id, model_id, embedding) VALUES(?1, ?2, ?3);
DELETE FROM node_embeddings WHERE node_id = ?1;
DELETE FROM node_embeddings WHERE node_id IN (SELECT id FROM nodes WHERE path = ?1);

-- status (no extension needed)
SELECT count(*) FROM node_embeddings WHERE model_id = ?1;
SELECT count(*) FROM nodes WHERE id NOT IN (SELECT node_id FROM node_embeddings WHERE model_id = ?1);
```

ANN search (Step 4) is the **only** operation that needs embedding intelligence — and that
runs entirely inside the plugin process.

---

### 2. `EmbedPlugin` trait (extends `travsr-plugin-protocol`)

A new trait alongside `Plugin` in `travsr-plugin-protocol`. It is transport-agnostic; the
`EmbedSidecar` in `travsr-plugin-host` drives it over the same framed-protobuf channel used
for language plugins.

```rust
// travsr-plugin-protocol/src/embed.rs

/// An embedding backend. Stateless; a single instance is held by the daemon's
/// EmbedSupervisor and called concurrently only from the sidecar dispatch loop.
pub trait EmbedPlugin: Send + Sync {
    /// Stable identifier stored as `model_id` in node_embeddings.
    /// Format: "<model-family>-<variant>", e.g. "nomic-v1.5-int8", "voyage-code-3".
    /// Changing the model MUST change model_id — the daemon uses this to detect staleness.
    fn model_id(&self) -> &str;

    /// Output vector dimension after any truncation (e.g. 256 for MRL-256).
    fn dim(&self) -> usize;

    /// Maximum items in a single embed_batch call. Default: 100.
    fn max_batch(&self) -> usize { 100 }

    /// ANN strategy this plugin uses for knn().
    fn ann_strategy(&self) -> AnnStrategy;

    /// Embed a batch of (node_id, text) pairs.
    /// text = "{signature} {kind} {path}" — same concatenation as RFC-012 A2 §write-path.
    /// The prefix "search_document: " is applied inside the plugin.
    /// MUST return exactly one result per input item (same order).
    fn embed_batch(&self, items: &[EmbedItem]) -> Vec<EmbedResult>;

    /// ANN search. Embed `query` with prefix "search_query: " and return
    /// the top-k node_ids ordered by cosine similarity (closest first).
    /// The plugin opens `db_path` with its own connection (registering any
    /// extensions it needs — the main process's DB connection is untouched).
    fn knn(&self, query: &str, k: usize, db_path: &std::path::Path) -> Vec<KnnHit>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnStrategy {
    /// In-process cosine similarity over all stored blobs. O(n·d).
    /// Correct for < 50k embeddings; plugin should auto-upgrade to vec0 above that.
    BruteForce,
    /// sqlite-vec vec0 virtual table opened inside the plugin's own connection.
    /// No extension registration needed in the main process.
    Vec0,
    /// HNSW index file at `~/.travsr/<corpus>/embed.idx`.
    /// Plugin manages the index file lifecycle independently.
    Hnsw,
}

pub struct EmbedItem {
    pub node_id: u64,
    pub text:    String,
}

pub struct EmbedResult {
    pub node_id:   u64,
    pub embedding: Vec<f32>,   // length == EmbedPlugin::dim()
}

pub struct KnnHit {
    pub node_id: u64,
    pub score:   f32,   // cosine similarity ∈ [0, 1]
}
```

**Normative:** `embed_batch` MUST NOT open `db_path`. It operates on text only. `knn` MAY
open `db_path` read-only and is the only method permitted to register SQLite extensions in
that connection.

---

### 3. Protocol extension (plugin.proto)

New message types in `travsr-plugin-protocol/proto/plugin.proto`, added to the existing
`Request`/`Response` oneof. Wire framing is unchanged (4-byte BE length prefix + protobuf).

```protobuf
// --- Embed plugin handshake ---
message EmbedHandshakeResponse {
    string model_id         = 1;   // e.g. "nomic-v1.5-int8"
    uint32 dim              = 2;   // 256 for MRL-256
    uint32 max_batch        = 3;   // 100
    string ann_strategy     = 4;   // "brute-force" | "vec0" | "hnsw"
    uint32 protocol_version = 5;   // monotonic; daemon fail-fasts on mismatch
    string plugin_version   = 6;   // semver
}

// --- Batch embed ---
message EmbedRequest {
    repeated EmbedItemProto items = 1;
}
message EmbedItemProto {
    uint64 node_id = 1;
    string text    = 2;
}
message EmbedResponse {
    repeated EmbedResultProto results = 1;
}
message EmbedResultProto {
    uint64          node_id   = 1;
    repeated float  embedding = 2;
}

// --- ANN search ---
message KnnRequest {
    string query   = 1;
    uint32 k       = 2;
    string db_path = 3;
}
message KnnResponse {
    repeated KnnHitProto hits = 1;
}
message KnnHitProto {
    uint64 node_id = 1;
    float  score   = 2;
}
```

The daemon sends `EmbedHandshakeRequest` (identical structure to `HandshakeRequest`) at plugin
startup and validates `EmbedHandshakeResponse.protocol_version` before accepting the plugin.

---

### 4. `EmbedSidecar` in `travsr-plugin-host`

A new struct that manages the embed plugin subprocess. It mirrors `Sidecar` from RFC-011 but
with different message types and a **persistent** (not lazy) lifetime.

```rust
// travsr-plugin-host/src/embed_sidecar.rs

pub struct EmbedSidecar {
    child:    SandboxedChild,
    codec:    FrameCodec,
    pub caps: EmbedCapabilities,
}

pub struct EmbedCapabilities {
    pub model_id:    String,
    pub dim:         usize,
    pub max_batch:   usize,
    pub ann_strategy: AnnStrategy,
    pub plugin_version: String,
}

impl EmbedSidecar {
    /// Spawn the plugin binary, perform capabilities handshake, return the
    /// ready sidecar or an error if handshake fails or version mismatches.
    pub fn spawn(binary: &Path, db_path: &Path) -> Result<Self, EmbedError> { ... }

    /// Send a batch of items to embed; blocks until all results arrive.
    /// Caller chunks input to caps.max_batch before calling.
    pub fn embed_batch(&mut self, items: &[EmbedItem]) -> Result<Vec<EmbedResult>, EmbedError> { ... }

    /// Send a KNN query; blocks until hits arrive.
    pub fn knn(&mut self, query: &str, k: usize) -> Result<Vec<KnnHit>, EmbedError> { ... }
}
```

The sidecar runs under the same `SandboxedChild` wrapper as Phase B tools (ADR-017) but with
**network access disabled** — the model is loaded from local files, no egress needed at
runtime. The sandbox policy key is `"embed"`.

---

### 5. Daemon integration — `EmbedSupervisor`

A new subsystem in `travsr-daemon` that owns the embed plugin lifecycle.

```
travsr-daemon startup:
  1. Check ~/.travsr/bin/travsr-embed-<backend> exists.
  2. If yes: spawn EmbedSidecar, perform handshake.
  3. If no:  log "embed plugin not installed — `travsr embed init` to enable Step 4"
             Step 4 disabled for this daemon session.
  4. On handshake success: compare caps.model_id with store.get_meta("current_embed_model").
     If mismatch: warn "model_id mismatch — run `travsr embed init` to re-index".
                  Step 4 disabled until mismatch resolved.
```

Step 4 integration in `search_nodes_fuzzy` (no Cargo feature gate, no unsafe):

```rust
// travsr-store/src/lib.rs — search_nodes_fuzzy, after Step 3

// Step 4 — semantic ANN (RFC-018). Fires only when Steps 1–3 combined return
// empty. Routed through the daemon's EmbedSupervisor via a callback registered
// at store-open time. No-ops to Ok(vec![]) when no embed plugin is running.
if let Some(knn_fn) = self.embed_knn_hook.as_ref() {
    let ids = knn_fn(query, 20)
        .map_err(|e| StoreError::Database(e.to_string()))?;
    if !ids.is_empty() {
        tracing::debug!(layer = "embed_ann", nodes_returned = ids.len());
        return self.get_nodes(&ids);
    }
}
Ok(Vec::new())
```

`embed_knn_hook` is a `Option<Arc<dyn Fn(&str, usize) -> Result<Vec<NodeId>> + Send + Sync>>`
injected by the daemon when it opens the store. The store crate has **zero** knowledge of the
plugin system — only the daemon wires them together.

---

### 6. Backend catalog (`travsr-plugin-host/src/embed_catalog.rs`)

Mirrors the Phase B language catalog from `travsr-plugin-host`.

```rust
pub struct EmbedBackend {
    pub id:               &'static str,     // "nomic-v1.5-int8"
    pub description:      &'static str,
    pub dim:              usize,
    pub binary_name:      &'static str,     // "travsr-embed-nomic-v1.5-int8"
    pub github_repo:      &'static str,     // "Travsr-com/travsr-embed"
    pub version_fallback: &'static str,
    pub model_files:      &'static [EmbedModelFile],
}

pub struct EmbedModelFile {
    pub name:         &'static str,   // "model_int8.onnx"
    pub url_path:     &'static str,   // "onnx/model_int8.onnx" under HF base
    pub hf_repo:      &'static str,   // "nomic-ai/nomic-embed-text-v1.5"
    pub size_hint_mb: u32,
}

pub const BACKENDS: &[EmbedBackend] = &[
    EmbedBackend {
        id:               "nomic-v1.5-int8",
        description:      "nomic-embed-text-v1.5 int8 ONNX — 137 MB, MRL-256 (dim=256), local inference",
        dim:              256,
        binary_name:      "travsr-embed-nomic-v1.5-int8",
        github_repo:      "Travsr-com/travsr-embed",
        version_fallback: "v1.0.0",
        model_files: &[
            EmbedModelFile {
                name:         "model_int8.onnx",
                url_path:     "onnx/model_int8.onnx",
                hf_repo:      "nomic-ai/nomic-embed-text-v1.5",
                size_hint_mb: 137,
            },
            EmbedModelFile {
                name:         "tokenizer.json",
                url_path:     "tokenizer.json",
                hf_repo:      "nomic-ai/nomic-embed-text-v1.5",
                size_hint_mb: 1,
            },
        ],
    },
    // Future entries — no code changes in travsr-core or travsr-store:
    // EmbedBackend { id: "voyage-code-3", description: "Voyage Code 3 via API — no local model", ... }
    // EmbedBackend { id: "openai-text-3-small", ... }
];

pub fn lookup(id: &str) -> Option<&'static EmbedBackend> {
    BACKENDS.iter().find(|b| b.id == id)
}
```

Adding a new backend is a **catalog entry + a plugin binary release**. Zero changes to
`travsr-core`, `travsr-store`, `travsr-retrieval`, or `travsr-mcp`.

---

### 7. CLI subcommands (zero Cargo feature flags)

`embed.rs` in `travsr-cli` — unconditionally compiled, no `#[cfg(feature = "embeddings")]`.

```
travsr embed list
  Shows the backend catalog with install status.

travsr embed init [--backend <id>] [--model-dir <dir>] [--rebuild]
  1. Resolve backend (default: nomic-v1.5-int8).
  2. Download travsr-embed-<backend>-<target> → ~/.travsr/bin/ (like lang install).
  3. Download model files from HuggingFace → ~/.travsr/models/<backend>/.
  4. Perform capabilities handshake (smoke-test the binary).
  5. Open graph store; if model_id mismatch or --rebuild: delete stale embeddings.
  6. Batch-embed all unembedded nodes via EmbedSidecar IPC (chunks of max_batch).
  7. set_meta("current_embed_model", caps.model_id).
  8. Restart daemon so it picks up the newly installed plugin.

travsr embed status [--backend <id>]
  Shows: plugin binary present?, model files present?, embedded/total counts,
  model_id stored in meta, dim, ann_strategy.

travsr embed switch --backend <id>
  = travsr embed init --backend <id> --rebuild
  (re-embeds all nodes with the new model; old model's rows deleted first)
```

None of these commands require `ort`, `sqlite-vec`, `tokenizers`, or `ndarray` in the main
binary. The main binary only does:
- File existence checks (`~/.travsr/bin/travsr-embed-*`)
- HTTP downloads (reqwest — already a dependency)
- `EmbedSidecar::spawn` + IPC calls (in `travsr-plugin-host`)
- Plain SQL on `node_embeddings` (INSERT OR REPLACE / SELECT count(*))

---

### 8. Plugin binary (`travsr-embed` repo)

A new repository: `Travsr-com/travsr-embed`.

```
travsr-embed/
├── Cargo.toml              (workspace)
├── crates/
│   ├── travsr-embed-core/  (EmbedPlugin trait impl harness, depends on travsr-plugin-sdk)
│   └── travsr-embed-nomic/ (nomic-embed-text-v1.5 backend: ort + tokenizers + ndarray)
└── .github/
    └── workflows/
        └── release.yml     (builds for aarch64-apple-darwin, x86_64-linux, aarch64-linux, x86_64-windows)
```

The nomic backend binary:
- Links `ort` with `download-binaries` feature (200 MB ORT native lib, only in this repo)
- Uses `sqlite-vec` for vec0 KNN (registered in the plugin's own SQLite connection)
- Self-selects ANN strategy at runtime: brute-force if `node_embeddings` count < 50k,
  vec0 otherwise (upgrades transparently as the DB grows)

A future `travsr-embed-voyage` backend would link the Voyage API client instead of ort —
same binary interface, different deps, zero changes to the main repo.

---

### 9. Model swappability contract

| State | `current_embed_model` meta | `node_embeddings.model_id` | Step 4 behaviour |
|---|---|---|---|
| No plugin installed | (none) | (empty table) | Skip — returns Ok(vec![]) |
| Plugin installed, first run | (none) | (empty table) | `embed init` needed |
| Normal operation | `"nomic-v1.5-int8"` | `"nomic-v1.5-int8"` | Full ANN |
| Model changed, not rebuilt | `"voyage-code-3"` | `"nomic-v1.5-int8"` | Skip + warn "run embed switch" |
| Rebuilding | `"voyage-code-3"` | mixed | Skip (partial index) |
| Rebuild complete | `"voyage-code-3"` | `"voyage-code-3"` | Full ANN |

The daemon checks model_id match at startup. Mismatch → Step 4 disabled → `tracing::warn!`
→ user-visible hint via `travsr status`. No crash, no wrong results.

---

### 10. ANN strategy evolution (inside plugin, transparent to main binary)

The plugin binary upgrades its ANN strategy automatically as the embedding table grows:

```
Phase 1 (< 50k rows):   brute-force cosine in-process
                         O(n·d) — 50k × 256 × 4 = 50 MB, ~5ms on modern hardware

Phase 2 (50k–2M rows):  vec0 KNN inside plugin's own connection
                         O(log n) per query, vec0 managed entirely in plugin process

Phase 3 (> 2M rows):    HNSW index at ~/.travsr/<corpus>/embed.idx
                         O(log n) amortized, ~1ms P99, incremental updates
```

The `ann_strategy` field in `EmbedHandshakeResponse` lets the daemon log which strategy is
active. The strategy selection logic lives entirely in the plugin binary — updating the
strategy does not require a main binary release.

---

### 11. Crate dependency impact

```
UNCHANGED:
  travsr-core        — no embedding types; NodeId / VName / Edge unchanged
  travsr-store       — plain SQL on node_embeddings; no extension; no new deps
  travsr-retrieval   — embed_knn_hook injected at runtime; no new deps
  travsr-mcp         — no changes

EXTENDED (new files, no new external deps to main binary):
  travsr-plugin-protocol — +embed.rs (EmbedPlugin trait, message types)
  travsr-plugin-sdk      — +embed harness (run_embed_plugin helper)
  travsr-plugin-host     — +embed_sidecar.rs, +embed_catalog.rs, +embed_supervisor.rs

UPDATED:
  travsr-daemon          — EmbedSupervisor wired at startup; embed_knn_hook injected
  travsr-cli             — embed.rs (unconditional; downloads plugin binary via reqwest)

NEW REPO:
  Travsr-com/travsr-embed — plugin binaries (ort + sqlite-vec live here, not in main repo)
```

TL2 CI gate (`cargo metadata --no-deps` check) continues to pass: `ort` and `sqlite-vec` are
absent from every crate in the main repo.

---

### 12. What to do with P2-A / P2-B / P2-C / P2-D (current branch)

The following commits on `feature/travsr-retrieval-embeddings` are superseded:

| Commit | Status | Action |
|---|---|---|
| `2776c08` — PR-1 edge annotation | ✅ Keep — unrelated | Cherry-pick to new branch |
| `ab45fe5` — P2-A dep pinning + vec0 loader | 🗑 Remove | Reverted by RFC-018 |
| `c9b9314` — P2-B write path (embeddings.rs) | 🗑 Remove | Replaced by EmbedSidecar IPC |
| `d708055` — P2-C Step 4 + vec_search | 🗑 Rework | embed_knn_hook replaces cfg-gated call |
| P2-D embed.rs (CLI, this session) | 🗑 Rework | New embed.rs: plugin download + IPC |
| P2-E CI gate (ci.yml) | ✅ Keep — gate logic unchanged | Minor wording update |

Migration v12 (`v12_vec0_embeddings.sql`) is replaced by the plain blob table DDL above.
The `embeddings` feature is removed from both `travsr-store` and `travsr-cli` Cargo.toml.
`#![forbid(unsafe_code)]` is restored in `travsr-store/src/lib.rs` (no unsafe needed).

---

## Alternatives Considered

### A. Compile-time feature flag (current P2-A/B/C/D)
Rejected — see Motivation §1–4. Forces ort into the main binary, requires separate build for
end users, makes model swap a recompile, and forces vec0 registration in the main process.

### B. WASM plugin (compile embedding model to WASM, load in-process)
Interesting but rejected for now. WASM JIT for ONNX inference is ~3–5× slower than native
ORT. No stable Rust WASM runtime with good SIMD support exists at this maturity level.
Revisit when `wasmtime` + WASM SIMD is mainstream. Filed as DEBT(travsr-#TBD).

### C. Extend `travsr-lang` repo (add embed backend as a language plugin)
Rejected. Language plugins implement `parse()` and `invoke_phase_b()` — file-oriented,
stateless per call. The embed plugin is DB-oriented, session-persistent, and bidirectional
(write + read path). Forcing it into the `Plugin` trait would require adding embed-specific
methods to a language-specific contract and confuse the `travsr lang list` output.

### D. REST/HTTP sidecar (embed plugin as a localhost HTTP server)
Rejected. Violates "local first" — requires port management, process management, TLS/auth
decisions. Stdio framing (RFC-011 §4) gives the same bidirectionality with zero network
surface and natural process lifecycle (child dies when parent dies).

### E. Separate `embed_knn_hook` per query (spawn plugin per query, no warm process)
Rejected. Model cold-start for `nomic-v1.5-int8` is ~1–2 seconds (ORT session init).
Step 4 latency P95 must be < 50ms (RFC-012 A2 SLA). Warm persistent process is required.

---

## Drawbacks

- **New repo (`travsr-embed`) to maintain.** Release pipeline for the plugin binary must be
  set up and kept green independently. Mitigation: same `release.yml` pattern as `travsr-lang`.

- **IPC latency on Step 4.** Cold query through a warm sidecar adds ~1–5ms IPC overhead vs.
  the in-process approach. Step 4 fires only when Steps 1–3 miss — the common case is still
  in-process BFS/PPR with zero IPC cost. Acceptable.

- **Plugin binary version drift.** The main binary and the embed plugin are released
  independently. Protocol version validation in the handshake fail-fasts on mismatch — the
  daemon logs a clear error and disables Step 4 until the plugin is updated. This is by
  design: it surfaces skew explicitly rather than silently producing wrong results.

- **Model files must be downloaded separately from the plugin binary.** `travsr embed init`
  does two downloads: the plugin binary (~10 MB) and the model files (~137 MB for nomic). The
  CLI shows sizes and progress for both. This is unavoidable — the model files are not
  bundled in the plugin binary to keep download size manageable.

---

## Unresolved Questions

1. **Incremental embedding during `hook-run`.** When a commit modifies N files, the indexer
   writes new nodes. Should the daemon immediately call `embed_batch` for those new nodes, or
   should embeddings be lazily generated on the next `travsr embed init`? Proposal: immediate
   — the EmbedSupervisor subscribes to the post-write hook in the daemon and calls
   `embed_batch` for newly written nodes if the plugin is running. Left for the implementation
   PR to decide.

2. **ANN strategy auto-upgrade.** The plugin self-selects BruteForce vs vec0 at startup based
   on row count. Should it re-evaluate mid-session (e.g. after a `travsr embed init` that adds
   rows past the 50k threshold)? Proposal: re-evaluate at each `knn()` call (cheap count
   query). Implementation detail left to the plugin binary.

3. **Multiple DB paths (global MCP server).** `travsr mcp --global` serves multiple repos,
   each with its own `graph.db`. The EmbedSupervisor would need one plugin instance per DB
   path, or the `KnnRequest` must carry the `db_path`. Current design passes `db_path` in
   `KnnRequest` (§3) — this supports the global case without spawning multiple plugin
   processes.

4. **Plugin binary signing.** Downloaded binaries should be SHA256-verified against a
   manifest in the GitHub Release. The `travsr lang install` pattern already does this (SHA256
   sidecar file). The embed plugin installer MUST follow the same pattern. Left for the
   `travsr-embed` repo release workflow.

5. **API-backed backends (no local model).** A `voyage-code-3` backend would call the Voyage
   API instead of running a local ONNX session. This requires an API key — stored in
   `~/.travsr/embed.toml` or `TRAVSR_EMBED_API_KEY`. The `EmbedPlugin` trait is compatible
   (embed_batch and knn look the same from the daemon's perspective); the plugin binary just
   calls the API instead of ort. The `model_files` field in `EmbedBackend` is empty for
   API-backed backends. Deferred to a follow-up implementation PR.

---

## Acceptance Criteria (Implementation Gate)

- [ ] Migration v12 creates `node_embeddings` as a plain table (no vec0 virtual table)
- [ ] `cargo build --workspace` zero new external deps vs. pre-RFC-018 default build
- [ ] TL2 CI gate (`cargo metadata` check) still green: ort and sqlite-vec absent
- [ ] `#![forbid(unsafe_code)]` restored in `travsr-store/src/lib.rs`
- [ ] `travsr embed init` works on a freshly `npm install -g travsr`'d binary (no --features)
- [ ] `travsr embed status` shows correct counts and model_id without plugin running
- [ ] Step 4 returns correct ANN results when plugin is running
- [ ] Step 4 silently skips (Ok(vec![])) when plugin is not installed
- [ ] Step 4 silently skips + warns when model_id mismatches current_embed_model meta
- [ ] Daemon crash-isolated: embed plugin panic/exit does not crash the daemon
- [ ] `travsr embed switch --backend <id>` re-embeds with new model, old rows deleted
- [ ] Protocol handshake fail-fasts on version mismatch (daemon logs clear error, disables Step 4)
- [ ] SHA256 verification on plugin binary download (same as `travsr lang install`)

---

## Implementation Order

```
[1] Revert P2-A/B/C/D from feature/travsr-retrieval-embeddings
    Cherry-pick PR-1 (edge annotation) + P2-E (CI gate) onto new branch

[2] Migration v12 — plain blob table (replaces vec0 DDL)
    travsr-store: restore #![forbid(unsafe_code)]; drop embeddings feature

[3] travsr-plugin-protocol — add embed.rs (EmbedPlugin trait + proto messages)
    travsr-plugin-sdk — add run_embed_plugin() harness

[4] travsr-plugin-host — EmbedSidecar + EmbedCatalog + EmbedSupervisor

[5] travsr-store — embed_knn_hook (Option<Arc<dyn Fn>>, injected by daemon)
    travsr-daemon — EmbedSupervisor wired at startup; hook injected on store open

[6] travsr-cli — embed.rs (list/init/status/switch); unconditional, no feature gate

[7] travsr-embed repo — nomic-v1.5-int8 plugin binary (ort + sqlite-vec + tokenizers)
    Release pipeline for aarch64-apple-darwin, x86_64-linux, aarch64-linux, x86_64-windows

[8] PR-2: squash-merge to master
```

Steps [1]–[6] land in the main repo. Step [7] is a separate repo and can be parallelised
with steps [4]–[6] once the protocol messages are stable (step [3]).
