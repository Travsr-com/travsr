#!/usr/bin/env bash
# Regression coverage for the release promotion gates (#871).
#
# Two halves:
#
#   1. Runs release-gates.sh across the channel x result matrix and asserts the
#      exit code, so the policy (which gates a channel requires, and that
#      skipped/cancelled/failure all block) cannot drift silently.
#
#   2. Asserts the wiring in release.yml that the script cannot see from the
#      inside: the gate jobs run on both release paths (no event_name in their
#      condition), the verdict job is the only consumer of gate results and
#      always runs, and publish/promote depend on the verdict rather than on
#      the gates directly. This is what stops a future edit reintroducing the
#      "gate skipped, publish anyway" hole.
#
# Zero dependencies beyond bash, grep and awk. Run from anywhere:
#   bash .github/scripts/test-release-gates.sh
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT="$HERE/release-gates.sh"
RELEASE_YML="$HERE/../workflows/release.yml"

PASS=0
FAIL=0

ok()   { PASS=$((PASS + 1)); echo "  ok    $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  FAIL  $1" >&2; }

# expect <0|1> <label> <CHANNEL> <fuzz> <osv> <accuracy> <ab-eval>
expect() {
    local want="$1" label="$2" channel="$3" fuzz="$4" osv="$5" acc="$6" ab="$7"
    local got=0
    CHANNEL="$channel" GATE_FUZZ="$fuzz" GATE_OSV="$osv" GATE_ACCURACY="$acc" GATE_AB_EVAL="$ab" \
        bash "$SCRIPT" >/dev/null 2>&1 || got=$?
    if [[ "$got" -eq "$want" ]]; then
        ok "$label (exit $got)"
    else
        fail "$label: expected exit $want, got $got"
    fi
}

echo "== release-gates.sh policy matrix =="

# Stable: all four required; only success passes.
expect 0 "stable, all gates success -> publish"                 stable success success success success
expect 1 "stable, accuracy failure -> block"                    stable success success failure success
expect 1 "stable, ab-eval failure -> block"                     stable success success success failure
expect 1 "stable, fuzz failure -> block"                        stable failure success success success
expect 1 "stable, osv failure -> block"                         stable success failure success success
expect 1 "stable, accuracy skipped -> block (#871)"             stable success success skipped success
expect 1 "stable, ab-eval skipped -> block (#871)"              stable success success success skipped
expect 1 "stable, accuracy cancelled -> block"                  stable success success cancelled success
expect 1 "stable, ab-eval cancelled -> block"                   stable success success success cancelled
expect 1 "stable, every gate skipped -> block (old push path)"  stable skipped skipped skipped skipped
expect 1 "stable, gate result unset -> block"                   stable success success "" success

# RC: fuzz, osv, accuracy required; ab-eval is not consulted.
expect 0 "rc, required gates success, ab-eval skipped -> publish" rc success success success skipped
expect 0 "rc, required gates success, ab-eval failure -> publish (not required)" rc success success success failure
expect 1 "rc, accuracy skipped -> block (#871)"                 rc success success skipped skipped
expect 1 "rc, accuracy failure -> block"                        rc success success failure skipped
expect 1 "rc, fuzz skipped -> block"                            rc skipped success success skipped
expect 1 "rc, osv cancelled -> block"                           rc success cancelled success skipped

# Beta: not gated on these suites; the gates are skipped by design.
expect 0 "beta, every gate skipped -> publish"                  beta skipped skipped skipped skipped
expect 0 "beta, gates ran and failed -> publish (not required)" beta failure failure failure failure

# Guard rails: no channel or an unknown channel never releases.
expect 1 "empty channel -> block"                               "" success success success success
expect 1 "unknown channel -> block"                             nightly success success success success

echo
echo "== release.yml wiring =="

# Prints the body of one top-level job: from `  <job>:` to the next job key.
job_block() {
    awk -v job="$1" '
        $0 ~ "^  " job ":[[:space:]]*$" { p = 1; print; next }
        p && /^  [A-Za-z0-9_-]+:[[:space:]]*$/ { exit }
        p { print }
    ' "$RELEASE_YML"
}

# Everything between `if:` and the next job-level key, joined onto one line,
# so multi-line `if: |` blocks are checked as a whole.
#
# Both consumers below read their whole input instead of exiting early. Under
# `set -o pipefail` an early exit (awk `exit`, `head -1`, `grep -m1`) can hit
# the producing awk with SIGPIPE and turn the pipeline into exit 141, which
# `set -e` then treats as a test failure. Linux delivers that signal; Git Bash
# on Windows does not, so the race only showed in CI.
job_if() {
    job_block "$1" | awk '
        !p && /^    if:/ { p = 1; sub(/^    if:[[:space:]]*\|?[[:space:]]*/, ""); printf "%s ", $0; next }
        p && !done && /^    [A-Za-z0-9_-]+:/ { done = 1 }
        p && !done { gsub(/^[[:space:]]+/, ""); printf "%s ", $0 }
    '
}

job_needs() {
    job_block "$1" | awk '!found && /^    needs:/ { print; found = 1 }'
}

assert_contains() {
    local label="$1" haystack="$2" needle="$3"
    if [[ "$haystack" == *"$needle"* ]]; then ok "$label"; else fail "$label: expected to find '$needle' in: $haystack"; fi
}
assert_not_contains() {
    local label="$1" haystack="$2" needle="$3"
    if [[ "$haystack" != *"$needle"* ]]; then ok "$label"; else fail "$label: must not contain '$needle': $haystack"; fi
}

for gate in gate-fuzz gate-osv gate-accuracy gate-ab-eval; do
    block="$(job_block "$gate")"
    [[ -n "$block" ]] || { fail "$gate job is missing from release.yml"; continue; }
    cond="$(job_if "$gate")"
    # The #871 hole: gating on the event meant the gates only ran on the
    # promotion path, which no release has ever taken.
    assert_not_contains "$gate runs on both release paths (no event_name in its condition)" "$cond" "github.event_name"
    assert_contains     "$gate is scoped by channel"                                          "$cond" "needs.channel.outputs.channel"
done
assert_contains "gate-ab-eval is required for stable only" "$(job_if gate-ab-eval)" "== 'stable'"
for gate in gate-fuzz gate-osv gate-accuracy; do
    assert_contains "$gate is required past beta" "$(job_if "$gate")" "!= 'beta'"
done

verdict="$(job_block gates)"
[[ -n "$verdict" ]] || fail "gates verdict job is missing from release.yml"
assert_contains "gates verdict depends on every gate" "$(job_needs gates)" "gate-fuzz"
assert_contains "gates verdict depends on every gate" "$(job_needs gates)" "gate-osv"
assert_contains "gates verdict depends on every gate" "$(job_needs gates)" "gate-accuracy"
assert_contains "gates verdict depends on every gate" "$(job_needs gates)" "gate-ab-eval"
assert_contains "gates verdict depends on channel"    "$(job_needs gates)" "channel"
# A skipped gate skips its dependants unless the condition carries a status
# function (actions/runner#491). The verdict must run so it can say no.
assert_contains "gates verdict runs even when a gate was skipped" "$(job_if gates)" "!cancelled()"
assert_contains "gates verdict runs release-gates.sh" "$verdict" "release-gates.sh"
for var in CHANNEL GATE_FUZZ GATE_OSV GATE_ACCURACY GATE_AB_EVAL; do
    assert_contains "gates verdict passes $var to the script" "$verdict" "$var:"
done

for consumer in publish promote; do
    needs="$(job_needs "$consumer")"
    assert_contains     "$consumer depends on the gates verdict"           "$needs" "gates"
    for gate in gate-fuzz gate-osv gate-accuracy gate-ab-eval; do
        assert_not_contains "$consumer does not bypass the verdict via $gate" "$needs" "$gate"
    done
    cond="$(job_if "$consumer")"
    # No status function in the condition means GitHub adds success(), so
    # publish/promote run only when channel, build and the verdict succeeded.
    assert_not_contains "$consumer relies on the implicit success() of its needs" "$cond" "always()"
    assert_not_contains "$consumer does not reason about gate results itself"     "$cond" "gate-"
done

# The word is only ever allowed in the verdict script.
if grep -nE "== *'skipped'" "$RELEASE_YML" >/dev/null; then
    fail "release.yml compares a job result to 'skipped'; gate results are decided in release-gates.sh only"
else
    ok "release.yml never treats a skipped job as a result to accept"
fi

echo
echo "passed: $PASS  failed: $FAIL"
[[ "$FAIL" -eq 0 ]]
