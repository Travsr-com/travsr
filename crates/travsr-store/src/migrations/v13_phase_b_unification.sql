-- Travsr SQLite schema v13 — Phase B graph unification (RFC-014, WS2).
--
-- 1. end_line: G2 span attribution — enclosing-function lookup for SCIP occurrences.
-- 2. symbol_aliases: G1 unification — SCIP symbol string → unified tree-sitter NodeId.
-- 3. edge_sites: O4 evidence — call-site line numbers per (src, dst, kind) triple.
--
-- All DDL uses IF NOT EXISTS / ADD COLUMN guards so re-running after an
-- atomicity-gap crash is safe (see migration.rs trait-level doc).
--
-- NOTE: The `end_line` column is added via ALTER TABLE in the Rust migration
-- struct (V13PhaseB::up) because SQLite has no ADD COLUMN IF NOT EXISTS.
-- This file handles the new-table DDL that IS idempotent with IF NOT EXISTS.

CREATE TABLE IF NOT EXISTS symbol_aliases (
    scip_symbol TEXT NOT NULL PRIMARY KEY,
    node_id     INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_symbol_aliases_node
    ON symbol_aliases(node_id);

-- The composite PK makes the INSERT OR IGNORE in write_scip_attributed_batch
-- an actual dedup: re-indexing re-derives identical (src, dst, kind, line)
-- rows, and without a uniqueness constraint the table would grow unboundedly.
-- WITHOUT ROWID (SQLite >= 3.8.2; bundled libsqlite3 is far newer) stores the
-- rows clustered on the PK, and the PK prefix (src, dst, kind) covers the
-- lookup index this table needs — no separate index required.
CREATE TABLE IF NOT EXISTS edge_sites (
    src  INTEGER NOT NULL,
    dst  INTEGER NOT NULL,
    kind TEXT NOT NULL,
    line INTEGER NOT NULL,
    PRIMARY KEY (src, dst, kind, line)
) WITHOUT ROWID;

-- G1/G2 lookups probe nodes by (corpus, path) per SCIP ref — on a monorepo
-- that is millions of probes, and without this index each one is a full
-- table scan over every node (hours on kubernetes; minutes with it).
CREATE INDEX IF NOT EXISTS idx_nodes_corpus_path
    ON nodes(corpus, path);
