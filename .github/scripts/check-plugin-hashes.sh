#!/usr/bin/env bash
# ADR-017 Rule 5 — CI enforcement gate for plugin_version bumps.
# Computes a SHA-256 of each plugin crate's src/ tree and compares against
# the recorded hash in plugin-hashes.lock. A source change without a
# plugin_version bump fails CI.
set -euo pipefail

LOCK_FILE="plugin-hashes.lock"
CRATES_DIR="crates"
FAILED=0

# Portable SHA-256: prefer sha256sum (GNU coreutils / Linux), fall back to
# shasum -a 256 (macOS / BSD). Fail loudly if neither is available.
if command -v sha256sum &>/dev/null; then
    SHA256=sha256sum
elif command -v shasum &>/dev/null; then
    SHA256="shasum -a 256"
else
    echo "ERROR: neither sha256sum nor shasum found on PATH" >&2
    exit 1
fi

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
    # Use a while-read loop instead of xargs to avoid xargs reading from stdin when find
    # produces no output (which causes an indefinite hang on GNU xargs or wrong hash on BSD).
    actual_hash=$(find "$src_dir" -type f -name "*.rs" | sort | while IFS= read -r f; do cat -- "$f"; done | $SHA256 | awk '{print $1}')

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
