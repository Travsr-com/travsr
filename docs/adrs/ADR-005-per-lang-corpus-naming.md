# ADR-005: Per-Language Corpus Naming Convention

**Date:** 2026-05-21
**Status:** Accepted
**Issue:** #115 (tracked as ADR-004 in issue tracker; file is ADR-005 due to
existing `ADR-004-error-taxonomy.md`)
**Author:** Principal Architect
**Supersedes:** N/A
**Related:** ARCH-102 (TypeScript corpus naming), RFC-002 (VName signature versioning),
RFC-003 (multi-language indexer architecture)

---

## Context

ARCH-102 locked the corpus naming convention for TypeScript:

> corpus = the basename of the git remote URL
> (e.g. `travsr` from `git@github.com:raj-rkv/travsr.git`)

Phase 3 adds Rust (Sprint 8) and Python (Sprint 10). A repo like Travsr
contains both Rust crates and TypeScript packages. Without a locked convention,
two parsers could emit VNames that collide or that cannot be linked in Phase 4
cross-language queries.

This ADR answers:

1. What is the corpus for Rust crates in a Cargo workspace?
2. What is the corpus for Python packages?
3. How does language identity flow into VNames without polluting the corpus field?
4. How does `signature_version` (RFC-002) extend to non-TypeScript languages?

---

## Decision

### Rule 1 — Corpus is always the git remote basename

All languages in the same repository share **one corpus**: the basename of the
`origin` remote URL (existing ARCH-102 rule, extended to all languages).

```
github.com/raj-rkv/travsr  →  corpus = "travsr"
gitlab.com/acme/backend    →  corpus = "backend"
(no remote / local only)   →  corpus = basename of the working-directory path
```

**Do not encode language in the corpus.** Encoding language (e.g.
`travsr-rust`, `travsr-typescript`) would prevent Phase 4 cross-language edges:
both endpoints of a `calls_foreign` edge must share a corpus for the VName to
remain globally addressable under the Kythe model.

Language identity is carried by:
- The `language` column on the `nodes` table (human-readable string).
- The `Language::signature_prefix()` embedded in the VName `signature` field
  (RFC-002 format).

**Scope note:** The basename rule applies to local single-developer installations.
The cloud tier must use the full remote URL (e.g. `github.com/acme/backend`) as
the tenant-partitioned corpus to prevent collisions between unrelated repos that
share the same basename. Local daemon code passes the basename only; the cloud
ingestion layer is responsible for prefixing the full URL.

### Rule 2 — VName `package` field carries the sub-unit identity

| Language | `package` value | Example |
|---|---|---|
| TypeScript | npm package name from `package.json`, or repo-relative directory | `@travsr.com/travsr` |
| Rust | Cargo package name from `Cargo.toml` (`[package] name`) | `travsr-core` |
| Python | Top-level package name (directory containing `__init__.py`) | `travsr` |

For a Cargo **workspace**, the corpus is still the workspace root remote
basename. Each crate is distinguished by its `package` field. This means
`fn open` in `travsr-store` and `fn open` in `travsr-indexer` have different
VNames even though they share a corpus:

```
VName {
  corpus:    "travsr",
  root:      "",
  path:      "crates/travsr-store/src/lib.rs",
  language:  "rust",
  package:   "travsr-store",
  signature: "rust:v1:<blake3-of-fn-open-in-travsr-store>"
}

VName {
  corpus:    "travsr",
  root:      "",
  path:      "crates/travsr-indexer/src/lib.rs",
  language:  "rust",
  package:   "travsr-indexer",
  signature: "rust:v1:<blake3-of-fn-open-in-travsr-indexer>"
}
```

The signatures differ because the blake3 input includes the `path` component.

**Virtual workspace root edge case:** A workspace-level `Cargo.toml` may have
no `[package]` section (virtual manifest). If the walk-up finds a `Cargo.toml`
without `[package] name`, continue walking up or fall back to the
working-directory basename. In practice this affects only files placed directly
at the workspace root (e.g. a top-level `build.rs`), which is uncommon.

### Rule 3 — Signature version prefix per language (RFC-002 extension)

RFC-002 introduced `ts:v1:` as the prefix for TypeScript VName signatures.
This ADR extends the convention to all languages:

| Language | Prefix | Example signature |
|---|---|---|
| TypeScript | `ts:v1:` | `ts:v1:a3f2…` |
| Rust | `rust:v1:` | `rust:v1:9c1e…` |
| Python | `py:v1:` | `py:v1:ff03…` |

The prefix is prepended to the blake3 hash of the canonical symbol descriptor
before hashing the VName. This guarantees that a TypeScript `function foo` and
a Rust `fn foo` in the same file can never produce the same signature, even if
the descriptor strings are identical.

**Existing TypeScript VNames are not modified.** They already carry the `ts:v1:`
prefix. No migration is required for the TypeScript corpus.

### Rule 4 — `language` field on `Node` (schema migration v4)

The `language` column added by `CORE-201` stores the `Language::as_str()`
value (`"typescript"`, `"rust"`, `"python"`). This is the human-readable
complement to the signature prefix and is used by MCP tools to filter by
language without parsing signatures.

Backfill: existing rows are updated to `"typescript"` during migration v4.

---

## Consequences

### Positive

- **No VName collisions** across languages in the same repo.
- **Phase 4 cross-language edges work** without schema changes: both endpoints
  share the same corpus and are distinguished by the `language` field and
  signature prefix.
- **Existing TypeScript indexing is unchanged** — no VName churn, no
  re-indexing required for current users.
- **Cargo workspaces are handled correctly** — multi-crate repos produce
  properly scoped VNames via the `package` field.

### Negative

- Clients filtering by language must inspect the `language` column or the
  signature prefix, not the corpus. This is a minor query complexity increase
  vs. language-per-corpus, but it is the correct trade-off.
- The `package` field is optional in Kythe VNames; Travsr uses it as a
  required field for Rust and Python nodes. Tooling that ignores `package`
  (e.g. vanilla Kythe tools) will produce false VName matches. Accepted — we
  are not targeting vanilla Kythe compatibility.

---

## Examples

### Travsr repo — mixed Rust + TypeScript

```
# Rust crate node
VName {
  corpus:    "travsr",
  root:      "",
  path:      "crates/travsr-core/src/lib.rs",
  language:  "rust",
  package:   "travsr-core",
  signature: "rust:v1:3a9c..."
}

# TypeScript package node
VName {
  corpus:    "travsr",
  root:      "",
  path:      "packages/travsr-lsif-ts/src/index.ts",
  language:  "typescript",
  package:   "travsr-lsif-ts",
  signature: "ts:v1:7f2b..."
}
```

### Python repo

```
VName {
  corpus:    "myapp",
  root:      "",
  path:      "myapp/models/user.py",
  language:  "python",
  package:   "myapp",
  signature: "py:v1:c801..."
}
```

---

## Implementation Notes

- `Language::signature_prefix()` is implemented in `travsr-core` (CORE-201).
- The Rust indexer reads `[package] name` from `Cargo.toml` via the
  `cargo_metadata` crate or by parsing the nearest `Cargo.toml` up the
  directory tree.
- The Python indexer walks up from the source file to find the **highest**
  contiguous directory that still contains `__init__.py` and uses that
  directory's name as the package. This correctly handles `src/` layouts
  (e.g. `src/myapp/__init__.py` → `package = "myapp"`, not `"src"`).
  For namespace packages (no `__init__.py` anywhere in the path), fall back
  to the repo-root-relative first path component.
- Both rules are encapsulated in `VName::for_rust_file(path, workspace_root)`
  and `VName::for_python_file(path, repo_root)` helpers in `travsr-core`.
