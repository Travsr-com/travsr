# ADR-005: Per-Language Corpus Naming Convention

**Date:** 2026-05-21
**Status:** Proposed
**Issue:** #115
**Phase:** 3 (Sprint 8 prerequisite)
**Author:** Principal Architect
**Supersedes:** N/A (extends ARCH-102 without changing its corpus rule)
**Related:** ARCH-102 (TypeScript corpus naming, accepted — corpus shape unchanged by this ADR), RFC-002 (VName signature versioning), RFC-003 (multi-language indexer architecture, depends on this ADR)

---

## Context

ARCH-102 locked the corpus naming convention for all repos (TypeScript today, all languages going forward):

> ```
> host/org/repo
> ```
>
> All lowercase. No URL scheme prefix. No `.git` suffix. No trailing slash.
> `github.com/raj-rkv/travsr` is the canonical form.

Phase 3 adds Rust (Sprint 8) and Python (Sprint 10). A repo like Travsr contains both Rust crates and TypeScript packages. Without a locked convention, two parsers could emit VNames that collide, or that cannot be linked in Phase 4 cross-language queries.

This ADR answers:

1. What corpus does a multi-language repo use? (Same as ARCH-102: one `host/org/repo` for the whole repo.)
2. How do Rust crates inside a Cargo workspace stay disambiguated when they share a corpus?
3. How do Python packages stay disambiguated, including for the standard `src/` layout?
4. Where does language identity live in storage so MCP tools can filter by language?

---

## Decision

### Rule 1 — Corpus is `host/org/repo` for all languages (ARCH-102, unchanged)

All languages in the same repository share **one corpus**, derived from the `origin` remote URL per ARCH-102's `canonical_corpus()`:

```
github.com/raj-rkv/travsr  →  corpus = "github.com/raj-rkv/travsr"
gitlab.com/acme/backend    →  corpus = "gitlab.com/acme/backend"
(no remote / local only)   →  corpus = "local/<sanitised-basename>"
```

This is the same string the daemon writes today; nothing changes about existing TypeScript graphs. The ADR explicitly does *not* switch to "basename only" — basename collides across hosts (`github.com/a/backend` vs `gitlab.com/b/backend` would both become `"backend"`), and any change to the corpus string would alter every existing NodeId (corpus is part of the BLAKE3 input) and force a full re-index per RFC-002's `SIGNATURE_FORMAT_VERSION` invariant.

**Do not encode language in the corpus.** Encoding language (e.g. `travsr-rust`, `travsr-typescript`) would prevent Phase 4 cross-language edges: both endpoints of a `calls_foreign` edge must share a corpus for the VName to remain globally addressable under the Kythe model.

Language identity is carried in storage (Rule 3), **not** in the corpus and **not** in the VName signature.

### Rule 2 — `nodes.package` column carries the sub-unit identity

A new column `package TEXT` is added to the `nodes` table in schema migration v4 (sibling to the `language` column added by the same migration). This column stores the sub-unit identity for symbols inside a multi-package repo:

| Language | `package` value | Example |
|---|---|---|
| TypeScript | npm package name from the nearest `package.json` `name` field | `@travsr.com/travsr` |
| Rust | Cargo package name from the nearest `Cargo.toml` `[package] name` | `travsr-core` |
| Python | Top-level package name — see Python resolution below | `travsr` |

**Important: `package` is a stored column, *not* a sixth VName field.** It is **not** part of the BLAKE3 hash input. Adding it to the hash input would change every NodeId and force `SIGNATURE_FORMAT_VERSION` from 1 → 2 plus a full re-index — there is no correctness benefit to justify that cost. Symbol uniqueness inside the BLAKE3 input is already guaranteed by `path` (different crates live under different paths).

The column exists so MCP tools (`get_blast_radius`, `search_symbol`, `get_repo_map`) can scope queries to a single Cargo crate / npm package without parsing paths.

**Cargo workspace handling.** For a Cargo workspace, the corpus is still the workspace remote URL per Rule 1. Each crate is distinguished by its `package` column. So `fn open` in `travsr-store` and `fn open` in `travsr-indexer` produce distinct rows in storage because their `path` (and therefore VName signature hash) and `package` columns differ.

**Virtual workspace root edge case.** A workspace-level `Cargo.toml` may have no `[package]` section (virtual manifest). For files at the workspace root (e.g. a top-level `build.rs`), `package` falls back to the working-directory basename. In practice this affects very few files.

**Python package resolution.** Walk up from the source file along *contiguous* `__init__.py`-containing directories; use the topmost such directory's name. See RFC-003 §3 for five worked examples covering single-file modules, script directories without `__init__.py`, the standard `src/` layout, nested packages, and PEP 420 namespace packages. The rule is shared between this ADR and RFC-003 — that document is the single source of truth for edge cases.

### Rule 3 — `nodes.language` column carries language identity (schema migration v4)

The `language` column (added by CORE-201) stores the `Language::as_str()` value (`"typescript"`, `"rust"`, `"python"`). Backfill: existing rows are updated to `"typescript"` during migration v4.

There is **no per-language prefix in the VName signature string**. The earlier draft of this ADR proposed `Language::signature_prefix()` returning `"ts:v1:"`, `"rust:v1:"`, `"py:v1:"`, but that was redundant with the `language` column (already part of row identity at the storage layer) and would have changed the VName signature vocabulary — triggering a `SIGNATURE_FORMAT_VERSION` bump and a forced re-index of every existing TypeScript graph. RFC-003 also rejects this approach for the same reason. Cross-language disambiguation between a TypeScript `function main` and a Rust `fn main` is handled by the `language` column at the storage layer; the BLAKE3 hash itself is already domain-separated by `SIGNATURE_FORMAT_VERSION` per RFC-002.

### Rule 4 — Storage schema (v4 migration summary)

`CORE-201` (issue #116) is the single owner of the v4 migration. The migration must add two columns to `nodes` (and to `edges` for symmetry with provenance queries):

```sql
ALTER TABLE nodes ADD COLUMN language TEXT NOT NULL DEFAULT 'typescript';
ALTER TABLE nodes ADD COLUMN package  TEXT NOT NULL DEFAULT '';
ALTER TABLE edges ADD COLUMN language TEXT NOT NULL DEFAULT 'typescript';
CREATE INDEX idx_nodes_language ON nodes(language);
CREATE INDEX idx_nodes_package  ON nodes(package);
```

The defaults backfill existing v3 databases without churn (every node pre-Phase-3 is TypeScript with no specific package identity — empty string is the safe sentinel). Parity test (QA-010) must be extended to assert SQLite ↔ Kùzu equality on both new columns.

---

## Consequences

### Positive

- **No VName collisions** across languages in the same repo — handled by the `language` column at the storage layer without changing the hash input.
- **Phase 4 cross-language edges work** without schema changes: both endpoints share the same corpus and are disambiguated by the `language` + `package` columns.
- **Existing TypeScript indexing is byte-identical** — no NodeId churn, no re-indexing required. ARCH-102's corpus rule is preserved exactly.
- **Cargo workspaces query cleanly** — `WHERE package = 'travsr-store'` returns all symbols in one crate.

### Negative

- Clients filtering by language must inspect the `language` column rather than the corpus or the signature prefix. Trivial query complexity vs. language-per-corpus, and it is the correct trade-off (cross-language edges win).
- Storage size grows by two short text columns per node. Negligible relative to the existing row weight.

---

## Examples

### Travsr repo — mixed Rust + TypeScript (corpus per ARCH-102)

```
# Rust crate node
VName {
  corpus:    "github.com/raj-rkv/travsr",
  root:      "",
  path:      "crates/travsr-core/src/lib.rs",
  language:  "rust",
  signature: "<blake3-of-fn-open>"
}
# stored row also carries: package = "travsr-core"

# TypeScript package node
VName {
  corpus:    "github.com/raj-rkv/travsr",
  root:      "",
  path:      "packages/travsr-lsif-ts/src/index.ts",
  language:  "typescript",
  signature: "<blake3-of-export-default>"
}
# stored row also carries: package = "@travsr.com/travsr-lsif-ts"
```

### Python repo

```
VName {
  corpus:    "github.com/example/myapp",
  root:      "",
  path:      "src/myapp/models/user.py",
  language:  "python",
  signature: "<blake3-of-class-User>"
}
# stored row also carries: package = "myapp"   (NOT "src" — see Rule 2 + RFC-003 §3)
```

---

## Implementation Notes

- `Language` enum (with `as_str()`, `from_extension()`) is implemented in `travsr-core` by `CORE-201`. There is no `signature_prefix()` method.
- The Rust indexer reads `[package] name` by parsing the nearest `Cargo.toml` directly (toml + serde — no `cargo_metadata` subprocess). The daemon's offline-first / no-subprocess philosophy excludes invoking cargo at index time; subprocess execution of cargo-family tools is reserved for the Sprint 9 rust-analyzer LSIF path, which is gated behind ADR-006 (rust-analyzer trust model).
- The Python indexer walks up from the source file using the "highest contiguous `__init__.py`" rule. Edge cases and worked examples live in RFC-003 §3.
- VName construction helpers live in `travsr-core`:
  - `VName::for_rust_file(path, workspace_root) -> (VName, package: String)`
  - `VName::for_python_file(path, repo_root) -> (VName, package: String)`
  - `VName::for_typescript_file(path, repo_root) -> (VName, package: String)`
  Each returns the canonical VName (5 fields, ARCH-102-compliant corpus) plus the `package` string the caller will write to the `nodes.package` column.
