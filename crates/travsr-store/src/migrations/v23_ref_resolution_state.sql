-- RFC-027 section 9.2: honest abstention for the live semantic lane, and the
-- read-path indexes the lane's `provenance` plumbing needs.
--
-- A reference Tree-sitter detected but nothing could resolve is not an edge and
-- must never be rendered as one. It is also not nothing: "there is a call here,
-- and its target is not yet known" is a true and useful statement, and the
-- whole precision-first argument rests on being able to say it out loud instead
-- of guessing. This table is where that statement lives.
--
-- Named `ref_resolution_state` rather than `resolution_state` to avoid
-- colliding with the daemon's unrelated `record_dart_resolution_state`
-- bookkeeping, which tracks Dart Phase B availability.
--
-- Modelled on `edge_sites`: a composite PK so re-resolving a file is an actual
-- dedup rather than unbounded growth, and WITHOUT ROWID so rows cluster on that
-- PK and its prefix (src) serves as the lookup index without a second one.
--
-- `state` is 'pending' or 'resolved'. Rows are owned by their `src` node's file
-- the same way `edge_sites` rows are, so the live engine deletes a file's rows
-- before re-resolving it.
--
-- That ownership is by convention, not by constraint: there is no foreign key
-- and no cascade, so a vanished symbol's rows do NOT go with its node. A NodeId
-- hashes the VName (signature included), so renaming a symbol retires its id and
-- the per-file delete, which resolves `src` through `nodes`, can no longer reach
-- the row. `purge_orphan_ref_resolution_states` sweeps those at ratification, and
-- both readers (`pending_refs_in_file`, `pending_ref_count`) join `nodes` so they
-- agree in between.
--
-- `resolved_dst` records what the live lane actually claimed, so its precision
-- can be measured against Phase B's answer at the next commit (section 12). The
-- edge itself cannot answer this: by ratification a live edge Phase B re-derived
-- has been relabelled in place, and one it did not re-derive is about to be
-- swept, so the edges table no longer says "the live lane resolved this
-- reference to this node". NULL for a pending row, which is the whole point of
-- an abstention.
CREATE TABLE IF NOT EXISTS ref_resolution_state (
    src          INTEGER NOT NULL,
    ref_line     INTEGER NOT NULL,
    ref_col      INTEGER NOT NULL,
    name         TEXT NOT NULL,
    state        TEXT NOT NULL,
    resolved_dst INTEGER,
    PRIMARY KEY (src, ref_line, ref_col, name)
) WITHOUT ROWID;

-- The MCP surface asks "what is pending in this file", which is a scan by state
-- across many src nodes rather than a probe of one. Without this it is a full
-- table scan on every query that mentions a dirty file.
CREATE INDEX IF NOT EXISTS idx_ref_resolution_state
    ON ref_resolution_state(state);

-- ── Read-path indexes for `Edge.provenance` (DEBT-75) ────────────────────────
--
-- Threading provenance to the MCP surface added it to the `iter_edges_from_kind`
-- and `iter_edges_to` SELECT lists, which took both off the covering indexes v4
-- and v14 added, so SQLite went back to fetching the main-table row for every
-- edge. Verified on a real graph (33.5k edges):
--
--   SELECT dst FROM edges WHERE src=? AND kind=?
--       -> SEARCH edges USING COVERING INDEX idx_edges_src_kind_cov
--   SELECT dst, provenance FROM edges WHERE src=? AND kind=?
--       -> SEARCH edges USING INDEX idx_edges_src_kind_cov
--
-- `iter_edges_from_kind` is the PPR traversal step, which filters by kind at
-- every hop and owes a p95 under 50ms; `iter_edges_to` backs get_callers, blast
-- radius and the live lane's `reverse_closure`. Eliminating that fetch is what
-- v4 and v14 exist for.
--
-- Both tails carry the column rather than dropping provenance from the traversal
-- query, even though no reader consults it there today. `query.rs::prov_of` maps
-- a missing provenance to `"tree-sitter"`, so an unlabelled edge would read as
-- *ratified truth* rather than as unknown -- a silent false claim in a lane whose
-- whole purpose is never presenting a `live` edge as ratified.
--
-- DROP then CREATE, not CREATE IF NOT EXISTS: the names already exist from v4
-- and v14, so a guarded create would silently keep the narrow definitions.
DROP INDEX IF EXISTS idx_edges_src_kind_cov;
CREATE INDEX idx_edges_src_kind_cov ON edges(src, kind, dst, provenance);

DROP INDEX IF EXISTS idx_edges_dst_kind_cov;
CREATE INDEX idx_edges_dst_kind_cov ON edges(dst, kind, src, provenance);

-- The live-overlay freshness note (section 10) counts `live` edges on every
-- prose MCP query, and `WHERE provenance = 'live'` had no index at all:
--
--   SELECT count(*) FROM edges WHERE provenance='live'   -> SCAN edges
--
-- A full scan of the edge table per query, for a note that is usually about
-- zero rows. A partial index costs nothing on a clean tree (the overlay is
-- bounded by uncommitted edits, and rows leave it at every commit) and turns the
-- count into a seek over just those rows.
CREATE INDEX IF NOT EXISTS idx_edges_live_provenance
    ON edges(provenance) WHERE provenance = 'live';
