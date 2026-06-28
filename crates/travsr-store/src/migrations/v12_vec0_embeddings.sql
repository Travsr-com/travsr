-- Migration v12: plain blob embedding table (RFC-018 EmbedPlugin architecture).
--
-- Replaces the original vec0 VIRTUAL TABLE stub (RFC-012 A2 F2, now superseded).
-- RFC-018 moves embedding generation and ANN search into a downloadable sidecar
-- binary (`travsr-embed-<backend>`).  The main process only does plain SQL: it
-- stores raw embedding BLOBs here and lets the sidecar open its own DB
-- connection with the sqlite-vec extension for KNN queries.
--
-- Schema rationale:
--   node_id   — FK to nodes.id; cascade-delete keeps embeddings in sync
--   model_id  — opaque backend tag ("nomic-v1.5-int8", "bge-small-en-v1.5", …)
--   embedding — raw bytes; layout is backend-defined (MRL-256 float32 for nomic)
--
-- The (node_id, model_id) composite PK makes INSERT OR REPLACE idempotent
-- on re-indexing: same node + same model always overwrites, never duplicates.
-- WITHOUT ROWID clusters rows on the PK, which is the dominant access pattern
-- (point lookup by node_id during KNN re-rank).

CREATE TABLE IF NOT EXISTS node_embeddings (
    node_id   INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    model_id  TEXT    NOT NULL,
    embedding BLOB    NOT NULL,
    PRIMARY KEY (node_id, model_id)
) WITHOUT ROWID;

-- Secondary index to support "count rows for model X" and model-scoped scans
-- without a full-table scan (used by `travsr embed status`).
CREATE INDEX IF NOT EXISTS idx_node_embeddings_model
    ON node_embeddings(model_id);
