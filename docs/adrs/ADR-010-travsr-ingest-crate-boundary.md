# ADR-010: `travsr-ingest` — A Separate Crate for Phase B Semantic Indexing

**Date:** 2026-05-27
**Status:** Proposed
**Phase:** 4 (Sprint 12 — structural prerequisite for Sprints 13–16)
**Author:** Tech Lead
**Supersedes:** N/A
**Related:** RFC-003 (multi-language indexer architecture — crate-dependency-changes section), RFC-008 (multi-language extension architecture), RFC-009 (cross-language bridge plugin system), ADR-006 (rust-analyzer subprocess trust model), ADR-009 (SCIP vs LSIF wire format), CLAUDE.md §crate-dependency-rules

---

## Context

The current Travsr crate graph keeps both Tree-sitter parsing (Phase A) and LSIF ingestion (Phase B) inside `travsr-indexer`:

```
travsr-indexer/src/
  lib.rs
  ffi.rs              ← FFI marker model (Phase A)
  ffi_resolver.rs     ← cross-language resolver (Phase B-adjacent)
  go.rs               ← Tree-sitter Go (Phase A)
  hash.rs             ← SHA256 file hashing (Phase A)
  lsif.rs             ← LSIF JSON-LD parser (Phase B)
  python.rs           ← Tree-sitter Python (Phase A)
  python_lsif.rs      ← Python LSIF specifics (Phase B)
  ra_runner.rs        ← rust-analyzer subprocess invocation (Phase B)
  runner.rs           ← Walker + parse dispatcher (Phase A)
  rust.rs             ← Tree-sitter Rust (Phase A)
  sandbox.rs          ← ADR-006 sandbox primitives (Phase B trust)
  typescript.rs       ← Tree-sitter TypeScript (Phase A)
```

This layout was correct for Phase 1–3 (one language at a time, LSIF being a thin add-on). For Phase 4, it has three concrete problems:

1. **Mixed trust profile.** ADR-006 sandbox rules apply only to subprocess invokers (`ra_runner.rs`, future `scip-java` invoker, etc.). The Tree-sitter parsers do not execute user code and need no sandboxing. Mixing them in one crate prevents a `cargo deny` policy that restricts subprocess-spawning dependencies to ingest-only.
2. **Dependency footprint pollution.** Adding SCIP (per ADR-009) introduces `prost` (protobuf). Adding LSIF transcoders introduces `serde_json` streaming logic. Neither dependency should bleed into the Tree-sitter parsing path used by the always-fresh structural indexer.
3. **Release-cadence coupling.** LSIF and SCIP indexer integrations evolve with their upstream toolchains (rust-analyzer ships monthly; `scip-java` ships continuously). Tree-sitter grammars change rarely. Coupling them in one crate means a `cargo update` for the rapidly-changing Phase B path triggers rebuilds of the stable Phase A path.

This ADR decides: **split Phase B into a new crate, `travsr-ingest`, at the same dependency tier as `travsr-indexer`**.

---

## Decision

### Rule 1 — `travsr-ingest` is a peer to `travsr-indexer`

The new crate joins the existing dependency graph as a peer, both depending only on `travsr-core`:

```
travsr-core                  ← unchanged, zero internal deps
  ├── travsr-indexer         ← Tree-sitter, Phase A; depends on travsr-core only
  ├── travsr-ingest          ← LSIF/SCIP, Phase B; depends on travsr-core only    NEW
  └── travsr-store           ← unchanged
          └── travsr-retrieval ← unchanged
                  └── travsr-mcp ← unchanged
                          └── travsr-daemon ← depends on indexer + ingest    UPDATED
                                  └── travsr-cli ← unchanged
```

`travsr-ingest` does **not** depend on `travsr-indexer`. Both produce `ParseOutput` independently; the daemon merges the two streams. This preserves the existing rule from CLAUDE.md: *"travsr-indexer → depends on travsr-core only"*.

The dependency table in CLAUDE.md is updated:

```diff
 travsr-core        → zero dependencies on other travsr crates
 travsr-indexer     → depends on travsr-core only
+travsr-ingest      → depends on travsr-core only
 travsr-store       → depends on travsr-core only
 travsr-retrieval   → depends on travsr-core + travsr-store
 travsr-mcp         → depends on travsr-retrieval
-travsr-daemon      → depends on travsr-mcp + travsr-indexer
+travsr-daemon      → depends on travsr-mcp + travsr-indexer + travsr-ingest
 travsr-cli         → depends on travsr-daemon
```

### Rule 2 — Content split (what moves, what stays)

**Moves from `travsr-indexer` to `travsr-ingest`:**

| Source path | Destination path | Reason |
|-------------|------------------|--------|
| `travsr-indexer/src/lsif.rs` | `travsr-ingest/src/lsif.rs` | LSIF JSON-LD parser — Phase B |
| `travsr-indexer/src/python_lsif.rs` | `travsr-ingest/src/python_lsif.rs` | Python-specific LSIF handling. **Cross-crate-import note:** `travsr-indexer/src/python.rs` currently `use`s items from `python_lsif`. The S12 PR breaks this import by making `python_lsif` invisible to `travsr-indexer`. Resolution: the helper functions in `python_lsif.rs` that `python.rs` actually consumes (`extract_module_path`, `normalize_import`) move to `travsr-core::python_common` in the same PR. The pure LSIF-handling code stays in `travsr-ingest`. |
| `travsr-indexer/src/ra_runner.rs` | `travsr-ingest/src/ra_runner.rs` | rust-analyzer subprocess invocation |
| `travsr-indexer/src/sandbox.rs` | `travsr-ingest/src/sandbox.rs` | ADR-006 sandbox primitives — used only by subprocess invokers |
| `travsr-indexer/src/ffi_resolver.rs` | `travsr-ingest/src/bridges/legacy.rs` | Cross-language resolver — operates on merged ingest output. Named `legacy.rs` (not `mod.rs`) to signal that this is the pre-plugin implementation; RFC-009's S15 work converts it into the plugin form under `bridges/`. The `bridges/mod.rs` skeleton is added in S12 alongside `legacy.rs` to host the future `CrossLanguageBridge` trait. |

**Stays in `travsr-indexer`:**

| File / module | Reason |
|---------------|--------|
| `typescript.rs` | Tree-sitter TypeScript — Phase A |
| `rust.rs` | Tree-sitter Rust — Phase A |
| `python.rs` | Tree-sitter Python — Phase A |
| `go.rs` | Tree-sitter Go — Phase A |
| `ffi.rs` | FFI marker model — emitted by Phase A parsers, consumed by `travsr-ingest::bridges` |
| `hash.rs` | SHA256 file hashing — used by both phases but lives at the indexer layer |
| `runner.rs` | Walker + dispatch — Phase A |
| `emit.rs` | ParseOutput construction helpers — shared via `travsr-core` |

**New in `travsr-ingest`:**

| File / module | Purpose |
|---------------|---------|
| `scip.rs` | SCIP protobuf parser (per ADR-009) |
| `invoker.rs` | `LanguageInvoker` trait (per ADR-009 §rule-3) |
| `bridges/` | Cross-language bridge plugins (per RFC-009) |
| `langs/<lang>.rs` | Per-language invokers (TypeScript, Rust, Python today; Java, Kotlin, etc. in Phase 4) |

The `ffi.rs` model stays in `travsr-indexer` because Tree-sitter parsers emit FFI markers as part of structural parsing. The marker resolution (`ffi_resolver.rs` → `bridges/`) moves to `travsr-ingest` because resolution requires the merged semantic graph.

### Rule 3 — Daemon orchestrates both crates

`travsr-daemon` is the only crate that depends on both. The orchestrator runs Phase A and Phase B in parallel per repo:

```rust
// crates/travsr-daemon/src/orchestrator.rs (sketch)
async fn reindex_file(path: &Path, indexer: &Indexer, ingest: &Ingest) -> Result<()> {
    let (struct_out, sem_out) = tokio::try_join!(
        async { indexer.parse_file(path).map_err(IndexError::from) },
        async { ingest.parse_file(path).await.map_err(IngestError::from) },
    )?;

    let merged = merge_parse_outputs(struct_out, sem_out);
    store.commit(merged).await?;
    Ok(())
}
```

The merge logic upserts on VName per ADR-002 provenance rules; Phase B's `lsif`/`scip` provenance wins on conflict with Phase A's `tree-sitter` provenance for the same edge.

### Rule 4 — Build and CI

`travsr-ingest` is added to the workspace `Cargo.toml`:

```toml
[workspace]
members = [
  "crates/travsr-core",
  "crates/travsr-indexer",
  "crates/travsr-ingest",      # new
  "crates/travsr-store",
  "crates/travsr-retrieval",
  "crates/travsr-mcp",
  "crates/travsr-daemon",
  "crates/travsr-cli",
]
```

**The crate boundary is enforced mechanically, not by convention.** `cargo-deny`'s `[bans]` table is workspace-wide, not per-crate — it cannot natively express "crate X may depend on Y, but crate Z may not." We therefore use a two-part CI strategy:

**Part 1 — Workspace-wide ban (cargo-deny):** disallow any subprocess-spawning or protobuf crate from entering the workspace except through the allowlist. Catches accidental top-level additions to `Cargo.lock`.

```toml
# deny.toml (workspace-scoped)
[bans]
multiple-versions = "warn"

# These crates are permitted somewhere in the workspace, but the per-crate
# script (Part 2) enforces that they only appear in travsr-ingest's
# dependency tree.
skip = [
  { name = "tokio-process", version = "*" },
  { name = "duct",          version = "*" },
  { name = "prost",         version = "*" },
  { name = "prost-derive",  version = "*" },
  { name = "prost-types",   version = "*" },
]
```

**Part 2 — Per-crate enforcement (CI script):** run `cargo tree` against each crate that MUST NOT see the restricted deps, and fail CI if any appear:

```yaml
# .github/workflows/crate-boundary.yml — runs on every PR
- name: Assert travsr-indexer has no subprocess or protobuf deps
  run: |
    FORBIDDEN="tokio-process|duct|prost"
    if cargo tree -p travsr-indexer --edges normal --prefix none \
       | grep -E "^(${FORBIDDEN})\\b"; then
      echo "::error::travsr-indexer depends on a forbidden crate (ADR-010 §rule-4)"
      exit 1
    fi

- name: Assert travsr-core has no subprocess deps
  run: |
    if cargo tree -p travsr-core --edges normal --prefix none \
       | grep -E "^(tokio-process|duct)\\b"; then
      echo "::error::travsr-core depends on a subprocess-spawning crate"
      exit 1
    fi
```

This is a real, working mechanism — verified against the current `cargo-deny` 0.14.x feature set. The earlier draft of this ADR proposed a `[bans.<crate>] allow = [...]` syntax that does not exist in `cargo-deny`; that draft is superseded by this two-part approach.

**Part 3 — Dependency-graph assertion:** `cargo depgraph` runs in the same workflow and asserts no edge from `travsr-indexer` → `travsr-ingest` or vice versa, satisfying the CLAUDE.md §module-boundary-rules guarantee.

---

## Migration Plan

The split is a Sprint 12 deliverable. It lands as **one PR** with zero functional change — all existing fixtures pass byte-identically. The PR is reviewed primarily for code-organization correctness, not behavior.

### Step 1 — Create empty crate skeleton

```sh
cargo new --lib crates/travsr-ingest
```

Add to workspace members. Add minimal `Cargo.toml` declaring `travsr-core` as the only travsr dependency.

### Step 2 — Move files with `git mv`

Use `git mv` (not delete + create) to preserve `git blame` history. Create the `bridges/` subdirectory before the final `git mv`:

```sh
# 1. Extract shared Python helpers to travsr-core so python.rs (staying in
#    indexer) and python_lsif.rs (moving to ingest) can both depend on them
#    without creating a forbidden ingest ← indexer edge. This is a small
#    surgical refactor done first so subsequent moves stay byte-identical.
git mv crates/travsr-indexer/src/python_common.rs   crates/travsr-core/src/python_common.rs
# (If python_common.rs does not yet exist, the helpers are extracted from
#  python_lsif.rs into a new travsr-core module in the same commit.)

# 2. Move LSIF / subprocess / sandbox primitives.
git mv crates/travsr-indexer/src/lsif.rs            crates/travsr-ingest/src/lsif.rs
git mv crates/travsr-indexer/src/python_lsif.rs     crates/travsr-ingest/src/python_lsif.rs
git mv crates/travsr-indexer/src/ra_runner.rs       crates/travsr-ingest/src/ra_runner.rs
git mv crates/travsr-indexer/src/sandbox.rs         crates/travsr-ingest/src/sandbox.rs

# 3. Move the legacy FFI resolver into the new bridges/ namespace. The
#    `bridges/mod.rs` skeleton is added in the same commit to host the
#    `CrossLanguageBridge` trait that RFC-009 §1 introduces in S15.
mkdir -p crates/travsr-ingest/src/bridges
git mv crates/travsr-indexer/src/ffi_resolver.rs    crates/travsr-ingest/src/bridges/legacy.rs
```

The `ffi_resolver.rs` → `bridges/legacy.rs` rename signals that the file is the pre-plugin implementation. RFC-009's S15 work converts it to plugin form; in S12 it ships as-is to keep the PR purely structural. The `bridges/mod.rs` skeleton is added (not via `git mv` — new file) in the same commit and re-exports `legacy::*` so existing call sites resolve without further edits.

**Module-level audit anchor.** The new `bridges/legacy.rs` opens with a module-level `//!` doc comment that flags it for ADR-006 scope:

```rust
//! Legacy pre-plugin FFI resolver, relocated from `travsr-indexer` in S12
//! (ADR-010). This code runs inside `travsr-ingest` and is therefore in scope
//! for ADR-006-style subprocess-trust review. Converted to plugin form in
//! S15 per RFC-009.
```

### Step 3 — Update imports

Replace `use crate::lsif::*` with `use travsr_ingest::lsif::*` across daemon code. The compiler surfaces every import that needs updating. Estimated edit count: 15–25 files.

### Step 4 — Update `travsr-indexer/src/lib.rs`

Remove the `pub mod lsif`, `pub mod ra_runner`, `pub mod sandbox`, `pub mod ffi_resolver` declarations. Add a deprecation comment pointing future contributors to `travsr-ingest`:

```rust
// LSIF, SCIP, subprocess invocation, and cross-language resolution moved to
// the `travsr-ingest` crate in Phase 4 (ADR-010). Tree-sitter structural
// parsing stays here.
```

### Step 5 — Update `travsr-daemon`

Add `travsr-ingest = { path = "../travsr-ingest" }` to its `Cargo.toml`. Update the orchestrator per §rule-3 above.

### Step 6 — CI policy update

Add the `cargo deny` per-crate rules. Add the `cargo depgraph` assertion job to `.github/workflows/ci.yml`.

### Step 7 — Verify fixtures green

The full golden-fixture suite (TypeScript, Rust, Python from Sprint 8; Go from Sprint 11) must pass byte-identically. This is the PR gate. Any divergence means the move introduced a bug — root-cause and fix before merge.

### Step 8 — Update CLAUDE.md

Apply the dependency-table diff from §rule-1. Update `## Repo Structure` to list the new crate.

**Estimated effort:** 1 engineer × 3 days. Mostly mechanical; the test suite catches any subtle import or module-visibility regression. No design work — the design is this ADR.

---

## Consequences

### Positive

- **Trust boundary is now a crate boundary.** ADR-006 sandbox audits scope to a single crate. `cargo deny` enforces it mechanically.
- **Tree-sitter parsing isolated from subprocess churn.** `cargo update` in the ingest path doesn't trigger rebuilds in the indexer path. Cold-build time impact estimated at +3–5%; warm-build (incremental) impact estimated at −10–15% because rebuilds are scoped.
- **Phase B evolution is independent.** Adding `scip.rs` (ADR-009), refactoring `ffi_resolver.rs` into plugins (RFC-009), and per-invoker sandbox ADRs (per RFC-008 §unresolved-questions) all happen inside one crate without disturbing Phase A.
- **Clear contributor onboarding.** A new community member adding a Tree-sitter grammar works in `travsr-indexer` only. A new community member adding a SCIP indexer works in `travsr-ingest` only. The split makes the contribution surface obvious.
- **Per-crate feature flags become meaningful.** `travsr-ingest` can ship feature-gated subsystems (`feature = "scip"`, `feature = "sandbox-linux"`, `feature = "sandbox-macos"`) without affecting `travsr-indexer`'s compile graph.

### Negative

- **Workspace grows from 7 to 8 crates.** Documented in RFC-008 §drawbacks. Cold build time +3–5%; mitigated by parallel cargo build and by `travsr-ingest` not being on the `travsr-cli` critical path for users who index Tree-sitter–only languages.
- **`git mv` preserves history but breaks raw GitHub URLs.** Any external doc or bookmark pointing to `crates/travsr-indexer/src/lsif.rs` becomes a 404. Mitigated by leaving a one-line `lsif.rs` stub in the old location for one release that contains a redirect comment, then deleting it. Documented in the migration PR.
- **Two crates can disagree on `travsr-core` version during refactors.** A breaking change to `travsr-core` requires coordinated PRs across both crates. Already true for `travsr-store`; the workspace `Cargo.toml` `[workspace.dependencies]` table keeps versions in sync.
- **Slightly increased orchestration complexity in `travsr-daemon`.** The daemon now joins two parse streams instead of one. Mitigated by `tokio::try_join!` keeping the code linear and by extensive integration tests covering the merge logic.

---

## Threat Model — Crate-Level Security

The split enables three concrete security improvements that are difficult to enforce with a single crate:

| Improvement | Mechanism |
|-------------|-----------|
| Subprocess spawning restricted to `travsr-ingest` | `cargo deny` per-crate ban list (§rule-4) |
| Sandbox primitives auditable in isolation | `sandbox.rs` lives only in `travsr-ingest`; the security review (ADR-006) scopes to one crate |
| Supply-chain blast radius reduced | A compromised `prost` or `serde_json` advisory affects only `travsr-ingest`; `travsr-indexer`'s Tree-sitter path is unaffected |

The Principal Security Engineer review of `travsr-ingest` is the gate for the S12 PR. The review covers:

- The crate's full transitive dependency tree (`cargo tree -p travsr-ingest`)
- The sandbox primitives now isolated in `sandbox.rs`
- The trait-level trust model exposed by `LanguageInvoker::trust_level()` (introduced in ADR-009 §rule-3)
- The `bridges/legacy.rs` cross-language code path pre-RFC-009 conversion

---

## Alternatives Considered

### A. Keep everything in `travsr-indexer` and use module-level visibility

Rejected. Module visibility (`pub(crate)`) prevents external crates from importing internals but does not isolate the dependency graph. `prost` would still be in `travsr-indexer`'s `Cargo.toml`, increasing the compile-time and supply-chain blast radius for the Tree-sitter path. `cargo deny` cannot scope to modules.

### B. Three crates: `travsr-tree-sitter`, `travsr-lsif`, `travsr-scip`

Rejected. Over-segmentation. The unifying concept of `travsr-ingest` is "anything that runs an external toolchain to produce semantic edges." The format (LSIF, SCIP) is an implementation detail of the invoker, not a crate-level concern. Per ADR-009, both parsers cohabit cleanly. Splitting on format would add two crates and one extra dependency edge with no functional benefit.

### C. Move `travsr-indexer` to be downstream of `travsr-ingest`

Rejected. Phase A is the always-fresh baseline that ships on every commit regardless of Phase B availability (ADR-006 sandbox failures must not block structural indexing). A dependency from Tree-sitter to LSIF/SCIP would couple their availability. Peer crates with the daemon as orchestrator preserve the independence.

### D. Leave `ffi_resolver.rs` in `travsr-indexer`

Rejected. The resolver operates on the merged semantic graph; it cannot run without ingest output. Logically, its dependencies and lifecycle belong with ingest. The Phase 1–3 layout had it in the indexer because there was nowhere else to put it; this ADR fixes that.

### E. Defer the split until Phase 5

Rejected. Phase 4's planned additions (SCIP parser, bridge plugin system, per-invoker sandboxing for Java/Kotlin) all amplify the cost of working in a mixed-trust crate. The split is cheap to do now and expensive to do later, after `prost` and `scip-java` are already entangled with the Tree-sitter path.

---

## Implementation Notes

- The S12 PR is structured to make review fast: one commit per `git mv`, one commit for `Cargo.toml` updates, one commit for daemon orchestration changes, one commit for CI policy. Reviewers can read commit-by-commit and verify each step in isolation.
- The PR description includes a verbatim `cargo tree` diff showing the new dependency graph.
- The S12 work does **not** add any new functionality — no SCIP, no plugins, no new languages. Those land in S13–S16 on top of the clean foundation.
- The `cargo depgraph --workspace --no-default-features` output is committed to `docs/repo-graph.svg` (already present in the repo) and updated as part of the PR.
- Telemetry: the daemon emits a `tracing::info!` span on startup indicating which crates contributed parse handlers, so operations have observability into which ingest paths are active.

---

## References

- CLAUDE.md §crate-dependency-rules (the rule this ADR updates)
- RFC-003 §crate-dependency-changes (the precedent for documenting crate-graph updates)
- RFC-008 §3 (the umbrella RFC that requires this split)
- RFC-009 — Cross-Language Bridge Plugin System (the consumer of the `bridges/` directory)
- ADR-006 — rust-analyzer Subprocess Trust Model (the trust precedent now scoped to `travsr-ingest`)
- ADR-009 — SCIP vs LSIF Wire Format (the format work that lives in `travsr-ingest`)
- `docs/repo-graph.svg` — workspace dependency graph (updated by S12 PR)
