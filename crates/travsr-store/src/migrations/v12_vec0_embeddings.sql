-- Migration v12: vec0 embedding table for RFC-012 A2 F2 (opt-in, feature-gated).
--
-- STUB — DEBT(travsr-#259). Only registered when the `embeddings` feature is
-- enabled, which is not yet wired (ort + sqlite-vec deps unpinned, no extension
-- loader). This CREATE VIRTUAL TABLE requires the `vec0` module to be registered
-- on the connection; without it the statement fails. There is NO runtime guard
-- in V12Vec0Embeddings::up today — enabling `embeddings` before F2 lands will
-- error here. Kept in-tree so the migration ordering after v11 is reserved.
--
-- Stores MRL-256 + RaBitQ 1-bit compressed vectors per node.
-- The RaBitQ rotation seed is pinned in meta (key: rabitq_rotation_seed) at
-- embed-init time.  Two stores with different seeds produce incompatible
-- Hamming distances.

CREATE VIRTUAL TABLE IF NOT EXISTS node_embeddings USING vec0(
    node_id INTEGER PRIMARY KEY,
    embedding FLOAT[256]
);
