-- Migration v7: Graph RBAC columns (RFC-006, S14).
--
-- nodes.access_corpus: NULL = public (any session can read).
-- Non-NULL = only sessions whose allowed_corpora contains this value may traverse this node.
-- ALTER TABLE does not support IF NOT EXISTS in SQLite — caller guards with column_exists().

ALTER TABLE nodes ADD COLUMN access_corpus TEXT;

-- sessions: persistent mirror of the in-memory DashMap<SessionId, Session>.
-- The in-memory store is authoritative; this table is write-only in the hot path.
CREATE TABLE IF NOT EXISTS sessions (
    id      TEXT PRIMARY KEY,
    corpus  TEXT NOT NULL,
    created INTEGER NOT NULL DEFAULT (unixepoch()),
    expires INTEGER NOT NULL
);
