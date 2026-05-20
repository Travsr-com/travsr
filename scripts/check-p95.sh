#!/usr/bin/env bash
# check-p95.sh — assert that the 1k-file fixture benchmarks stay under 50 ms.
#
# Usage: ./scripts/check-p95.sh
#
# Reads Criterion's JSON estimates from target/criterion/ after
# `cargo bench --bench retrieval -- 1k_fixture` has been run.
# Fails with exit code 1 if any measured mean exceeds MAX_NS.
#
# Criterion stores per-benchmark results in:
#   target/criterion/<group>/<bench>/new/estimates.json
# where estimates.json contains:
#   { "mean": { "point_estimate": <nanoseconds (f64)>, ... }, ... }

set -euo pipefail

# 50 milliseconds in nanoseconds
MAX_NS=50000000

# Criterion maps group names with '/' to '_' in the output directory.
# The bench group "bfs/1k_fixture" becomes "bfs_1k_fixture" on disk.
BENCHES=(
  "bfs_1k_fixture/fan-1000"
  "ppr_1k_fixture/fan-1000"
)

PASS=true

for BENCH in "${BENCHES[@]}"; do
  ESTIMATES="target/criterion/${BENCH}/new/estimates.json"
  if [ ! -f "$ESTIMATES" ]; then
    echo "ERROR: criterion output not found at ${ESTIMATES}"
    echo "       Run: cargo bench --bench retrieval -- 1k_fixture"
    exit 1
  fi

  # Extract mean.point_estimate (nanoseconds) using python3 (always available in CI)
  MEAN_NS=$(python3 -c "
import json, sys
with open('${ESTIMATES}') as f:
    d = json.load(f)
print(int(d['mean']['point_estimate']))
")

  if [ "$MEAN_NS" -gt "$MAX_NS" ]; then
    echo "FAIL: ${BENCH} mean=${MEAN_NS}ns exceeds p95 budget of ${MAX_NS}ns (50ms)"
    PASS=false
  else
    MS=$(python3 -c "print(f'{${MEAN_NS}/1_000_000:.3f}')")
    echo "OK:   ${BENCH} mean=${MEAN_NS}ns (${MS}ms) — within 50ms budget"
  fi
done

if [ "$PASS" != "true" ]; then
  echo ""
  echo "One or more benchmarks exceeded the 50ms p95 budget."
  echo "See docs/adrs/ for the retrieval performance contract."
  exit 1
fi

echo ""
echo "All 1k-fixture benchmarks within p95 < 50ms budget."
