# ADR-009: SCIP vs LSIF as Wire Format for Semantic Indexing

**Date:** 2026-05-27
**Status:** Proposed
**Phase:** 4 (Sprint 14)
**Author:** Tech Lead
**Supersedes:** N/A
**Related:** RFC-005 (cross-language edge resolution), RFC-008 (multi-language extension architecture), RFC-009 (cross-language bridge plugin system), ADR-002 (edge provenance policy), ADR-010 (`travsr-ingest` crate boundary)

---

## Context

Travsr's semantic indexing layer (Phase B in RFC-008) consumes output from external language toolchains and parses it into `ParseOutput`. The current implementation accepts only **LSIF** (Language Server Index Format, Microsoft) and ships with three LSIF emitters: `tsc --lsif` for TypeScript, `rust-analyzer lsif` for Rust, and `pyright`'s LSIF output for Python. The format choice was made in Phase 2 (S4–S7) before alternatives were mature.

Phase 4 (S12–S16) adds Java, Kotlin, and more languages. Two relevant changes have happened upstream since the original choice:

1. **SCIP (Source Code Intelligence Protocol, Sourcegraph)** has reached production maturity. SCIP is a protobuf-based format explicitly designed to replace LSIF for code intelligence indexers. It standardizes the symbol identity format across languages — something LSIF deliberately left unspecified.
2. **The LSIF ecosystem is in maintenance mode.** Microsoft has shifted focus to language-specific successors (e.g. TypeScript's own indexing API). The LSIF specification has not been revised in over two years.

This ADR decides which wire format new Travsr language integrations should use, and what to do about the existing LSIF-based pipelines.

---

## Decision

### Rule 1 — New languages adopt SCIP

Any language added to `travsr-ingest` in Phase 4 or later uses **SCIP** as the wire format between the external toolchain and the Travsr ingest pipeline.

The first language to land under this rule is Java (S14), via `scip-java`. Kotlin follows in S16 via `scip-kotlin`. Subsequent additions (Ruby, Scala, C#, etc.) follow the same rule.

### Rule 2 — Existing LSIF pipelines are preserved unchanged

TypeScript, Rust, and Python continue to ingest LSIF. There is no migration mandate, no deprecation timeline, and no feature gate that prefers SCIP over LSIF for these languages.

Reasons:

- The existing pipelines are correct, tested against extensive fixtures (RFC-003 §6), and shipping to users today.
- Forcing a migration would invalidate every existing `.travsr/graph.db` and require a full re-index on update — directly violating CLAUDE.md non-negotiable principle #2 (always fresh).
- Both rust-analyzer's LSIF output (highest-fidelity Rust call graph available) and `scip-rust` (newer, less mature) coexist in the upstream ecosystem. There is no quality justification to switch Rust today.
- The two parsers (`lsif.rs` and `scip.rs`) both decode to the same `ParseOutput`. Coexistence costs one extra parser module; switching costs every user's index.

### Rule 3 — Both parsers live in `travsr-ingest`

The format-specific code is encapsulated in the new crate (per ADR-010):

```
travsr-ingest/src/
  lsif.rs    ← existing LSIF JSON-LD parser, relocated from travsr-indexer
  scip.rs    ← new SCIP protobuf parser
  invoker.rs ← LsifInvoker / ScipInvoker traits
  langs/
    typescript.rs ← LsifInvoker for tsc --lsif
    rust.rs       ← LsifInvoker for rust-analyzer lsif
    python.rs     ← LsifInvoker for pyright LSIF
    java.rs       ← ScipInvoker for scip-java       (new, S14)
    kotlin.rs     ← ScipInvoker for scip-kotlin     (new, S16)
```

Each language's invoker declares which parser to feed its output to. The dispatcher in `travsr-ingest::lib` chooses based on the invoker's `format()` method:

```rust
pub enum WireFormat { Lsif, Scip }

pub trait LanguageInvoker: Send + Sync {
    fn language(&self) -> Language;
    fn format(&self) -> WireFormat;
    fn invoke(&self, root: &Path) -> Result<Box<dyn Read>, InvokerError>;
    fn trust_level(&self) -> TrustLevel;  // see ADR-006
}
```

A single trait covers both — there is no separate `LsifInvoker` / `ScipInvoker` hierarchy at the API level. The format is a property of the invoker.

### Rule 4 — Bridges (RFC-009) operate on SCIP symbols only

The cross-language bridge plugin system (RFC-009) operates on `ScipSymbol`. LSIF-only languages cannot participate in cross-language resolution without a shim that produces SCIP-equivalent symbols.

Two shim paths exist:

1. **LSIF → SCIP transcoder.** Sourcegraph maintains [`lsif-go-to-scip`](https://github.com/sourcegraph/scip) and similar tools. For LSIF-only languages where a transcoder exists, the invoker can run LSIF → SCIP at ingest time. The fidelity loss is minimal for the symbol fields bridges care about.
2. **Synthetic SCIP symbol generation.** For LSIF-only languages without a transcoder, the LSIF parser can synthesize a SCIP-equivalent symbol string from the LSIF result type (package, version, range). This is a fallback with reduced confidence for cross-language matches.

Neither shim is required for Phase 4 — the three existing LSIF languages (TypeScript, Rust, Python) already participate in cross-language resolution via the legacy code path being migrated in S15 (RFC-009 §built-in-plugins at Phase 4 entry).

### Rule 5 — Provenance tagging per ADR-002

Per ADR-002 (edge provenance policy), every edge from a semantic indexer is tagged with its source. The provenance string now distinguishes formats:

| Source                          | `edges.provenance` |
|---------------------------------|--------------------|
| Tree-sitter (Phase A)           | `"tree-sitter"`    |
| LSIF (rust-analyzer, tsc, etc.) | `"lsif"`           |
| SCIP (scip-java, etc.)          | `"scip"`           |
| Cross-language bridge plugin    | `"bridge:<mech>"`  |

This enables queries like "show only LSIF-provenance edges" and supports future debugging when format-specific bugs surface. Schema migration v7 adds the new provenance values to the existing enum — no column changes required.

---

## Comparison

| Dimension | LSIF | SCIP | Decision driver |
|-----------|------|------|-----------------|
| Wire format | JSON-LD (text, verbose) | Protobuf (binary, compact) | SCIP wins on size and parse speed |
| Symbol identity | Per-emitter, ad hoc | Standardized package-qualified format | **Critical for cross-language (RFC-009)** |
| Streaming support | Vertex-by-vertex, fragile | First-class | SCIP wins for large repos |
| Ecosystem maintenance | Microsoft (low activity since 2023) | Sourcegraph (actively developed) | SCIP wins on maintenance velocity |
| Language coverage (mature indexers) | TypeScript, Rust, Python, Go | TS, Rust, Python, Go, Java, Kotlin, Ruby, Scala, C# | SCIP wins on breadth |
| Existing Travsr code | 3 working pipelines | 0 pipelines | LSIF wins on incumbency |
| Dependency footprint | `serde_json` (already present) | `prost` (new transitive dep) | LSIF wins on supply chain |
| Spec stability | Frozen | Active evolution | Mixed — frozen avoids breakage but blocks improvement |

The breadth and standardized symbol format outweigh the supply chain and incumbency costs **for new languages**. The incumbency advantage and the always-fresh constraint outweigh the breadth gain **for existing languages**. Hence the bifurcation.

---

## Consequences

### Positive

- **Cross-language resolution becomes structurally certain** for static FFI boundaries (RFC-009 §confidence-model). SCIP's package-qualified symbol format enables confidence-100 edges where the heuristic resolver previously assigned 70–90.
- **Phase 4 language additions are unblocked.** `scip-java` and `scip-kotlin` are mature; the equivalent LSIF emitters do not exist for these languages.
- **Existing pipelines are insulated.** No user-visible change for TypeScript, Rust, Python repos. Always-fresh property preserved.
- **Single API surface.** `LanguageInvoker` is the only trait downstream code (the daemon orchestrator) sees. Format is an implementation detail of the invoker.
- **Provenance audit trail.** ADR-002 compliance extended to distinguish LSIF and SCIP provenance, supporting future debugging and format-specific quality metrics.

### Negative

- **Two parsers to maintain.** `lsif.rs` and `scip.rs` both ship and both need test coverage. Mitigated by the parsers being narrow (each is ~500–1000 lines) and stable (the upstream formats change rarely).
- **`prost` enters the supply chain.** A new transitive dependency for protobuf decoding. `prost` is well-maintained by the Tokio project and widely used in the Rust ecosystem; the supply-chain risk is comparable to existing `serde_json`. `cargo deny` policy will pin `prost` to a minor version per RFC-003 §crate-dependency-changes precedent.
- **Cross-language coverage is incomplete for LSIF-only languages until shims land.** If a Phase 4 LSIF language (e.g. a community contribution adding C++ via LSIF) wants to participate in cross-language resolution, it needs the §rule-4 shim. The shim work is not in the Phase 4 critical path — it lands when a real user need arises.
- **Mental overhead for contributors.** A new contributor must learn that TypeScript uses LSIF but Java uses SCIP. The `travsr-ingest` README will document the rule and the rationale; the invoker's `format()` method makes the choice explicit in code.

---

## Migration Policy

**For existing TypeScript, Rust, Python users:** Nothing changes. The S12 crate split (per ADR-010) is a code relocation only; the LSIF parsing logic is byte-identical pre- and post-move. Existing `.travsr/graph.db` files continue to work without re-indexing.

**For new SCIP-using languages:** Users opt in by enabling the language in `travsr.toml`:

```toml
[lsif.java]
enabled = true
invoker = "scip-java"
trust = true   # per ADR-006 — required for any subprocess invoker
```

The first run on a new repo indexes Java via SCIP automatically. No global configuration is required.

**Future SCIP-for-existing-language opt-in:** If a user wants to migrate their Rust pipeline from rust-analyzer LSIF to `scip-rust`, they can override the default invoker:

```toml
[lsif.rust]
invoker = "scip-rust"   # explicit override; default is "rust-analyzer-lsif"
```

This re-indexes the repo on next daemon start. The graph is rebuilt from the SCIP source. Provenance edges flip from `"lsif"` to `"scip"`. This is supported but unrecommended until `scip-rust` reaches feature parity with rust-analyzer's LSIF output.

---

## Threat Model Implications

The SCIP/LSIF choice is largely orthogonal to security — both are output from a subprocess that ADR-006 already sandboxes. However:

| Threat | Effect of format choice |
|--------|-------------------------|
| Malicious indexer subprocess produces malformed output | SCIP's protobuf parser is harder to attack than JSON-LD (length-prefixed, no nested string parsing). SCIP wins. |
| Resource exhaustion via large index | SCIP's binary encoding is 5–10× smaller than equivalent LSIF JSON-LD. Parser memory pressure is lower. SCIP wins. |
| Symbol-spoofing attack (malicious indexer emits cross-language reference to attacker-controlled symbol) | Same risk in both formats. The corpus invariant (RFC-005, RFC-009 §3) blocks cross-corpus spoofing. Wash. |
| Supply-chain compromise of parser dependency | `serde_json` (LSIF) is at version 1.x, widely audited. `prost` (SCIP) is at version 0.13.x, also widely audited. Risk is comparable. |

The Principal Security Engineer's sign-off on `travsr-ingest` (per ADR-010 §security) covers both parsers under one review.

---

## Open Questions

1. **`scip-typescript` adoption timing.** Sourcegraph's TypeScript SCIP indexer is mature. At some point the `tsc --lsif` pipeline could be replaced by `scip-typescript` for better cross-language resolution with napi-bound Rust crates. Decision deferred until S15 fixture work surfaces a concrete pain point.
2. **Shim transcoders.** Whether to ship LSIF → SCIP transcoders as built-in invokers (so LSIF-only languages can participate in bridges) or as separate utilities. Deferred to S15 — driven by the first real user request.
3. **Format-aware retrieval tuning.** Edges from SCIP carry richer symbol metadata than LSIF. Future MCP tools may surface this difference (e.g. `get_callers` could return SCIP package info when available). Out of scope for this ADR.

---

## References

- RFC-005 — Cross-Language Edge Resolution Protocol
- RFC-008 — Multi-Language Extension Architecture
- RFC-009 — Cross-Language Bridge Plugin System
- ADR-002 — Edge Provenance Policy
- ADR-006 — rust-analyzer Subprocess Trust Model
- ADR-010 — `travsr-ingest` Crate Boundary
- [SCIP specification](https://github.com/sourcegraph/scip)
- [SCIP `scip.proto`](https://github.com/sourcegraph/scip/blob/main/scip.proto)
- [LSIF 0.6.0 specification](https://microsoft.github.io/language-server-protocol/specifications/lsif/0.6.0/specification/)
- [prost protobuf library](https://github.com/tokio-rs/prost)
