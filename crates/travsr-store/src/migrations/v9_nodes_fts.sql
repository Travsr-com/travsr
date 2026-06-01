-- Migration v9: FTS5 trigram index for fuzzy seed selection (RFC-012 L1).
--
-- nodes_fts  — contentless FTS5 virtual table.  Uses the built-in `trigram`
--              tokenizer which handles sub-token matches and single-char typos
--              without a word-boundary requirement.  `content=''` keeps the
--              FTS index from duplicating node data; we join nodes_fts_map →
--              nodes to resolve matches back to full Node rows.
--
-- nodes_fts_map — maps node_id (i64 rowid) ↔ the exact tokenized string that
--                 was inserted.  REQUIRED for contentless-FTS5 retraction: the
--                 `INSERT INTO nodes_fts(nodes_fts, rowid, tokens) VALUES('delete', ?, ?)`
--                 command must re-supply the *original* tokenized string; a
--                 contentless table cannot recover it from the index.
--                 Without this map, stale FTS rows from renamed/deleted nodes
--                 would be unretractable, corrupting the index over time.
--
-- Design note (deviation from RFC-012 schema):
--   The RFC proposed `node_id BLOB` (32 BLAKE3 bytes) with AUTOINCREMENT.
--   In the actual implementation `NodeId` is a `u64` newtype stored as i64,
--   so node_id (INTEGER PRIMARY KEY) serves directly as the FTS rowid.
--   The AUTOINCREMENT / BLOB map is dropped.

CREATE VIRTUAL TABLE IF NOT EXISTS nodes_fts USING fts5(
    tokens,
    content='',
    tokenize='trigram'
);

CREATE TABLE IF NOT EXISTS nodes_fts_map (
    node_id  INTEGER PRIMARY KEY,
    tokens   TEXT    NOT NULL
);
