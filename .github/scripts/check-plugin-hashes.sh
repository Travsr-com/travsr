#!/usr/bin/env bash
# ADR-017 Rule 5 — CI enforcement gate for plugin_version bumps.
# Computes a SHA-256 of each plugin crate's src/ tree and compares against
# the recorded hash in plugin-hashes.lock. A source change without a
# plugin_version bump fails CI.
set -euo pipefail

LOCK_FILE="plugin-hashes.lock"
CRATES_DIR="crates"
FAILED=0

while IFS='=' read -r crate recorded_hash; do
    [[ "$crate" =~ ^#.*$ ]] && continue  # skip comments
    [[ -z "$crate" ]] && continue

    crate=$(echo "$crate" | tr -d ' ')
    recorded_hash=$(echo "$recorded_hash" | tr -d ' ')

    src_dir="$CRATES_DIR/$crate/src"
    if [[ ! -d "$src_dir" ]]; then
        echo "SKIP: $crate/src not found"
        continue
    fi

    # Hash file contents only (not paths) so the hash is identical across macOS and Linux.
    actual_hash=$(find "$src_dir" -type f -name "*.rs" | sort | xargs cat 2>/dev/null | sha256sum | awk '{print $1}')

    if [[ "$actual_hash" != "$recorded_hash" ]]; then
        echo "ERROR: $crate source changed but plugin_version not bumped"
        echo "  recorded: $recorded_hash"
        echo "  actual:   $actual_hash"
        echo "  Fix: bump plugin_version in $CRATES_DIR/$crate/Cargo.toml"
        echo "       then run: bash .github/scripts/update-plugin-hashes.sh"
        FAILED=1
    else
        echo "OK: $crate"
    fi
done < "$LOCK_FILE"

exit $FAILED
