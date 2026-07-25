# ADR-018: Drop the Kùzu Storage Backend (SQLite Only)

**Date:** 2026-07-25
**Status:** Accepted
**Issue:** #457
**Phase:** N/A (architecture-contract change)
**Author:** Principal Architect
**Supersedes:** The store-roadmap decision recorded in `CLAUDE.md` and RFC-001 ("SQLite+WAL (MVP) → Kùzu (prod) → RocksDB (hyperscale)"). Kùzu is removed as a backend option.
**Related:** ADR-002 (edge-provenance policy, references the second-backend case), ADR-004 (error taxonomy / release size budget), ADR-005 (per-language corpus naming, referenced SQLite ↔ Kùzu parity)

---

## Context

Kùzu was originally planned as the production storage backend once SQLite reached its node ceiling. It shipped as an optional, feature-gated backend (`--features kuzu`) that was never built by default. Around it accumulated:

- a 673-line `KuzuStore` module (`crates/travsr-store/src/kuzu_store.rs`),
- a one-way SQLite → Kùzu migration path with a SHA-256 integrity manifest (`migration_manifest.rs`) and a `travsr migrate --to kuzu` CLI command,
- a SQLite/Kùzu parity test harness (`tests/parity.rs`),
- a nightly CI workflow plus feature-gated jobs in `ci.yml`, `phase2-exit.yml`, and `release.yml`,
- a C++ build chain (Kùzu compiles its C++ library from source; a `cxx-build` version pin existed only to keep that bridge linking),
- and decision records in `CLAUDE.md`, RFCs, and ADRs.

None of this shipped in the default binary. In practice SQLite+WAL has been sufficient for every target workload, and RocksDB remains the intended path if and when hyperscale is ever needed. Keeping the Kùzu scaffolding was pure maintenance drag: dependency-bump noise, a heavyweight build variant to keep green, and decision records that no longer reflected reality.

## Decision

**Remove Kùzu entirely. SQLite+WAL is the storage backend for both MVP and production. RocksDB stays a possible future hyperscale backend, governed by a separate decision if it is ever pursued.**

Concretely, this issue removes: the `KuzuStore` module, the migration + manifest code and the `travsr migrate` command, the parity test, the `kuzu` cargo features and the optional `kuzu` dependency, the `cxx-build` pin that existed only for the bridge, the nightly workflow, and all `--features kuzu` gates in CI. The backend-agnostic `MigrationRunner` framework (`migration.rs`) is retained — SQLite schema migrations still use it.

## Consequences

- **Lighter build and CI.** No C++ toolchain requirement, no nightly Kùzu job, no proof-build in the release pipeline, fewer dependencies to audit and bump.
- **No production graph engine beyond SQLite for now.** If SQLite's ceiling is reached, RocksDB (or another engine) must be introduced under a new ADR. There is no longer a pre-built migration path; one would be designed fresh against whatever backend is chosen.
- **Historical trail preserved.** Older RFCs and ADRs that referenced Kùzu are annotated with a "superseded: Kùzu dropped, see ADR-018" note rather than rewritten, so the original decision history stays intact.
- **Provenance/parity notes obsolete.** The SQLite ↔ Kùzu parity requirement (ADR-005) and the second-backend provenance case (ADR-002) no longer apply while SQLite is the only backend.
