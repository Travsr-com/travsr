-- V15: k-core shell numbers on nodes.
-- shell_number is recomputed by travsr-retrieval::compute_kcore after every index.
-- Default 0 means "not yet computed" — safe for reads before the first kcore pass.
ALTER TABLE nodes ADD COLUMN shell_number INTEGER NOT NULL DEFAULT 0;
