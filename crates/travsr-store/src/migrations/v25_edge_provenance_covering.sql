-- Migration v25: carry `provenance` in the two edge covering indexes.
--
-- RFC-027 threads `Edge.provenance` to the MCP surface (DEBT-75), which added it
-- to the SELECT lists of `iter_edges_from_kind` and `iter_edges_to`. Neither
-- index carried the column, so both queries fell off their covering index and
-- SQLite went back to fetching the main-table row for every edge -- exactly the
-- fetch v4 and v14 exist to eliminate:
--
--   SELECT dst FROM edges WHERE src=? AND kind=?
--       -> SEARCH edges USING COVERING INDEX idx_edges_src_kind_cov
--   SELECT dst, provenance FROM edges WHERE src=? AND kind=?
--       -> SEARCH edges USING INDEX idx_edges_src_kind_cov
--
-- `iter_edges_from_kind` is the PPR traversal step, which filters by kind at
-- every hop and owes a p95 under 50ms; `iter_edges_to` backs get_callers, blast
-- radius and `reverse_closure`. Appending the column to both index tails
-- restores the index-only scan rather than paying a row fetch per edge on every
-- traversal.
--
-- DROP then CREATE, not CREATE IF NOT EXISTS: the names already exist from v4
-- and v14, so a guarded create would silently keep the narrow definitions.
DROP INDEX IF EXISTS idx_edges_src_kind_cov;
CREATE INDEX idx_edges_src_kind_cov ON edges(src, kind, dst, provenance);

DROP INDEX IF EXISTS idx_edges_dst_kind_cov;
CREATE INDEX idx_edges_dst_kind_cov ON edges(dst, kind, src, provenance);

-- The live-overlay freshness note (RFC-027 section 10) counts `live` edges on
-- every prose MCP query, and `WHERE provenance = 'live'` had no index at all:
--
--   SELECT count(*) FROM edges WHERE provenance='live'   -> SCAN edges
--
-- A full scan of the edge table per query, for a note that is usually about
-- zero rows. A partial index costs nothing on a clean tree (the overlay is
-- bounded by uncommitted edits, and rows leave it at every commit) and turns the
-- count into a seek over just those rows.
CREATE INDEX IF NOT EXISTS idx_edges_live_provenance
    ON edges(provenance) WHERE provenance = 'live';
