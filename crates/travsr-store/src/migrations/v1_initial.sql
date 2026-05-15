-- Travsr SQLite schema v1.
-- Tables: nodes, edges, files. The meta table is created outside this
-- migration so the schema_version row can be read before the first
-- migration runs (see SqliteStore::migrate).

CREATE TABLE IF NOT EXISTS nodes (
    id        INTEGER PRIMARY KEY,
    corpus    TEXT NOT NULL,
    root      TEXT NOT NULL,
    path      TEXT NOT NULL,
    language  TEXT NOT NULL,
    signature TEXT NOT NULL,
    kind      TEXT NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_nodes_vname
    ON nodes(corpus, root, path, language, signature);

CREATE TABLE IF NOT EXISTS edges (
    src  INTEGER NOT NULL,
    dst  INTEGER NOT NULL,
    kind TEXT NOT NULL,
    PRIMARY KEY (src, dst, kind)
);

CREATE INDEX IF NOT EXISTS idx_edges_src_kind ON edges(src, kind);
CREATE INDEX IF NOT EXISTS idx_edges_dst_kind ON edges(dst, kind);

CREATE TABLE IF NOT EXISTS files (
    path            TEXT PRIMARY KEY,
    sha256          TEXT NOT NULL,
    last_indexed_at INTEGER NOT NULL
);
