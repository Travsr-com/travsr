# ADR-002: Edge Provenance and LSIF/Tree-sitter Precedence Policy

**Date:** 2026-05-18
**Status:** Accepted

## Context

Both the Tree-sitter structural pipeline and the LSIF semantic pipeline emit edges
into the same `edges` table. Without a precedence policy, duplicate `(src, dst, kind)`
rows were silently dropped by `INSERT OR IGNORE`, making the graph nondeterministic
across re-indexes and blocking DEBT-014 (#23).

## Decision

1. Add a `provenance TEXT NOT NULL DEFAULT 'tree-sitter'` column to the `edges`
   table via schema migration v2.
2. Valid values: `'tree-sitter'` | `'lsif'` | `'merged'` (reserved for future use).
3. **LSIF wins:** when both pipelines emit the same `(src, dst, kind)` edge, the
   stored provenance is `'lsif'`. Tree-sitter cannot demote an existing `'lsif'` row.
4. LSIF edges are written via `SqliteStore::put_edge_lsif`. All other callers use
   the `Store::put_edge` trait method which defaults to `'tree-sitter'` provenance.
5. The upsert logic lives entirely in SQL — no Rust-level read-modify-write cycle,
   no race window.

## Consequences

**Positive:**
- Graph is now deterministic: re-indexing the same source always produces the same
  provenance regardless of pipeline execution order.
- DEBT-014 (#23) is unblocked — consumers can filter by `provenance = 'lsif'` to
  prefer semantic edges over structural ones.
- MCP `get_callers` already tags edges by kind; provenance is additive metadata
  that can be surfaced to LLM clients in a future sprint.
- `all_edges()` returns a 4-tuple including provenance — callers updated accordingly.

**Negative:**
- `all_edges()` return type changed from `Vec<(NodeId, NodeId, String)>` to
  `Vec<(NodeId, NodeId, String, String)>` — all existing callers needed updating.
- The `Store` trait does not expose `put_edge_lsif`; it is a concrete method on
  `SqliteStore` only. If a second backend is ever added, it must implement the
  same precedence logic independently. (Kùzu, once planned as that backend, was
  dropped in #457 — see ADR-018.)
