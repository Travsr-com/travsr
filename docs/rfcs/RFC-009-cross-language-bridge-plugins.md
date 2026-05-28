# RFC-009: Cross-Language Bridge Plugin System

**Status:** Draft
**Author:** Tech Lead
**Date:** 2026-05-27
**Phase:** 4 (Sprint 15 implementation gate)
**Crate(s) affected:** `travsr-core`, `travsr-ingest` (new — see ADR-010)
**Supersedes (in part):** RFC-005 §pipeline-placement (the hardcoded `ffi_resolver::Resolver` becomes plugin-driven), RFC-005 §ffi-pairs (the three-pair table becomes a plugin registry)
**Related:** RFC-005 (the predecessor this RFC generalizes), RFC-008 (multi-language extension architecture umbrella), ADR-005 (per-language corpus naming — corpus invariant relied upon here), ADR-009 (SCIP wire format)

---

## Summary

Replace the fixed-set FFI resolver (RFC-005's hardcoded napi / pyo3 / cgo handling in `ffi_resolver.rs`) with a registry of `CrossLanguageBridge` plugins. Each plugin declares one language pair and one bridge mechanism. The registry runs all applicable plugins against the merged SCIP index in a single pass, producing `FFICall` edges. SCIP's standardized package-qualified symbol format collapses most heuristic name-matching to exact symbol equality, raising confidence from RFC-005's 30–95 range to 100 for static FFI boundaries.

---

## Motivation

RFC-005 solved cross-language FFI for three specific pairs (TS↔Rust via napi, Python↔Rust via pyo3, Go↔C via cgo) using `ffi_resolver.rs` — a hardcoded resolver that pattern-matches language-specific annotations and assigns heuristic confidence scores. The architecture has three structural limits:

1. **Adding a new bridge requires editing `ffi_resolver.rs`.** Java↔C via JNI, Kotlin↔Java via JVM, Swift↔ObjC via runtime bridges, Ruby↔C via C extensions, Lua↔C via `lua_register` — each of these is a `match` arm in a single file. The combinatorial pressure grows as Phase 4 adds languages.

2. **Heuristic name-matching produces low-confidence edges even when the symbol is structurally certain.** RFC-005's rubric assigns confidence 90 to "name + arity match" and 70 to "name match + explicit alias." With SCIP's standardized symbol format including package identity, the same edge can be asserted with confidence 100 via exact package-qualified symbol equality. The heuristic is no longer the right tool when a definitive one exists.

3. **N-language repos require N×(N−1)/2 hardcoded pairs in the worst case.** A monorepo with TypeScript + Rust + Python + Go has 6 potential pair combinations. The current resolver hardcodes 3. Adding Java + Kotlin would bring the maximum to 15. This does not scale — but as §4 below shows, graph transitivity through C-as-lingua-franca means most pairs need no plugin at all.

This RFC defines the plugin contract, the registry mechanics, the multi-language semantics, the performance model, and the registration policy.

---

## Detailed Design

### 1. The `CrossLanguageBridge` trait

```rust
// travsr-ingest/src/bridges/mod.rs

use travsr_core::{Language, NodeId};
use crate::scip::ScipSymbol;

/// One implementor per (source_language, target_language, mechanism) triple.
/// A plugin is stateless and shareable across threads.
pub trait CrossLanguageBridge: Send + Sync + std::panic::UnwindSafe {
    /// Source language of the call site.
    fn source_language(&self) -> Language;

    /// Target language of the definition.
    fn target_language(&self) -> Language;

    /// Bridge mechanism identifier — stable string used in logs, metrics,
    /// and the EdgeKind::FFICall provenance field.
    fn mechanism(&self) -> &'static str;

    /// Expected SCIP scheme for the source-language indexer. Used by the
    /// registry's scheme-check guard to prevent intra-corpus
    /// mechanism-spoofing (e.g. a compromised `scip-java` emitting a symbol
    /// claiming to be `scip-typescript`). MUST match exactly — not a prefix
    /// or pattern.
    fn source_language_scheme(&self) -> &'static str;

    /// Expected SCIP scheme for the target-language indexer. Same role as
    /// `source_language_scheme` but for the dst side.
    fn target_language_scheme(&self) -> &'static str;

    /// Given two SCIP symbols — one from each language — can this bridge
    /// assert that they refer to the same callable across the FFI boundary?
    /// Returns None if the symbols are unrelated; Some(_) if they match.
    ///
    /// The registry has already verified corpus equality and scheme match
    /// before invoking this method (§3); the implementation only needs to
    /// reason about package + descriptor + mechanism-specific signals.
    fn resolve(&self, src: &ScipSymbol, dst: &ScipSymbol) -> Option<BridgeMatch>;
}

/// Result of a successful bridge match.
#[derive(Debug, Clone, Copy)]
pub struct BridgeMatch {
    /// Confidence score 0–100. See §5 confidence model.
    pub confidence: u8,
    /// Whether to emit a reverse edge in addition to the forward one.
    pub direction: Direction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Emit src → dst FFICall only.
    Unidirectional,
    /// Emit both src → dst (FFICall) and dst → src (RefCall) — used when
    /// confidence ≥ 80 per RFC-005 §edge-direction.
    Bidirectional,
}
```

**Why this shape:**

- `source_language` and `target_language` enable pre-filtering (§3 performance). The registry only invokes `resolve()` on symbol pairs from the matching languages.
- `mechanism()` is a static string so it is zero-cost in hot paths and stable across versions.
- `resolve()` takes two `ScipSymbol` references — not raw VNames — because the SCIP package-qualified symbol carries the structural information bridges need (§2). The plugin never sees graph-level state.
- Returning `Option<BridgeMatch>` lets bridges abstain on any pair they cannot assert; the registry then moves on to the next plugin.

### 2. Why SCIP symbols, not VNames

The plugin operates on `ScipSymbol` because the SCIP symbol format encodes exactly the information bridges need:

```
<scheme> <package-manager> <package-name> <package-version> <descriptor>
```

Example matched pair under the `napi` bridge:

```
src (TypeScript):  scip-typescript  npm    travsr-node  0.1.0  src/index.d.ts/getCallers().
dst (Rust):        scip-rust        cargo  travsr-node  0.1.0  src/lib.rs/get_callers().
```

The `napi` bridge plugin's `resolve()` checks:

1. Both symbols reference the same `(package-name, package-version)` after normalizing for napi-specific naming (e.g. `travsr-node` ↔ `travsr-node`).
2. The descriptors match after camelCase ↔ snake_case normalization.
3. The TypeScript side is in a `.d.ts` file under a directory that contains a `package.json` with a `"napi"` key, and the Rust side is in a crate whose `Cargo.toml` declares `crate-type = ["cdylib"]` and depends on `napi`.

The Kythe VName is derived **after** the bridge match by the surrounding registry — the plugin itself does not synthesize VNames. This keeps the corpus invariant (ADR-005, RFC-005 §corpus-invariant) enforced in one place rather than scattered across plugins.

**Three structural checks the registry performs before invoking `resolve()`, in this order (see §3 for the implementation):**

1. **Corpus equality** — `src.corpus() == dst.corpus()`. RFC-005 §corpus-invariant requires this; per-package pruning alone is insufficient because the same package name can exist in two different repos under multi-repo indexing.
2. **Scheme match** — `src.scheme == bridge.source_language_scheme()` and likewise for `dst`. This is the **security gate** and runs second, before the package pre-filter. A spoofed package name would pass the package equality check if the scheme gate came after it — an attacker who controls `scip-java` output could emit a symbol with the Rust package name, and a package-first check would then invoke a Rust bridge on attacker-controlled data. Placing the scheme check second prevents this.
3. **Package equality within the same corpus** — `src.package == dst.package`. The **performance pre-filter**; combined with #1 above, this cuts ~99% of candidate pairs. It runs after the scheme gate because it is an efficiency optimization, not a correctness or security gate.

These three checks are the registry's responsibility, not the plugin's. A bridge plugin assumes all three have already passed and reasons only about descriptor matching and mechanism-specific signals.

### 3. The `BridgeRegistry`

```rust
// travsr-ingest/src/bridges/mod.rs

pub struct BridgeRegistry {
    bridges: Vec<Box<dyn CrossLanguageBridge>>,
}

impl BridgeRegistry {
    /// Built-in registry assembled at daemon startup.
    pub fn builtin() -> Self {
        Self {
            bridges: vec![
                Box::new(NapiBridge::new()),
                Box::new(Pyo3Bridge::new()),
                Box::new(CgoBridge::new()),
                // Phase 4 additions land here:
                // Box::new(JniBridge::new()),
                // Box::new(KotlinJvmBridge::new()),
            ],
        }
    }

    /// Resolve all FFI edges across the merged SCIP index in one pass.
    /// Returns FFICall edges; caller is responsible for upserting into the graph.
    ///
    /// Complexity: O(B · S_src · S_dst) in the worst case, where B is the
    /// number of bridges, and S_src / S_dst are the symbol counts of the
    /// languages each bridge connects. The pre-filter in §performance below
    /// keeps this tractable on real polyglot repos.
    pub fn resolve_all(&self, symbols: &[ScipSymbol]) -> Vec<FFICallEdge> {
        // Pre-filter by language to avoid O(S²) blow-up.
        let by_lang = group_by_language(symbols);

        let mut edges = Vec::new();
        for bridge in &self.bridges {
            let srcs = by_lang.get(&bridge.source_language());
            let dsts = by_lang.get(&bridge.target_language());

            let (Some(srcs), Some(dsts)) = (srcs, dsts) else { continue };

            for src in srcs {
                for dst in dsts {
                    // MANDATORY corpus invariant (RFC-005 §corpus-invariant,
                    // ADR-005). Two SCIP symbols can share a package name across
                    // corpora — e.g. `npm:react` in two different repos under
                    // multi-repo indexing. Per-package pruning alone is NOT
                    // sufficient; the corpus check must come first.
                    if src.corpus() != dst.corpus() { continue; }

                    // MANDATORY scheme check — security gate (RFC-009 §2).
                    // Prevents a compromised SCIP indexer from forging symbols
                    // claiming to belong to another language's scheme (intra-corpus
                    // mechanism-spoofing). Must run BEFORE the package pre-filter:
                    // a spoofed package name would pass the package equality check
                    // if the scheme gate came after it.
                    if src.scheme != bridge.source_language_scheme() { continue; }
                    if dst.scheme != bridge.target_language_scheme() { continue; }

                    // Per-package pruning: only compare symbols from the same
                    // SCIP package within the same corpus. Cuts ~99% of pairs.
                    // Runs last because it is a performance filter, not a security
                    // gate — the scheme check above already excluded forgeries.
                    if src.package != dst.package { continue; }

                    if let Some(m) = bridge.resolve(src, dst) {
                        edges.push(FFICallEdge {
                            src_vname: src.to_vname(),
                            dst_vname: dst.to_vname(),
                            mechanism: bridge.mechanism(),
                            confidence: m.confidence,
                            direction: m.direction,
                        });
                    }
                }
            }
        }
        edges
    }
}
```

The registry is stateless aside from the plugin list. It is constructed once on daemon startup and shared via `Arc<BridgeRegistry>` across the indexing thread pool.

### 4. Multi-language repos — N languages, one pass

A repo with K > 2 languages does **not** require K×(K−1)/2 bridge plugins. The registry's design and graph traversal cooperate:

**Direct edges** are produced by every applicable plugin in one pass. A 4-language repo (TypeScript + Rust + Python + Go) with the three built-in bridges produces:

```
napi  : TS  → Rust   (every TS↔Rust napi binding)
pyo3  : Py  → Rust   (every Python↔Rust pyo3 binding)
cgo   : Go  → C      (every Go↔C cgo binding)
```

No TS↔Python or TS↔Go bridge plugin exists, and **none is needed**.

**Transitive paths** are produced by graph traversal at retrieval time:

```
get_blast_radius(python_fn) →
   reverse BFS through FFICall + RefCall edges →
   reaches Rust callers → reaches TypeScript callers via napi edges
```

PCST (`get_execution_path`) likewise finds connecting subgraphs through intermediate languages without needing a direct plugin.

**C as lingua franca.** Most non-trivial FFI in real codebases goes through C (Python/Ruby/Lua/R extension APIs all bottom out at C; Rust and Go expose C ABIs). One plugin per language bridging to C provides full N-language coverage transitively. Direct plugins are only required for pairs that bypass C: N-API (TS↔Rust direct), JVM languages (Java↔Kotlin direct), Swift↔ObjC (Obj-C runtime).

**Concrete transitive path example (unblocks the S15 `cgo` bridge implementation).** A Go function that calls a C function that is implemented in Rust produces this edge chain:

```
Go fn  →[FFICall, mechanism="cgo", confidence=100]→  C synthetic node
C synthetic node  →[RefCall, confidence=100]→  Rust fn
```

C synthetic nodes are introduced by the `cgo` bridge plugin:

- `NodeKind::CExport`
- `corpus` = the Go repo's corpus (same corpus as the `cgo` call site — corpus invariant preserved)
- `language = "c"` (the scheme field in the synthetic SCIP symbol)
- `descriptor` = the exported C symbol name from the `//export <name>` directive

The `pyo3` bridge (Python → Rust) follows the same pattern when Rust exposes a C-compatible ABI layer. At retrieval time, `get_blast_radius(rust_fn)` traverses `RefCall` edges inbound to the C synthetic node, then `FFICall` edges inbound to the C synthetic node from Go, finding the Go call sites — no direct Go↔Rust plugin required.

Traversal depth limit for transitive cross-language paths: the same BFS depth-3 limit (MVP) or PPR convergence (production) as same-language traversal. No special casing for synthetic C nodes.

The implication for Phase 4 scope: the planned plugin set (napi, pyo3, cgo, jni, kotlin-jvm) plus future swift-objc covers the vast majority of polyglot repos without combinatorial explosion.

### 5. Confidence model

RFC-005 §confidence-rubric defined a 0–95 scale for the LSIF-based resolver, with most edges landing at 70–90 due to name-matching ambiguity. With SCIP, the model splits cleanly:

| Tier | Score | When it applies | Example |
|------|-------|-----------------|---------|
| Structural | 100 | Both sides reference same SCIP `(package-name, package-version, descriptor)` after mechanism-specific normalization | TypeScript `.d.ts` generated by `napi build` matched to `#[napi]` Rust fn |
| Strong | 95 | Same package, descriptors match by explicit alias annotation | `#[pyo3(name = "compute")]` Rust ↔ `compute()` in `.pyi` stub |
| Inferred | 70–90 | Same package, descriptors match heuristically (camelCase ↔ snake_case, arity) | Bridge that lacks an explicit alias mechanism |
| Probabilistic | 30–60 | Symbol name match without package or stub anchor | Python dynamic import without `.pyi` stub |
| Below threshold | < 30 | Not emitted | — |

Each bridge declares its **default emit threshold** as a constant; the daemon's `travsr.toml` `[bridges.<mechanism>]` section can override per-bridge.

The `EdgeKind::FFICall` PPR weight (RFC-005 §edge-type) of 0.85 is replaced by a **confidence-derived weight**: `weight = 0.70 + 0.30 · (confidence / 100)`. A confidence-100 edge weights 1.00 (same as `RefCall`); a confidence-30 edge weights 0.79. This makes high-confidence cross-language edges first-class citizens in PPR traversal while still down-weighting probabilistic ones.

### 6. Plugin registration — static for Phase 4

Plugins are statically registered: each is a struct in `travsr-ingest/src/bridges/<mechanism>.rs`, added to the `Vec` in `BridgeRegistry::builtin()`. Enabling/disabling is configured per-repo in `travsr.toml`:

```toml
[bridges]
enabled = ["napi", "pyo3", "cgo", "jni"]   # subset of compiled-in bridges

[bridges.napi]
emit_threshold = 70                         # overrides default

[bridges.pyo3]
require_pyi_stubs = true                    # bridge-specific config
```

**Why not dynamic loading.** A `dlopen`-based plugin can read the entire indexed graph and exfiltrate it. The trust model for dynamic plugins is non-trivial (signing, sandboxing, capability restriction) and properly belongs to a separate ADR with Principal Security Engineer sign-off. Static registration covers the known FFI mechanisms; dynamic loading is deferred to Phase 5+ if community demand justifies the security work.

### 7. Built-in plugins at Phase 4 entry

| Bridge       | Source language | Target language | Mechanism signal source | Mechanism signal target |
|--------------|-----------------|-----------------|-------------------------|-------------------------|
| `napi`       | TypeScript      | Rust            | `.d.ts` in package with `"napi"` key | `#[napi]` / `#[napi(js_name = "x")]` |
| `pyo3`       | Python          | Rust            | `.pyi` stub + `from pkg import _rust_module` | `#[pyfunction]` / `#[pyo3(name = "x")]` |
| `cgo`        | Go              | C (synthetic)   | `C.<name>` after `import "C"` | `//export <name>` directive |
| `jni`        | Java            | C (synthetic)   | `native` method declaration | `JNIEXPORT` C signature |
| `kotlin-jvm` | Kotlin          | Java            | Bytecode-level method reference | JVM class method |

The first three are migrations of the existing `ffi_resolver.rs` logic, refactored to fit the trait. The last two are new in Phase 4 (S15 + S16).

### 8. Failure modes

All bridge failures are non-fatal. The registry returns whatever edges it produces; an exception in a single plugin's `resolve()` is caught and logged at `tracing::warn!` with the bridge name and symbol pair. The indexing pipeline produces the structural graph regardless.

A plugin that panics is **dropped from the registry for the remainder of the daemon's lifetime** and reported via the daemon's error channel. Subsequent indexing runs do not retry the plugin until the daemon restarts. This protects against a single buggy plugin degrading every subsequent indexing run.

### 9. Testing strategy

Each bridge ships with four test tiers:

1. **Unit tests in the plugin module.** Pure `resolve()` tests with hand-constructed `ScipSymbol` pairs covering the confidence rubric tiers, including **negative-path cases**. Lives in `travsr-ingest/src/bridges/<mechanism>.rs` `#[cfg(test)]`.

   Required negative cases (per bridge):
   - A symbol pair with confidence < 30 must produce `None` (no edge emitted).
   - A symbol pair from the wrong language pair for this bridge must produce `None`.
   - A symbol pair with matching package but mismatched descriptor must produce `None` (no edge emitted on descriptor mismatch alone).

   A bridge that emits edges for all inputs would fail these cases. Without explicit negative-path tests, a broken bridge that always returns `Some(BridgeMatch { confidence: 100, .. })` would pass the positive-path tests.

2. **Integration fixtures.** A polyglot fixture per bridge under `travsr-ingest/tests/fixtures/<mechanism>/` with both sides of the FFI binding plus a golden JSON of expected edges. Snapshot-managed with `insta` (per RFC-003 §6 convention).

3. **Property-based tests for the registry.** Three required properties (using `proptest`), asserting post-B1 fix invariants:
   - `prop_resolve_all_never_emits_cross_corpus_edge` — for any symbol pair with differing corpora, `resolve_all` emits zero edges.
   - `prop_resolve_all_rejects_scheme_mismatch` — for any symbol pair where `src.scheme != bridge.source_language_scheme()`, `resolve_all` emits zero edges.
   - `prop_resolve_all_package_mismatch_zero_edges` — for any symbol pair within the same corpus and with correct schemes but differing packages, `resolve_all` emits zero edges.
   
   Also verifies the RFC-005 §symmetry-property invariant for every (src_lang, dst_lang) pair in the polyglot fixture, generalized to the plugin set.

4. **Transitive traversal fixture (added in S15).** The combined polyglot fixture (TypeScript + Rust + Python + Go + Java) must include at least one golden assertion for a **transitive** cross-language path — e.g. calling `get_blast_radius` on a Rust function that is exposed via both `napi` (TS→Rust) and `pyo3` (Python→Rust) and asserting that the result reaches TypeScript and Python call sites through the intermediate Rust node. This tier guards against a regression where direct FFICall edges are emitted correctly but the traversal engine fails to cross language boundaries.

   Required golden assertion:
   - Input: the Rust `get_callers` entrypoint in the fixture (bound via napi to TypeScript and via pyo3 to Python).
   - Expected: `get_blast_radius` result includes at least one TypeScript call site and at least one Python call site, reached via FFICall edges, within depth-3 BFS.

---

## Alternatives Considered

### A. Trait object–free design via an enum of bridge kinds

Rejected. The `Box<dyn CrossLanguageBridge>` indirection costs one virtual call per `resolve()` invocation — negligible compared to the per-symbol hashing inside `resolve()`. An enum would require touching every variant when adding a new bridge, defeating the plugin model.

### B. One plugin per language pair, no `mechanism` field

Rejected. A single language pair can have multiple FFI mechanisms (Python↔Rust via PyO3 or via cffi/ctypes for non-Rust-backed C extensions; TypeScript↔Rust via napi or via WebAssembly). The `mechanism` field is necessary to disambiguate and to allow per-mechanism configuration.

### C. Resolve at indexing time per file vs. post-merge

Rejected. The current `ffi_resolver.rs` resolves after merging because cross-language matches require symbols from both sides to be present. Per-file resolution would miss any cross-file binding (the common case for napi: the `.d.ts` and `lib.rs` are in different files and often different packages). Post-merge resolution is correct; this RFC preserves it.

### D. Confidence as an enum (Definite | Strong | Inferred | Probabilistic)

Rejected. The numeric scale interoperates with the PPR weight formula (§5) and with RFC-005's existing `edges.confidence` column (a `u8`). Discretizing would lose information and complicate the migration from the existing resolver.

### E. Cross-corpus FFI plugins

Rejected for this RFC. RFC-005's corpus invariant (`src.corpus == dst.corpus`) holds here. Cross-corpus FFI (legitimate cross-repo napi bindings) is a real use case but requires Graph RBAC trust modeling, which is Phase 5 (S18) scope. Deferred.

---

## Drawbacks

- **Per-bridge configuration surface in `travsr.toml`.** Each bridge introduces optional config keys (thresholds, stub-requirement flags). Users may not know which to set. Mitigated by sensible defaults per bridge and documentation in the `travsr-ingest` README.
- **The `mechanism` string namespace is global.** Two unrelated plugins could collide on the same name (e.g. two interpretations of "napi"). Mitigated by reserving the name registry in the `travsr-ingest` crate root and requiring a code review for any addition.
- **SCIP-only.** Bridges operate on `ScipSymbol`. A language that only has an LSIF indexer cannot participate in cross-language resolution without a SCIP shim. This is by design — the standardized symbol format is the architectural enabler — but it means LSIF-only languages (legacy or niche) are limited to the structural Phase A graph. Documented in ADR-009.
- **Registry behavior changes when a plugin is added or removed.** A repo's FFICall edges depend on which plugins were enabled at the last indexing run. Re-running with a different plugin set produces different edges. Mitigated by recording the plugin set in the `.travsr/graph.db` metadata table; `travsr status` surfaces drift.

---

## Unresolved Questions

1. **Should bridges expose a `version()` method?** When a bridge's `resolve()` logic changes between Travsr releases, edges from older runs are silently outdated. A version field could trigger a re-resolution pass. Deferred until we have a real instance of bridge-logic evolution to motivate the design.

2. **Symmetric bridges vs. unidirectional ones.** The current design names a `source_language` and a `target_language`, implying direction. Some bridges (kotlin-jvm) are genuinely symmetric. Should we allow `source_language() == target_language()` or `Option<Language>` for both sides? Pending input from the Kotlin S16 work.

3. **What about Wasm?** WebAssembly bindings (e.g. TypeScript ↔ Rust via `wasm-bindgen`) are functionally another FFI mechanism. The plugin model accommodates a `wasm-bindgen` bridge cleanly, but the SCIP indexers for the Wasm side are nascent. Tracking as a Phase 5 candidate.

4. **Per-bridge sandboxing.** ADR-006 sandboxes rust-analyzer because the LSIF emitter executes user code. Bridge plugins are pure Rust running in the daemon process — no sandbox required for the plugin itself. But the **invoker** that produces the SCIP index (e.g. `scip-java`) does execute user code in some cases (build script execution). Each invoker needs its own ADR-006-style appendix; see RFC-008 §unresolved-questions.

---

## References

- RFC-005 — Cross-Language Edge Resolution Protocol (the predecessor this RFC generalizes)
- RFC-008 — Multi-Language Extension Architecture (umbrella context)
- ADR-005 — Per-Language Corpus Naming (corpus invariant)
- ADR-006 — rust-analyzer Subprocess Trust Model (per-invoker trust precedent)
- ADR-009 — SCIP vs LSIF Wire Format (why bridges operate on `ScipSymbol`)
- ADR-010 — `travsr-ingest` Crate Boundary (where this code lives)
- [SCIP symbol format](https://github.com/sourcegraph/scip/blob/main/scip.proto)
