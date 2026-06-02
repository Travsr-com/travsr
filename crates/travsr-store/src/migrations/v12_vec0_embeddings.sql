-- Migration v12: vec0 embedding table for RFC-012 A2 F2 (opt-in, feature-gated).
--
-- Only runs when the `embeddings` feature flag is enabled AND the sqlite-vec
-- extension has been loaded into the connection (done by travsr embed init).
-- Without the extension this migration is a no-op (the virtual table module
-- is not registered so CREATE VIRTUAL TABLE would fail — guard is in V12Vec0Embeddings::up).
--
-- Stores MRL-256 + RaBitQ 1-bit compressed vectors per node.
-- The RaBitQ rotation seed is pinned in meta (key: rabitq_rotation_seed) at
-- embed-init time.  Two stores with different seeds produce incompatible
-- Hamming distances.

CREATE VIRTUAL TABLE IF NOT EXISTS node_embeddings USING vec0(
    node_id INTEGER PRIMARY KEY,
    embedding FLOAT[256]
);
