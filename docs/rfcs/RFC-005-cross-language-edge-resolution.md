# RFC-005: Cross-Language Edge Resolution Protocol

**Status:** Accepted
**Author:** Solution Architect
**Date:** 2026-05-24
**Crate(s) affected:** `travsr-core`, `travsr-store`, `travsr-indexer`
**Blocks:** Issue #171 (S13)

## Summary

Defines how Travsr resolves foreign-function-interface (FFI) calls across language boundaries, emitting `EdgeKind::FFICall` edges with confidence scores so AI agents can traverse TS↔Rust, Python↔Rust, and Go↔C call chains.

## Motivation

Each per-language indexer produces a disjoint subgraph. Without cross-language edges, PPR and PCST traversal cannot follow `TS callsite → Rust impl`, defeating the blast-radius calculation for any polyglot codebase.

## Detailed Design

### Edge type

`EdgeKind::FFICall` is a **unit variant** (not data-carrying). Confidence lives on `Edge.confidence: Option<u8>` and in the `edges.confidence` column (migration v6). This keeps `EdgeKind` `Copy` and avoids `PRIMARY KEY (src, dst, kind)` contamination.

PPR weight: **0.85** (between `RefCall` at 1.00 and `DefinesBinding` at 0.70). See ADR-003 amendment.

### VName canonicalization

Each side keeps its native VName. The resolver emits a directed `FFICall` edge connecting them — it does NOT unify VNames across language boundaries (that would break Kythe invariants).

### Corpus invariant — REQUIRED

The resolver **only** emits FFI edges where `src.corpus == dst.corpus`. Cross-corpus FFI is silently dropped (confidence forced to 0). This prevents confidence-spoofing attacks where attacker-controlled Rust code claims to bind a TS module in another corpus.

### Edge direction

`FFICall` is directional: call-site → implementation, same convention as `RefCall`. A reverse `RefCall` edge is additionally emitted when confidence ≥ 80.

### Confidence rubric

| Score | Criterion |
|---|---|
| 95 | Name + arity + parameter-type compatible |
| 90 | Name + arity match |
| 70 | Name match + explicit alias (`js_name`, `pyo3(name)`) |
| 50 | Name only, arity unknown |
| 30 | Heuristic camelCase↔snake_case match |
| 0  | No edge emitted (below threshold) |

Default emit threshold: 30 (tunable in `[ffi]` section of `travsr.toml`).

### Pipeline placement

After all per-language indexers complete for a single parse invocation, before store commit:
1. Each per-language parser emits `FfiMarker` records alongside nodes/edges
2. `ffi_resolver::Resolver` runs a second pass over the merged `ParseOutput`
3. Resolver emits `Edge::ffi_call(src, dst, confidence)` for each matched pair

### FFI pairs

| Pair | Source signal | Target signal |
|---|---|---|
| TS → Rust (napi-rs) | `.d.ts` in napi package (gated by `"napi"` key in `package.json`) | `#[napi]` or `#[napi(js_name = "x")]` on Rust fn |
| Python → Rust (PyO3) | `.pyi` stub adjacent to `_*.so` OR `from pkg import _rust_module` | `#[pyfunction]` or `#[pyo3(name = "x")]` on Rust fn |
| Go → C (cgo) | `C.<name>` callsite after `import "C"` | `//export <name>` directive on Go fn; C destination is a synthetic VName |

### Synthetic C VNames

Go's `//export` directives and cgo preambles produce synthetic C destination nodes with `VName { language: "c", path: "<cgo_synthetic>/<go_file_basename>", signature: "fn:<name>" }`. The `<cgo_synthetic>/` prefix distinguishes them from any future Phase 5 real C indexer output.

### Failure modes

All FFI resolution failures are non-fatal. The indexer returns tree-sitter output unmodified when FFI resolution fails. Failures are logged at `tracing::warn!`.

### Symmetry property

`prop_cross_lang_edge_symmetric`: for every FFI pair (src_lang, dst_lang) in the polyglot fixture, BFS from any node in src_lang reaches at least one node in dst_lang IFF BFS from that dst_lang node reaches back to src_lang. "Reaches" = depth ≤ 5 over `RefCall ∪ FFICall ∪ Depends`.

## Alternatives Considered

- **Unified VNames**: rejected — breaks Kythe invariants and couples node identity to the resolver's correctness.
- **EdgeKind::FFICall { confidence: u8 }** (data-carrying variant): rejected — contaminates `PRIMARY KEY (src, dst, kind)` and breaks `#[derive(Copy)]` on EdgeKind unnecessarily.

## Drawbacks

- Same-corpus malicious markers (e.g., a PR that adds a false `#[napi(js_name = "react")]`) produce misleading edges. This is equivalent to a malicious code change by someone with merge rights and is out of scope for the indexer.
- Cross-corpus FFI (legitimate cross-repo napi bindings) is not supported in S13. Revisit in S18 when Graph RBAC provides corpus-level trust modelling.

## Unresolved Questions

None blocking S13. Open items deferred to S18 STRIDE audit.
