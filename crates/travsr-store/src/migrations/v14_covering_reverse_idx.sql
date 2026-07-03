-- Migration v14: covering reverse index on (dst, kind, src) for iter_edges_to.
--
-- v1 created idx_edges_dst_kind ON edges(dst, kind) — a 2-column index that
-- makes WHERE dst=? AND kind=? an efficient seek but requires a separate
-- main-table random I/O to fetch `src` per matching row.  At 75M nodes with
-- average fan-in 20–50, every get_callers / get_blast_radius call pays 20–50
-- random reads that a covering index eliminates entirely.
--
-- idx_edges_dst_kind_cov adds `src` to the index leaf pages, turning the query
-- into an index-only scan.  Symmetric counterpart to the v4 forward index
-- idx_edges_src_kind_cov ON edges(src, kind, dst).
--
-- CREATE before DROP: if the process crashes mid-migration, the reverse
-- direction stays indexed (new index present, old one may or may not exist).
-- IF NOT EXISTS / IF EXISTS guards make the migration fully idempotent.
CREATE INDEX IF NOT EXISTS idx_edges_dst_kind_cov ON edges(dst, kind, src);
DROP   INDEX IF EXISTS idx_edges_dst_kind;
