-- Migration v11: per-repo dynamic synonym table for RFC-012 A2 F1.
--
-- fts_synonyms replaces the compile-time SYNONYMS static in seed_lexicon.rs
-- for the Step 2 hot path.  Seeded from those defaults at `travsr init`;
-- modified at runtime via `travsr synonym add/remove/list/reset`.
--
-- Direction: term → alias (one-directional, same as static table).
-- "auth" → "session" does NOT imply "session" → "auth".
--
-- Hard cap: 200 rows enforced on insert by the CLI.
-- Hot-path reads use prepare_cached + a partial-scan on the PK prefix.

CREATE TABLE IF NOT EXISTS fts_synonyms (
    term     TEXT NOT NULL,
    alias    TEXT NOT NULL,
    PRIMARY KEY (term, alias)
);

CREATE INDEX IF NOT EXISTS fts_synonyms_term_idx ON fts_synonyms(term);
