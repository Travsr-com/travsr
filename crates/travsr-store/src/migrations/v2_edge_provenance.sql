-- Travsr SQLite schema v2.
-- Adds provenance column to the edges table.
-- Policy: 'lsif' beats 'tree-sitter'. Both pipelines may emit the same
-- (src, dst, kind) triple; LSIF (semantic) always wins on conflict.
-- Valid values: 'tree-sitter' | 'lsif' | 'merged' (reserved for future use).
ALTER TABLE edges ADD COLUMN provenance TEXT NOT NULL DEFAULT 'tree-sitter';
