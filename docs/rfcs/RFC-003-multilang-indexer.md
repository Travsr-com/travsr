# RFC-003: Multi-Language Indexer Architecture

**Status:** Proposed
**Author:** Principal Architect
**Date:** 2026-05-21
**Issue:** #114
**Phase:** 3 (Sprint 8 implementation gate)
**Crate(s) affected:** `travsr-core`, `travsr-indexer`, `travsr-daemon`
**Depends on:** PR #143 ([ADR-005] per-language corpus naming) — must merge first.
**Related:** RFC-002 (VName signature format versioning), ADR-002 (edge provenance), ADR-006 ([rust-analyzer subprocess trust model] — required before Phase B implementation begins in Sprint 9).

---

## Summary

Define the architecture for adding multi-language support to the Travsr indexer
pipeline. The goal is to support Rust and Python in Phase 3 (and Go + cross-language
linking in Phase 4) without breaking the existing TypeScript path or the Kythe
VName identity model.

---

## Motivation

`travsr-indexer` is currently TypeScript-only. The multi-language extension must:

1. Preserve the existing TypeScript parse pipeline without modification.
2. Allow per-language parsers to be added independently with no changes to
   the storage or retrieval layers.
3. Maintain globally unique Kythe VNames across all languages in the same repo
   (a Rust `fn main` in `crates/travsr-cli` must not collide with a TypeScript
   `function main` in `packages/travsr-vscode`).
4. Lay the foundation for Phase 4 cross-language edges (e.g. FFI call sites).

---

## Detailed Design

### 1. `Language` enum in `travsr-core`

```rust
/// Identifies the source language of a node or edge.
/// Used for VName addressing and parser dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub enum Language {
    TypeScript,
    Rust,
    Python,
}

impl Language {
    /// Map a file extension to a `Language`.
    /// Returns `None` for unrecognised extensions — callers skip those files.
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "ts" | "tsx" | "mts" | "cts" => Some(Self::TypeScript),
            "rs"                          => Some(Self::Rust),
            "py" | "pyi"                  => Some(Self::Python),
            _                             => None,
        }
    }

    /// Human-readable string stored on `nodes.language` column.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TypeScript => "typescript",
            Self::Rust       => "rust",
            Self::Python     => "python",
        }
    }
}
```

The `language` field is added to `Node` (schema migration v4). Existing TypeScript
rows are backfilled with `"typescript"` on migration (see `CORE-201`).

**Why no per-language signature prefix?** Cross-language collision is already
prevented by two mechanisms inherited from RFC-002 and the schema:

1. RFC-002's `SIGNATURE_FORMAT_VERSION: u8` domain-separates the entire BLAKE3
   input. Any future change to the signature vocabulary bumps this byte and
   triggers a full re-index — the existing invariant is sufficient.
2. The `language` field is part of the row identity at the storage layer. A
   Rust `fn main` and a TypeScript `function main` in the same repo produce
   distinct rows because their `language` columns differ, even if their VName
   signatures happened to hash to the same value (probability `~2^-64`).

Adding a per-language prefix inside the signature string would therefore be
redundant. More importantly, introducing one *now* would change the signature
vocabulary, requiring `SIGNATURE_FORMAT_VERSION` to be bumped from 1 → 2 and
forcing every existing `.travsr/graph.db` to be fully re-indexed. There is no
correctness benefit to justify that cost.

### 2. `LanguageIndexer` trait in `travsr-indexer`

```rust
use std::path::Path;
use travsr_core::Language;
use crate::ParseOutput;

/// Implemented once per language. The dispatcher holds a Vec of boxed
/// indexers and routes each file to the matching one.
pub trait LanguageIndexer: Send + Sync {
    /// Which language this indexer handles.
    fn language(&self) -> Language;

    /// Parse a single source file and return nodes + edges.
    ///
    /// `vname_path` — repo-relative path used as VName `path` component.
    /// `corpus`     — repo identity from ADR-005 corpus naming convention.
    /// `package`    — ADR-005 Rule 2 sub-unit identity:
    ///                  TypeScript: npm package name from package.json
    ///                  Rust:       Cargo package name ([package] name)
    ///                  Python:     top-level package dir (contains __init__.py)
    ///
    /// The caller (`parse_file_with_vname`) is responsible for pre-computing
    /// `package` once per crate/module before iterating over files — this
    /// avoids per-file Cargo.toml / __init__.py discovery and keeps parse()
    /// O(1) with respect to filesystem access.
    fn parse(
        &self,
        path: &Path,
        source: &str,
        vname_path: &str,
        corpus: &str,
        package: &str,
    ) -> anyhow::Result<ParseOutput>;
}
```

Concrete implementors:

| Struct | Language | Crate feature |
|---|---|---|
| `TypeScriptIndexer` | `TypeScript` | always on |
| `RustIndexer` | `Rust` | always on (tree-sitter-rust) |
| `PythonIndexer` | `Python` | always on (tree-sitter-python) |

### 3. File-extension dispatcher

`Indexer::parse_file_with_vname` is refactored to use `Language::from_extension`.
The caller pre-computes `package` from the nearest `Cargo.toml` / `package.json` /
`__init__.py` once per directory tree before iterating over files in that unit:

```rust
pub fn parse_file_with_vname(
    &self,
    abs_path: &Path,
    vname_path: &str,
    package: &str,     // pre-computed by caller; see §package-resolution below
) -> Result<ParseOutput, IndexError> {
    let src = read_source(abs_path)?;
    let ext = abs_path.extension().and_then(|e| e.to_str()).unwrap_or("");
    match Language::from_extension(ext) {
        Some(Language::TypeScript) =>
            self.typescript.parse(abs_path, &src, vname_path, &self.corpus, package)
                .map_err(|e| IndexError::Parse { file: abs_path.to_string_lossy().into(), message: e.to_string() }),
        Some(Language::Rust) =>
            self.rust.parse(abs_path, &src, vname_path, &self.corpus, package)
                .map_err(|e| IndexError::Parse { file: abs_path.to_string_lossy().into(), message: e.to_string() }),
        Some(Language::Python) =>
            self.python.parse(abs_path, &src, vname_path, &self.corpus, package)
                .map_err(|e| IndexError::Parse { file: abs_path.to_string_lossy().into(), message: e.to_string() }),
        None => Ok(ParseOutput::default()),
    }
}
```

The `Indexer` struct gains three fields:
```rust
pub struct Indexer {
    pub corpus: String,
    typescript: TypeScriptIndexer,
    rust:       RustIndexer,
    python:     PythonIndexer,
}
```

No dynamic dispatch is used — the enum match is monomorphic and zero-cost.

**Lifecycle:** `Indexer` is constructed once per repo on daemon startup (or
once per `travsr index` invocation in CLI mode) and shared via `Arc<Indexer>`
across walker threads. The per-language sub-indexers are stateless and
`Send + Sync`, so a single shared instance suffices.

**Package resolution (pre-computed by the walker, not by parse()):**

| Language | Resolution rule |
|---|---|
| Rust | Walk up from `abs_path` to the nearest `Cargo.toml`; read `[package] name`. Done once per crate root, memoised for all files under it. |
| TypeScript | Read `name` from the nearest `package.json`. Done once per package dir. |
| Python | Walk up from `abs_path` along *contiguous* `__init__.py`-containing directories; use the topmost such directory's name (see §Python edge cases below for the precise rule). For namespace packages (PEP 420 — no `__init__.py` anywhere on the chain), fall back to the repo-root-relative first path component. Done once per package root, memoised. |

**Python edge cases (worked examples):**

| Layout | `abs_path` | Resolved `package` | Why |
|---|---|---|---|
| `repo/util.py` | `repo/util.py` | `"util"` | No package dir; single-file module. `package` = file stem. |
| `repo/scripts/build.py` (no `__init__.py` in `scripts/`) | `repo/scripts/build.py` | `"scripts"` | PEP 420 namespace fallback: first path component below repo root. |
| `repo/src/foo/__init__.py` + `repo/src/foo/bar.py` (no `__init__.py` in `src/`) | `repo/src/foo/bar.py` | `"foo"` | Walk up *contiguously*: `foo/` has `__init__.py`, `src/` does not — stop at `foo`. Avoids returning `"src"` for the common src-layout. |
| `repo/a/__init__.py` + `repo/a/b/__init__.py` + `repo/a/b/c.py` | `repo/a/b/c.py` | `"a"` | Topmost contiguous `__init__.py`-bearing directory is `a/`. |
| `repo/ns/sub/x.py` (no `__init__.py` anywhere — PEP 420 namespace pkg) | `repo/ns/sub/x.py` | `"ns"` | First path component. The namespace boundary is ambiguous in PEP 420; we pick the repo-root-relative top to keep VNames stable across the repo. |

The rule **"highest *contiguous* `__init__.py` directory"** prevents `src/` from being incorrectly returned for the standard src-layout, which is a common cause of cross-repo VName collisions.

**Note on `#[non_exhaustive]` scope:** `#[non_exhaustive]` on `Language` prevents
exhaustive pattern matching in *external* crates only. Within the travsr workspace
itself the Rust compiler still enforces exhaustive matches on `Language`. This means
adding `Go` in Phase 4 will produce a compile error on every incomplete `match`
inside travsr — the compiler itself enforces that all three dispatcher sites are
updated together. This is intentional.

### 4. Tree-sitter vs. LSIF provenance (ADR-002 compliance)

> **Security precondition for Phase B.** `rust-analyzer --lsif` executes
> `build.rs` and proc-macros, which is arbitrary code execution in the indexed
> repo's context. Sprint 9 implementation of Phase B is gated on **ADR-006**
> (rust-analyzer subprocess trust model) being merged first. That ADR defines
> the sandbox, network egress policy, and opt-in trust model. Until ADR-006
> lands, only Phase A (Tree-sitter, no subprocess execution) ships.

Each language uses a two-phase approach:

```
Phase A — Tree-sitter (Sprint 8):
  structural nodes only:
    fn/struct/enum/trait/impl/type/const/static (Rust)
    function/class/import/variable (TypeScript — existing)
    function/class/import/variable (Python — Sprint 10)
  edges:
    DefinesBinding  — file defines symbol
    ChildOf         — module/class hierarchy
    Imports         — from_extension resolved to target VName

Phase B — LSIF / rust-analyzer (Sprint 9):
  semantic edges:
    RefCall         — call sites (resolved by compiler)
    TypeRef         — type references
  provenance field set to "lsif" per ADR-002
```

Nodes from Phase A and Phase B share the same VName. Phase B edges are
annotated with `provenance = "lsif"` so they can be distinguished from
Tree-sitter structural edges (`provenance = "tree-sitter"`) in queries.

**Invariant:** A Tree-sitter node and an LSIF node for the same symbol have
identical VNames. The graph merge is an upsert on VName hash — no duplicates.

**Upsert semantics (CORE-201 must implement):**

- *Nodes*: `INSERT OR REPLACE` keyed on VName hash. Phase B may encounter
  symbols that exist in the compiled crate but are not present in the source
  AST (e.g. derive-generated methods, proc-macro expansions in Rust; the
  equivalent in Python is `@dataclass`-synthesised methods, `attrs.define`
  attributes, etc.). Tree-sitter parses the attributes / decorators but cannot
  expand them — the resulting symbols only become visible to the compiler /
  type checker. The upsert must therefore also handle bare `INSERT` for VNames
  not yet in the graph — Phase B creates stub nodes for these.
- *Edges*: Merge key is `(src_vname_hash, dst_vname_hash, edge_kind)`.
  Edges are **deduplicated**, not additive — inserting the same
  `(src, dst, RefCall)` edge twice produces one row. Phase B edges carry
  `provenance = "lsif"`; Phase A edges carry `provenance = "tree-sitter"`.
  If the same edge exists in both phases (e.g. a `DefinesBinding` edge
  emitted by both Tree-sitter and LSIF), the LSIF provenance wins on conflict
  (higher-fidelity source).

### 5. Corpus naming

Corpus names follow ADR-005 (per-language corpus naming) — see PR #143. Summary:

- All languages in the same repo share the **same corpus** (`{git-remote-basename}`).
- Language identity is carried by the `language` column on `nodes` and (per §1
  above) by RFC-002's `SIGNATURE_FORMAT_VERSION` byte at the hash domain layer.
- **Do not encode language in corpus** — this is required for Phase 4
  cross-language edges to work (both endpoints must share a corpus).

Rust example: `VName { corpus: "travsr", root: "", path: "crates/travsr-core/src/lib.rs", language: "rust", signature: "fn:my_function" }`

(The `signature` is the symbol-level identifier emitted by the language indexer
— e.g. `"fn:foo"`, `"struct:Foo"`. The signature is hashed together with the
other VName components plus the `SIGNATURE_FORMAT_VERSION` byte per RFC-002.)

### 6. Test fixture strategy

```
crates/travsr-indexer/tests/
  fixtures/
    typescript/          ← existing
      simple.ts
      simple.golden.json
    rust/                ← Sprint 8
      simple.rs          — fn, struct, use, mod
      simple.golden.json — hand-curated expected nodes/edges
    python/              ← Sprint 10
      simple.py
      simple.golden.json
```

Each golden file is a JSON object with `nodes: [...]` and `edges: [...]`
matching the `ParseOutput` schema. Tests assert equality after normalising
node order by VName hash. Snapshot management uses [`insta`](https://crates.io/crates/insta)
to match the project-wide convention; `cargo insta review` is the workflow
for accepted snapshot changes.

Integration smoke test (`QA-201`): spawn `travsr init` in a temp clone of
`crates/travsr-core` (pure Rust, no TypeScript) and assert `node_count > 0`.

### 7. Crate dependency changes

| Crate | Change |
|---|---|
| `travsr-core` | Add `Language` enum (no new external deps) |
| `travsr-indexer` | Add `tree-sitter-rust = "=0.23.x"`, `tree-sitter-python = "=0.23.x"` (pinned to exact patch; cargo-deny advisory check runs in CI — Tree-sitter grammar CVEs have shipped historically) |
| `travsr-daemon` | No new deps; dispatcher change only |
| `travsr-store` | Schema migration v4 adds `language TEXT NOT NULL DEFAULT 'typescript'` column |

---

## Alternatives Considered

### A. Dynamic dispatch via `Box<dyn LanguageIndexer>`

Rejected: introduces a heap allocation per file parse and virtual dispatch.
The set of languages is statically known at compile time. The enum-match
dispatcher is zero-cost and the compiler inlines each branch.

### B. Encode language in corpus name (`travsr-rust`, `travsr-typescript`)

Rejected: breaks Phase 4 cross-language edges. Both sides of a
`calls_foreign` edge must share a corpus or the VName ceases to be globally
addressable under the Kythe model. The `Language` enum on `Node` and the
`signature_prefix` in the VName signature provide sufficient discrimination
without polluting the corpus field.

### C. Separate index files per language (`.travsr/graph-rust.db`)

Rejected: query paths would need to fan-out across N databases for any
cross-language query. Single graph file preserves the O(1) lookup property.

---

## Drawbacks

- `tree-sitter-rust` and `tree-sitter-python` add ~3 MB to the binary (C
  grammar sources compiled in). Acceptable — well under the 25 MB tarball gate.
- Schema migration v4 touches the `nodes` table. The backfill
  (`UPDATE nodes SET language = 'typescript' WHERE language IS NULL`) runs in
  O(N) time. Existing users see a one-time migration on next `travsr init`
  (identical to the v3 migration pattern already shipped).

---

## Unresolved Questions

1. **Python dynamic call graph** — `tree-sitter-python` gives structural nodes
   but Python's dynamic dispatch makes call-edge resolution probabilistic.
   Deferred to Sprint 10 / Open Problem #1 in the Principal Architect doc.

2. **Go grammar** — `tree-sitter-go` exists; Go support is Phase 4 scope.
   The `Language` enum uses `#[non_exhaustive]` to allow adding `Go` without
   a breaking change.

3. **Cross-language edges** — FFI call sites (Rust ↔ Python via PyO3) require
   a `CallsForeign` edge kind. Deferred to Phase 4 (`CROSS-LANG-001`).
