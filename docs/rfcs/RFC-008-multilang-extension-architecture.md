# RFC-008: Multi-Language Extension Architecture

> **Superseded in part (#457):** Kùzu was dropped as a storage backend; SQLite+WAL is the only backend. Kùzu references below are kept for historical context. See ADR-018.

**Status:** Draft
**Author:** Tech Lead
**Date:** 2026-05-27
**Phase:** 4 (Sprints 12–16) — design gate; implementation in Sprint 12
**Crate(s) affected:** `travsr-core`, `travsr-indexer`, `travsr-ingest` (new), `travsr-daemon`
**Supersedes (in part):** RFC-003 §2 (hardcoded `LanguageIndexer` implementors), RFC-003 §3 (hardcoded enum-match dispatcher)
**Related:** RFC-005 (cross-language edge resolution), RFC-009 (cross-language bridge plugin system), ADR-005 (per-language corpus naming), ADR-006 (rust-analyzer subprocess trust), ADR-009 (SCIP vs LSIF wire format), ADR-010 (`travsr-ingest` crate boundary)

---

## Summary

Generalize Travsr's two-phase indexer (Tree-sitter structural + LSIF semantic) so that adding a new language no longer requires modifying source code in `travsr-indexer`. Phase A becomes config-driven: a TOML-declared language descriptor drives a generic Tree-sitter indexer. Phase B moves to a new `travsr-ingest` crate that hosts pluggable SCIP/LSIF invokers and a cross-language bridge plugin registry (specified in RFC-009). Existing TypeScript, Rust, and Python pipelines are preserved unchanged; new languages slot in additively.

---

## Motivation

RFC-003 specified a per-language `LanguageIndexer` trait but hardcoded three concrete implementors (`TypeScriptIndexer`, `RustIndexer`, `PythonIndexer`) and a three-arm enum-match dispatcher. This was correct for Phase 3 (one-shot Rust + Python addition) but does not scale to the Phase 4 commitment of "two new languages green on the correctness suite" or the Phase 5 ambition of a community-extensible indexer.

The current pain points:

1. **Adding a language requires editing five files** — the `Language` enum, the dispatcher match arms, `Indexer` struct fields, the Tree-sitter parser module, and the test fixture wiring. Every addition is a breaking change to `travsr-indexer`'s public surface.
2. **Phase B (LSIF) lives in the same crate as Phase A (Tree-sitter)**, so the subprocess trust boundary defined by ADR-006 is not enforceable at the crate level. A `cargo deny` policy that restricts subprocess-spawning dependencies cannot be applied cleanly.
3. **Cross-language resolution (RFC-005) hardcodes three FFI pairs** (TS↔Rust napi, Python↔Rust pyo3, Go↔C cgo). Adding Java↔Kotlin, Swift↔ObjC, or Ruby↔C requires modifying `ffi_resolver.rs` rather than registering a plugin.
4. **No standardized symbol identity across language indexers.** LSIF's per-emitter symbol formats force `ffi_resolver.rs` into heuristic name-matching with confidence scores 30–95 (RFC-005 §confidence rubric). A standardized symbol format would make most cross-language edges structurally certain rather than inferred.

This RFC defines the architectural envelope that resolves all four pain points. The detailed bridge plugin design lives in RFC-009; the format choice lives in ADR-009; the crate split lives in ADR-010.

---

## Detailed Design

### 1. Two layers, two extension mechanisms

```
┌─────────────────────────────────────────────────────────────────┐
│                  travsr-indexer (Phase A)                       │
│                                                                 │
│  LanguageDescriptor (TOML) → GenericTreeSitterIndexer           │
│                                  ↓                              │
│                          ParseOutput (structural)               │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼ merge
┌─────────────────────────────────────────────────────────────────┐
│                  travsr-ingest (Phase B) — NEW                  │
│                                                                 │
│  LanguageInvoker trait (per-language plugin; format() chooses) │
│         ↓                                                       │
│  lsif.rs / scip.rs  (format parsers — see ADR-009 §rule-3)      │
│         ↓                                                       │
│  ParseOutput (semantic)                                         │
│         ↓                                                       │
│  BridgeRegistry  (cross-language plugins — see RFC-009)         │
│         ↓                                                       │
│  ParseOutput (cross-language FFICall edges)                     │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼ merge (upsert by VName, ADR-002 provenance)
                       Graph (SQLite / Kùzu)
```

**Phase A** continues to ship Tree-sitter structural parses for every language as the always-available baseline. **Phase B** is opt-in per language (and per-repo per ADR-006) and produces semantic edges when a SCIP or LSIF indexer is available.

### 2. Phase A — `LanguageDescriptor` config format

A language is described by a TOML stanza, loaded at indexer construction time. The first iteration ships built-in descriptors for the five MVP languages; additional descriptors can be registered at runtime via `travsr.toml`.

```toml
# Example: built-in descriptors live in travsr-indexer/langs/*.toml
# User descriptors live in <repo>/.travsr/langs/*.toml
[language]
name = "kotlin"
extensions = ["kt", "kts"]
tree_sitter_grammar = "tree-sitter-kotlin"  # crate name, statically linked
language_fn = "tree_sitter_kotlin::language"

[package_resolution]
strategy = "build_file_ancestor"
build_files = ["build.gradle", "build.gradle.kts", "settings.gradle"]
package_key = "rootProject.name"            # grammar-specific extraction rule

[nodes]
# Tree-sitter node kinds → travsr Node kinds.
# Right side maps to the existing core::NodeKind enum.
function_declaration = "Function"
class_declaration    = "Class"
object_declaration   = "Class"
property_declaration = "Variable"
import_header        = "Import"

[edges]
# How to derive structural edges from the parsed AST.
defines_binding = ["function_declaration", "class_declaration", "property_declaration"]
child_of        = { parent = "class_declaration", children = ["function_declaration", "property_declaration"] }
imports         = { node = "import_header", target_extraction = "identifier_path" }
```

**Why TOML, not Rust code?** TOML descriptors keep the per-language extension surface declarative. Community contributors can submit a `.toml` file and a grammar dependency without writing Rust. The downside — TOML cannot express grammar-specific quirks (e.g. JSX's interleaved expression/element grammar) — is handled by an optional `LanguageDescriptor.custom_module` field that names a Rust module providing post-processing hooks. We expect ~80% of languages to need no custom module; the remaining ~20% drop down to Rust.

**Descriptor trust model (normative — resolves Q2 from earlier drafts).** Following the precedent of ADR-006 Rule 1 (*"daemon never enables LSIF indexing based on content found inside the repo itself"*), descriptor loading is gated as follows:

| Source | Loaded? | `custom_module` permitted? |
|--------|---------|---------------------------|
| `travsr-indexer/langs/*.toml` (built-in, shipped with the crate) | Yes, unconditionally | Yes — `custom_module` must resolve to a statically-linked module in the `travsr-indexer` crate |
| `~/.config/travsr/langs/*.toml` (user-local config) | Yes, unconditionally | Yes — `custom_module` must resolve to a built-in module path (no user-supplied code; statically-linked only). Validation is against a `const` slice of permitted module paths compiled into the binary at build time — not a runtime string match. This prevents bypass via future crate additions that happen to share a path prefix. |
| `<repo>/.travsr/langs/*.toml` (repo-local) | **No, refused by default.** Requires `travsr config set descriptors.trust <repo-path> true` (same primitive as ADR-006 `rust-analyzer.trust`) | **Never** — `custom_module` is rejected with a hard error in any repo-local descriptor regardless of trust setting |

A repo-local descriptor without a corresponding home-directory trust entry causes the daemon to emit a `tracing::warn!` and skip the descriptor. The trust entry is per-canonical-corpus (ARCH-102), matching ADR-006's `rust-analyzer.trust.<canonical-corpus>` schema.

A malicious repo committing `.travsr/langs/evil.toml` with `custom_module = "evil::backdoor"` produces zero code execution: (a) the repo-local descriptor is refused until the user opts in via home-config, (b) even with opt-in, `custom_module` in a non-built-in descriptor is hard-rejected. Phase 5 may relax this once a plugin signing / capability model exists; the ADR-016 follow-up tracks that work.

**Built-in descriptors at the start of Phase 4:**

| Language    | Descriptor source         | Custom module needed? |
|-------------|---------------------------|------------------------|
| TypeScript  | `langs/typescript.toml`   | Yes (JSX interleaving) |
| Rust        | `langs/rust.toml`         | No                     |
| Python      | `langs/python.toml`       | No                     |
| Go          | `langs/go.toml`           | No                     |
| Java        | `langs/java.toml`         | No                     |
| Kotlin      | `langs/kotlin.toml`       | No                     |

Migrating existing hardcoded indexers to descriptors is part of the Sprint 12 deliverable. Behavior must be byte-identical against the existing golden fixtures (RFC-003 §6) — this is the regression gate.

### 3. Phase B — `travsr-ingest` crate (full design in ADR-010)

A new crate at the same dependency tier as `travsr-indexer`:

```
travsr-core
  ├── travsr-indexer   (Tree-sitter, Phase A)
  ├── travsr-ingest    (LSIF/SCIP, Phase B)  ← new
  └── travsr-store
          └── travsr-retrieval
                  └── travsr-mcp
                          └── travsr-daemon  ← orchestrates indexer + ingest
                                  └── travsr-cli
```

`travsr-ingest → depends on travsr-core only`. This preserves the dependency rule precedent set by `travsr-indexer`. The split is justified on three grounds, in priority order:

1. **Subprocess trust boundary.** ADR-006 sandbox rules apply to everything in `travsr-ingest` and nowhere in `travsr-indexer`. A crate boundary makes that auditable.
2. **Dependency footprint isolation.** SCIP requires `prost` (protobuf). LSIF requires custom JSON-LD streaming logic. Neither dependency should bleed into the Tree-sitter parsing path.
3. **Independent release cadence.** Ingest plugins evolve with their upstream toolchains (rust-analyzer, pyright, scip-typescript); decoupling versioning is valuable.

Full justification, content split, and migration plan are in ADR-010.

### 4. Phase B — Wire format choice (full decision in ADR-009)

New languages adopt **SCIP** as the wire format between the language toolchain and `travsr-ingest`. Existing languages (TypeScript, Rust, Python) keep their LSIF pipelines indefinitely — no migration is required because both formats decode to the same `ParseOutput`.

The two parsers coexist in `travsr-ingest`:

```rust
// travsr-ingest/src/lib.rs
pub mod lsif;  // LSIF JSON-LD parser — existing logic, relocated
pub mod scip;  // SCIP protobuf parser — new
// Both produce ParseOutput; downstream is format-agnostic.
```

The dispatcher selects based on the registered `Invoker` for each language. See ADR-009 for the trade-off matrix, migration policy, and the rationale for not retrofitting existing pipelines to SCIP.

### 5. Phase B — Cross-language plugin registry (full design in RFC-009)

The fixed three-pair `ffi_resolver.rs` from RFC-005 is replaced by a plugin registry:

```rust
// travsr-ingest/src/bridges/mod.rs
pub trait CrossLanguageBridge: Send + Sync {
    fn source_language(&self) -> Language;
    fn target_language(&self) -> Language;
    fn mechanism(&self) -> &'static str;  // "napi", "pyo3", "jni", "cgo", ...
    fn resolve(&self, src: &ScipSymbol, dst: &ScipSymbol) -> Option<BridgeMatch>;
}
```

Built-in bridges at Phase 4 entry: `napi` (TS↔Rust), `pyo3` (Python↔Rust), `cgo` (Go↔C). Phase 4 adds: `jni` (Java↔C), `kotlin-jvm` (Kotlin↔Java). Phase 5 considers community plugins.

**Key insight from the discussion that informs RFC-009:** With SCIP's standardized symbol format including package identity, most bridges reduce to package-qualified symbol matching rather than annotation heuristics. Confidence scores drop out for static FFI boundaries (N-API, cgo with `//export`, PyO3 with `.pyi` stubs); they remain only for genuinely probabilistic cases (Python dynamic imports without stubs).

**Multi-language repos.** The registry fires every applicable bridge against the merged SCIP index in one pass. A 4-language monorepo (e.g. TypeScript + Rust + Python + Go) produces all valid FFICall edges in one resolution pass without per-pair special-casing. Transitive cross-language paths (TS → Rust → Python) are resolved by graph traversal, not by adding direct plugins — C-as-lingua-franca makes this tractable. See RFC-009 §4 for the full multi-language semantics.

### 6. Backward compatibility

| Existing surface | Status |
|---|---|
| `Language::TypeScript / Rust / Python / Go` enum variants | Unchanged; new variants added behind `#[non_exhaustive]` (RFC-003 §1) |
| `LanguageIndexer` trait | Unchanged; `GenericTreeSitterIndexer` is just another implementor |
| `ParseOutput` schema | Unchanged |
| `Node`, `Edge`, `EdgeKind`, `VName` in `travsr-core` | Unchanged |
| SQLite / Kùzu schema | Unchanged (no migration needed for this RFC; the v4 / v6 migrations from RFC-003 and RFC-005 are sufficient) |
| MCP tool API (`get_dependencies`, `get_callers`, `get_blast_radius`, etc.) | Unchanged |
| `lsif.rs` location | **Moved** from `travsr-indexer` to `travsr-ingest` (see ADR-010 §migration) |
| `ffi_resolver.rs` location | **Moved** from `travsr-indexer` to `travsr-ingest/src/bridges/` |

The `lsif.rs` and `ffi_resolver.rs` moves are the only sources of churn for downstream consumers. The `travsr-daemon` orchestration code is updated in the same PR. No external crate currently imports these modules directly — they are internal to the indexer pipeline.

### 7. Phased rollout

| Sprint | Deliverable | Security gates |
|--------|-------------|----------------|
| S12 | Create `travsr-ingest` crate; move `lsif.rs` + `ffi_resolver.rs`; daemon wires both crates. Zero functional change — pure structural refactor with green fixtures. | Principal Security Engineer review of `travsr-ingest` crate boundary (ADR-010). |
| S13 | Implement `LanguageDescriptor` TOML loader + `GenericTreeSitterIndexer`. Migrate Go to descriptor. TypeScript, Rust, and Python remain hardcoded (custom modules) for now. | **ADR-016 (TOML descriptor trust model) MUST be merged in the same sprint.** Loader must reject repo-local descriptors without home-config opt-in and reject `custom_module` outside built-in descriptors. |
| S14 | Add SCIP parser (`scip.rs`) and unified `LanguageInvoker` trait (per ADR-009 §rule-3). First SCIP language: Java via `scip-java`. Migrate Python to descriptor. | **ADR-011 (`scip-java` subprocess trust model) MUST be merged in the same sprint.** Java SCIP invoker MUST NOT land without sandbox model signed off by Principal Security Engineer. |
| S15 | Extract `CrossLanguageBridge` trait from `ffi_resolver.rs`; convert existing three pairs to plugins; add `jni` bridge. | **ADR-015 (bridge plugin panic-isolation specification) MUST be merged in the same sprint.** **ADR-012 (`scip-typescript` trust model) IF `scip-typescript` is adopted in this sprint.** |
| S16 | Phase 4 exit: two new languages (Java + Kotlin) on the correctness suite. | **ADR-013 (`scip-kotlin` subprocess trust model) MUST be merged in the same sprint.** |

Each sprint exit is gated on the existing golden fixtures (RFC-003 §6) plus new fixtures for the added language.

**Normative invoker-introduction rule (resolves Q3 from earlier drafts):** Any sprint landing a new subprocess invoker (LSIF or SCIP) MUST be blocked on the corresponding ADR-006-style subprocess trust appendix being merged in the same sprint, with explicit sign-off from the Principal Security Engineer. This is not a "track as follow-up" item — it is a hard merge gate. A sprint that ships a new invoker without its trust ADR is non-conforming and must be reverted.

---

## Alternatives Considered

### A. One large RFC instead of RFC-008 + RFC-009 + ADR-009 + ADR-010

Rejected. The discussion identified four distinct decisions (config-driven Phase A, crate split, format choice, plugin system) with different audiences and review cadences. Splitting follows the precedent of RFC-003 (architecture) + RFC-005 (cross-language) being separate. ADRs capture the binary choices (SCIP vs LSIF, split vs don't split); RFCs capture the designs.

### B. Keep all Phase B logic in `travsr-indexer`

Rejected. See ADR-010 — the subprocess trust boundary is the strongest single argument for the split. Additional benefits: dependency footprint isolation and independent release cadence.

### C. Migrate existing TypeScript / Rust / Python pipelines to SCIP

Rejected for this RFC. See ADR-009 §migration policy. Forcing existing users to re-index would burn the always-fresh property (CLAUDE.md non-negotiable principle #2) for no correctness gain. Coexistence is the right policy.

### D. Dynamic plugin loading from the start (.so / .dll)

Rejected for Phase 4. See RFC-009 §plugin registration. The set of meaningful FFI mechanisms is small and well-known; static registration covers it. Dynamic loading is a Phase 5+ consideration and requires Principal Security Engineer sign-off on a plugin trust model.

### E. Drop LSIF entirely in favor of SCIP-only

Rejected. rust-analyzer's LSIF output remains the highest-fidelity source of Rust call graphs and is actively maintained. `scip-rust` exists but is less mature. Tearing down a working pipeline for ecosystem uniformity is the wrong trade-off; the parsers coexist cheaply.

---

## Drawbacks

- **Crate count grows from 7 to 8.** Build times increase by ~3–5% per `cargo check` on cold builds. Mitigated by aggressive use of workspace-level feature flags and by `travsr-ingest` not being on the `travsr-cli` critical path (it ships as a dynamic-load capability of the daemon, not the CLI).
- **TOML descriptors cannot express every grammar quirk.** The `custom_module` escape hatch (§2) covers this but is a hole in the "config-only" promise. We expect this to apply to ~20% of languages.
- **The relocation of `lsif.rs` and `ffi_resolver.rs` is a code-organization breaking change** internal to the project. Mitigated by landing the move (S12) before any new functionality lands on top of it.
- **SCIP adoption increases the dependency surface** by `prost` (protobuf) — a well-maintained crate, but a new transitive dependency in the Travsr supply chain. See ADR-009 §supply-chain.

---

## Unresolved Questions

1. **TOML descriptor versioning.** When the descriptor schema evolves (new field added), older third-party descriptors should continue to load with sensible defaults. **Resolved direction:** require `descriptor_version` from day-1 of S13 — refuse to load any descriptor without it. Easier than retrofitting. The exact forward-compat policy (semver vs monotonic integer) is finalized in the S13 implementation PR.

2. **Cross-language confidence threshold tuning per bridge.** RFC-005 set a global threshold of 30. With SCIP's standardized symbols, the threshold should likely be 100 for static FFI bridges and a lower number for dynamic ones. RFC-009 §confidence model proposes per-bridge thresholds; final values will come out of S15 fixture work.

3. **Custom module gating in Phase 5.** §2 normatively restricts `custom_module` to built-in descriptors for Phase 4. Phase 5 may relax this once a plugin signing / capability model exists. ADR-016 (TOML descriptor trust model) tracks the Phase 4 work; a separate Phase 5 ADR will define the relaxation policy if/when community demand justifies it.

*Note:* The previous Q2 (custom module trust) and Q3 (SCIP invoker sandboxing) have been promoted to normative decisions in §2 and §7 respectively. They are no longer unresolved.

---

## References

- RFC-003 — Multi-Language Indexer Architecture (the predecessor this RFC generalizes)
- RFC-005 — Cross-Language Edge Resolution Protocol (the predecessor RFC-009 generalizes)
- RFC-009 — Cross-Language Bridge Plugin System (this RFC's detailed sibling)
- ADR-005 — Per-Language Corpus Naming (corpus invariant relied upon for cross-language plugins)
- ADR-006 — rust-analyzer Subprocess Trust Model (sandbox precedent for all `travsr-ingest` invokers)
- ADR-009 — SCIP vs LSIF Wire Format (format decision)
- ADR-010 — `travsr-ingest` Crate Boundary (crate split decision)
- [SCIP specification](https://github.com/sourcegraph/scip) (external)
- [LSIF specification](https://microsoft.github.io/language-server-protocol/specifications/lsif/0.6.0/specification/) (external)
