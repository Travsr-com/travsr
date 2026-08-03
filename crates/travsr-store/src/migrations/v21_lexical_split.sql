-- Migration v21: RFC-023 lexical retrieval architecture (#478).
--
-- Splits the lexical front into a precision leg (word BM25, this table) and
-- the existing recall leg (nodes_fts, trigram, unchanged — no rebuild). See
-- docs/rfcs/RFC-023-lexical-retrieval-architecture.md §5.1.
--
-- `nodes.is_noise` is added separately in Rust (ALTER TABLE has no
-- IF NOT EXISTS in SQLite; the caller guards with column_exists()).

CREATE VIRTUAL TABLE IF NOT EXISTS nodes_fts_words USING fts5(
    sig, path, content='', tokenize='unicode61'
);

-- Retraction memory for the contentless nodes_fts_words table (parallel to
-- nodes_fts_map): a contentless FTS5 table cannot recover its own inserted
-- text on delete, so the exact word-segmented strings must be kept here.
CREATE TABLE IF NOT EXISTS nodes_fts_words_map (
    node_id    INTEGER PRIMARY KEY,
    sig_words  TEXT NOT NULL,
    path_words TEXT NOT NULL
);

-- fts5vocab 'row' mode: (term, doc, cnt) where `doc` is FTS5-maintained
-- document frequency — authoritative, zero drift (RFC-023 §5.5).
CREATE VIRTUAL TABLE IF NOT EXISTS nodes_words_vocab USING fts5vocab(nodes_fts_words, 'row');
