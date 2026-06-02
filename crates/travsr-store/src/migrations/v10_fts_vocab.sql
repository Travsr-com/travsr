-- Migration v10: vocabulary index for RFC-012 A1 L2-A vocabulary-grounded expansion.
--
-- fts_vocab stores every distinct token that has ever been inserted into
-- nodes_fts, along with a refcount of how many FTS rows still contain it.
-- The refcount is incremented by put_node_fts and decremented by delete_node_fts
-- and both bulk-delete paths.
--
-- expand_query (Step 3 in search_nodes_fuzzy) uses this table to find
-- vocabulary-grounded candidates: tokens that exist in the actual graph and
-- have high Jaccard(trigrams) similarity to the query tokens.  Because the
-- vocabulary is grounded (only real graph tokens appear here), L2-A cannot
-- hallucinate tokens that do not exist in the FTS index — enforcing PA C4.
--
-- The refcount_idx is optional (expand_query filters refcount > 0) but keeps
-- the linear scan cheap by pruning deleted-but-not-yet-vacuumed rows.

CREATE TABLE IF NOT EXISTS fts_vocab (
    token    TEXT    PRIMARY KEY,
    refcount INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS fts_vocab_refcount_idx ON fts_vocab(refcount);
