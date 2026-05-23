#!/usr/bin/env bash
# check-p95.sh — assert that all latency-gated benchmarks stay under 50 ms.
#
# Usage: ./scripts/check-p95.sh
#
# Reads Criterion's JSON estimates from target/criterion/ after running:
#   cargo bench -p travsr-retrieval --bench retrieval -- 1k_fixture
#   cargo bench -p travsr-indexer   --bench indexer
#
# Fails with exit code 1 if any measured mean exceeds MAX_NS.
#
# Criterion stores per-benchmark results in:
#   target/criterion/<group>/<bench>/new/estimates.json
# where estimates.json contains:
#   { "mean": { "point_estimate": <nanoseconds (f64)>, ... }, ... }
#
# NOTE: Criterion maps '/' in group names to '_' in the output directory.
#       "bfs/1k_fixture" → "bfs_1k_fixture", "rust/cold" → "rust_cold".

set -euo pipefail

# 50 milliseconds in nanoseconds
MAX_NS=50000000

# ── Retrieval benchmarks (query latency — 1k-file fixture) ───────────────────
RETRIEVAL_BENCHES=(
  "bfs_1k_fixture/fan-1000"
  "ppr_1k_fixture/fan-1000"
)

# ── Indexer benchmarks (parse latency — PERF-201 / Issue #121) ───────────────
# rust/travsr_core is optional: gracefully skipped when criterion output is
# absent (vendor-only CI that doesn't have the full workspace).
INDEXER_BENCHES_REQUIRED=(
  "rust_cold/simple_rs"
  "rust_warm/simple_rs"
)
INDEXER_BENCHES_OPTIONAL=(
  "rust_travsr_core/lib_rs"
)

PASS=true

# Locate the estimates.json for a given bench, checking multiple Criterion
# output directories in order of preference:
#   new/    — written on every bench run (canonical path)
#   pr/     — written when --save-baseline pr is used in CI
#   master/ — written when --save-baseline master is used on master push
#   base/   — written when --baseline is used for comparison
# Some Criterion versions / environments only write the named baseline and not
# "new/"; probing all candidates makes the gate robust across environments.
find_estimates() {
  local BENCH="$1"
  for SUFFIX in new pr master base; do
    local CANDIDATE="target/criterion/${BENCH}/${SUFFIX}/estimates.json"
    if [ -f "$CANDIDATE" ]; then
      echo "$CANDIDATE"
      return 0
    fi
  done
  return 1
}

check_bench() {
  local BENCH="$1"
  local OPTIONAL="${2:-false}"

  local ESTIMATES
  if ! ESTIMATES=$(find_estimates "$BENCH"); then
    if [ "$OPTIONAL" = "true" ]; then
      echo "SKIP: ${BENCH} — criterion output not found in any baseline dir (optional, skipping)"
      return
    fi
    echo "ERROR: criterion output not found for ${BENCH}"
    echo "       Checked: target/criterion/${BENCH}/{new,pr,master,base}/estimates.json"
    echo "       Run the relevant cargo bench command first."
    exit 1
  fi

  local MEAN_NS
  MEAN_NS=$(python3 -c "
import json
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
}

echo "=== Retrieval benchmarks ==="
for BENCH in "${RETRIEVAL_BENCHES[@]}"; do
  check_bench "$BENCH" "false"
done

echo ""
echo "=== Indexer benchmarks (PERF-201) ==="
for BENCH in "${INDEXER_BENCHES_REQUIRED[@]}"; do
  check_bench "$BENCH" "false"
done
for BENCH in "${INDEXER_BENCHES_OPTIONAL[@]}"; do
  check_bench "$BENCH" "true"
done

if [ "$PASS" != "true" ]; then
  echo ""
  echo "One or more benchmarks exceeded the 50ms p95 budget."
  echo "See docs/adrs/ for the retrieval performance contract."
  exit 1
fi

echo ""
echo "All gated benchmarks within p95 < 50ms budget."
