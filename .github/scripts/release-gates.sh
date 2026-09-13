#!/usr/bin/env bash
# Release promotion gate verdict (#871).
#
# release.yml runs four reusable suites as promotion gates (fuzz, OSV scan,
# accuracy nightly, A/B eval) and hands their `needs.<job>.result` values plus
# the target channel to this script. The script is the single place where the
# release policy is written down, so the same rule applies to both release
# paths (tag push and workflow_dispatch promotion) and can be unit-tested
# without a workflow run (see test-release-gates.sh).
#
# Policy, by target channel:
#
#   beta    no gates. The beta channel exists for fast iteration and the same
#           suites run nightly on master anyway.
#   rc      fuzz, osv, accuracy must have run and succeeded.
#   stable  fuzz, osv, accuracy, ab-eval must have run and succeeded.
#
# A required gate passes only with result `success`. Every other result blocks
# the release: `failure` and `cancelled` obviously, and `skipped` too, because
# a gate that never ran has not vouched for anything. This is the regression
# #871 reported: the old promote condition accepted skipped as success, and
# the gates were skipped on every real release.
#
# A gate that is not required for the channel may be `skipped`, but if it ran
# it must have succeeded too: "not required" means the channel does not wait
# for it, not that a red result is acceptable.
#
# Inputs (environment):
#   CHANNEL         beta | rc | stable   (needs.channel.outputs.channel)
#   GATE_FUZZ       needs['gate-fuzz'].result
#   GATE_OSV        needs['gate-osv'].result
#   GATE_ACCURACY   needs['gate-accuracy'].result
#   GATE_AB_EVAL    needs['gate-ab-eval'].result
#
# Exit 0 when every gate required for CHANNEL succeeded, 1 otherwise.
set -euo pipefail

CHANNEL="${CHANNEL:-}"

# Gates required per channel. Ordered so the table below reads the same way
# release.yml lists the jobs.
case "$CHANNEL" in
    beta)   REQUIRED="" ;;
    rc)     REQUIRED="fuzz osv accuracy" ;;
    stable) REQUIRED="fuzz osv accuracy ab-eval" ;;
    "")
        echo "ERROR: CHANNEL is empty; the channel job did not produce a verdict, refusing to release" >&2
        exit 1 ;;
    *)
        echo "ERROR: unknown release channel '$CHANNEL'; refusing to release" >&2
        exit 1 ;;
esac

# needs.<job>.result for each gate. An unset variable reads as empty, which is
# treated like any other non-success result.
result_of() {
    case "$1" in
        fuzz)     echo "${GATE_FUZZ:-}" ;;
        osv)      echo "${GATE_OSV:-}" ;;
        accuracy) echo "${GATE_ACCURACY:-}" ;;
        ab-eval)  echo "${GATE_AB_EVAL:-}" ;;
        *)        echo "ERROR: unknown gate '$1'" >&2; exit 1 ;;
    esac
}

is_required() {
    local g
    for g in $REQUIRED; do
        [[ "$g" == "$1" ]] && return 0
    done
    return 1
}

BLOCKED=0
printf 'Release channel: %s\n\n' "$CHANNEL"
printf '%-10s %-10s %-10s %s\n' "gate" "required" "result" "verdict"
for gate in fuzz osv accuracy ab-eval; do
    result="$(result_of "$gate")"
    if is_required "$gate"; then
        req=yes
        if [[ "$result" == "success" ]]; then
            verdict=ok
        else
            verdict="BLOCKS release"
            BLOCKED=1
        fi
    else
        req=no
        case "$result" in
            skipped|success) verdict="not required for $CHANNEL" ;;
            *)
                # Not required means the gate may be skipped for this channel,
                # not that it may fail. A gate that ran and did not succeed has
                # found something, whatever the channel; this also covers a
                # gate whose channel condition was widened without updating
                # the policy above.
                verdict="BLOCKS release (ran and did not succeed)"
                BLOCKED=1 ;;
        esac
    fi
    printf '%-10s %-10s %-10s %s\n' "$gate" "$req" "${result:-<unset>}" "$verdict"
done
echo

if [[ "$BLOCKED" -ne 0 ]]; then
    echo "ERROR: a gate blocks the '$CHANNEL' channel (see the table above)." >&2
    echo "  For a required gate, 'skipped' or 'cancelled' blocks just like 'failure': a gate" >&2
    echo "  that did not run has not vouched for this release. A gate that is not required" >&2
    echo "  still blocks if it ran and did not succeed. Fix the gate (or the workflow" >&2
    echo "  condition that stopped it running) and re-run the release." >&2
    exit 1
fi

if [[ -z "$REQUIRED" ]]; then
    echo "OK: the '$CHANNEL' channel is not gated on these suites (see policy above)."
else
    echo "OK: every gate required for '$CHANNEL' succeeded."
fi
