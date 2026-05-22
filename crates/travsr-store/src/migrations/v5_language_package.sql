-- Migration v5: add package column to nodes, language column to edges, and indexes.
--
-- nodes.language already exists since v1. This migration adds:
--   1. nodes.package  — sub-unit identity (Cargo crate / npm pkg / Python pkg)
--                       per ADR-005 Rule 2. NOT part of the BLAKE3 hash input.
--   2. edges.language — language tag on edges for Phase 4 cross-language queries
--                       per ADR-005 Rule 4.
--   3. Indexes on nodes(language) and nodes(package) for MCP language/package
--      scoped queries (get_repo_map, search_symbol, get_blast_radius).
--
-- Backfill: existing rows default to '' for package and 'typescript' for language.
-- These are safe sentinels — pre-Phase-3 graphs contain only TypeScript nodes.

-- NOTE: ALTER TABLE … ADD COLUMN is guarded in Rust (V5LanguagePackage::up)
-- via column_exists() checks because SQLite does not support IF NOT EXISTS here.

CREATE INDEX IF NOT EXISTS idx_nodes_language ON nodes(language);
CREATE INDEX IF NOT EXISTS idx_nodes_package  ON nodes(package);
