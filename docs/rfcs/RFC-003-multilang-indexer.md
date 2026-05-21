# RFC-003: Multi-Language Indexer Architecture

**Status:** Accepted
**Author:** Principal Architect
**Date:** 2026-05-21
**Issue:** #114
**Crate(s) affected:** `travsr-core`, `travsr-indexer`, `travsr-daemon`

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

    /// The signature-version prefix used in RFC-002 VName signatures.
    /// Ensures no hash collision between same-named symbols in different
    /// languages within the same corpus.
    pub fn signature_prefix(self) -> &'static str {
        match self {
            Self::TypeScript => "ts:v1:",
            Self::Rust       => "rust:v1:",
            Self::Python     => "py:v1:",
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
    fn parse(
        &self,
        path: &Path,
        source: &str,
        vname_path: &str,
        corpus: &str,
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

`Indexer::parse_file_with_vname` is refactored to use `Language::from_extension`:

```rust
pub fn parse_file_with_vname(
    &self,
    abs_path: &Path,
    vname_path: &str,
) -> Result<ParseOutput, IndexError> {
    let ext = abs_path.extension().and_then(|e| e.to_str()).unwrap_or("");
    match Language::from_extension(ext) {
        Some(Language::TypeScript) =>
            self.typescript.parse(abs_path, &read_source(abs_path)?, vname_path, &self.corpus)
                .map_err(|e| IndexError::Parse { file: abs_path.to_string_lossy().into(), message: e.to_string() }),
        Some(Language::Rust) =>
            self.rust.parse(abs_path, &read_source(abs_path)?, vname_path, &self.corpus)
                .map_err(|e| IndexError::Parse { file: abs_path.to_string_lossy().into(), message: e.to_string() }),
        Some(Language::Python) =>
            self.python.parse(abs_path, &read_source(abs_path)?, vname_path, &self.corpus)
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

### 4. Tree-sitter vs. LSIF provenance (ADR-002 compliance)

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

### 5. Corpus naming

Corpus names follow ADR-005 (per-language corpus naming). Summary:

- All languages in the same repo share the **same corpus** (`{git-remote-basename}`).
- Language identity is carried by `Language::signature_prefix` in the VName
  signature and the `language` column on `nodes`.
- **Do not encode language in corpus** — this is required for Phase 4
  cross-language edges to work (both endpoints must share a corpus).

Rust: `VName { corpus: "travsr", root: "", path: "crates/travsr-core/src/lib.rs", language: "rust", signature: "rust:v1:{blake3}" }`

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
node order by VName hash.

Integration smoke test (`QA-201`): spawn `travsr init` in a temp clone of
`crates/travsr-core` (pure Rust, no TypeScript) and assert `node_count > 0`.

### 7. Crate dependency changes

| Crate | Change |
|---|---|
| `travsr-core` | Add `Language` enum (no new external deps) |
| `travsr-indexer` | Add `tree-sitter-rust = "0.23"`, `tree-sitter-python = "0.23"` |
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
