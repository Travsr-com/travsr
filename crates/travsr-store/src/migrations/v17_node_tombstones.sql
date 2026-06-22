-- Migration v17: CDC tombstones for embed.db freshness (RFC-019).
--
-- node_embeddings moves out of graph.db into a sibling embed.db file written
-- exclusively by the embed sidecar. graph.db retains only the tombstone log so
-- the sidecar knows which node_ids were deleted between reindex passes.
--
-- Tombstone protocol (at-least-once delivery):
--   1. Trigger fires on every DELETE FROM nodes → appends to node_tombstones.
--   2. Sidecar reads node_tombstones at reindex start, deletes matching rows
--      from embed.db.node_embeddings, then acks by clearing node_tombstones.
--   3. If the sidecar crashes between delete and ack the tombstones replay on
--      the next pass — deletes from embed.db are idempotent (row absent = no-op).
--
-- Why no FK on node_tombstones.node_id:
--   The trigger captures the id BEFORE the node row is gone. Adding a FK would
--   cause the trigger itself to violate the constraint on the same DELETE.

DROP INDEX IF EXISTS idx_node_embeddings_model;
DROP TABLE IF EXISTS node_embeddings;

CREATE TABLE IF NOT EXISTS node_tombstones (
    node_id    INTEGER NOT NULL,
    deleted_at INTEGER NOT NULL DEFAULT (unixepoch())
);

-- Fires after every node deletion (delete_nodes_for_path, init purge, G3 cleanup).
CREATE TRIGGER IF NOT EXISTS capture_node_delete
AFTER DELETE ON nodes
BEGIN
    INSERT INTO node_tombstones(node_id) VALUES (OLD.id);
END;
