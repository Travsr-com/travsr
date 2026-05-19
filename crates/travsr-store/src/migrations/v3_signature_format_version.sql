-- Travsr SQLite schema v3.
-- Records the VName signature format version used when this graph was last
-- fully indexed (RFC-002). Existing databases receive version 0 (legacy:
-- no domain-separator byte in the BLAKE3 hash input). The daemon writes the
-- current SIGNATURE_FORMAT_VERSION constant after every successful full
-- re-index, so a stale value here reliably detects schema drift.
INSERT OR IGNORE INTO meta(key, value)
    VALUES('signature_format_version', '0');
