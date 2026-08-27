-- RFC-027 section 9.2: honest abstention for the live semantic lane.
--
-- A reference Tree-sitter detected but nothing could resolve is not an edge and
-- must never be rendered as one. It is also not nothing: "there is a call here,
-- and its target is not yet known" is a true and useful statement, and the
-- whole precision-first argument rests on being able to say it out loud instead
-- of guessing. This table is where that statement lives.
--
-- Named `ref_resolution_state` rather than `resolution_state` to avoid
-- colliding with the daemon's unrelated `record_dart_resolution_state`
-- bookkeeping, which tracks Dart Phase B availability.
--
-- Modelled on `edge_sites`: a composite PK so re-resolving a file is an actual
-- dedup rather than unbounded growth, and WITHOUT ROWID so rows cluster on that
-- PK and its prefix (src) serves as the lookup index without a second one.
--
-- `state` is 'pending' or 'resolved'. Rows are owned by their `src` node's file
-- the same way `edge_sites` rows are, so the live engine deletes a file's rows
-- before re-resolving it and a vanished symbol's rows go with its node.
CREATE TABLE IF NOT EXISTS ref_resolution_state (
    src      INTEGER NOT NULL,
    ref_line INTEGER NOT NULL,
    ref_col  INTEGER NOT NULL,
    name     TEXT NOT NULL,
    state    TEXT NOT NULL,
    PRIMARY KEY (src, ref_line, ref_col, name)
) WITHOUT ROWID;

-- The MCP surface asks "what is pending in this file", which is a scan by state
-- across many src nodes rather than a probe of one. Without this it is a full
-- table scan on every query that mentions a dirty file.
CREATE INDEX IF NOT EXISTS idx_ref_resolution_state
    ON ref_resolution_state(state);
