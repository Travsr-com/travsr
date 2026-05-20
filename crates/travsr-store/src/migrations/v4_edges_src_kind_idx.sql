-- Migration v4: composite index on (src, kind, dst) for iter_edges_from_kind.
--
-- Without this index, WHERE src=?1 AND kind=?2 must scan every row in the
-- src-partition of the primary key (src, dst, kind) and filter kind in memory.
-- With this covering index, the query reads only the matching (src, kind) rows
-- and retrieves dst without touching the main table (index-only scan).
--
-- Required for Personalized PageRank (Issue #29) which calls iter_edges_from_kind
-- in a tight inner loop over every node and must meet the p95 < 50ms budget.
CREATE INDEX IF NOT EXISTS idx_edges_src_kind ON edges(src, kind, dst);
