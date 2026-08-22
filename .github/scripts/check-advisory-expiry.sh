#!/usr/bin/env bash
# Surfaces osv-scanner.toml `ignoreUntil` deadlines on PR CI, before they bite.
#
# Why this exists: an expiring ignore is only a useful reminder if somebody
# sees it lapse. Relying on the nightly OSV Scan going red does not achieve
# that here. At the time this was written four scheduled workflows on this
# repo were chronically red (OSV Scan had been failing for 76 consecutive
# nightlies, Docs lane nightly had no success in its last 100 runs), so a
# fifth red nightly would have been invisible for exactly the same reason.
# Worse, osv-scanner.toml feeds `gate-osv` in release.yml, so a lapsed ignore
# does not merely add noise, it blocks every non-beta promotion, and the first
# person to find out would be whoever tried to cut a release.
#
# This runs on every PR instead, which is the one surface the team demonstrably
# does watch, and it fires well before the deadline rather than on it.
set -euo pipefail

CONFIG="${ADVISORY_CONFIG:-osv-scanner.toml}"
WARN_DAYS="${ADVISORY_WARN_DAYS:-60}"
FAIL_DAYS="${ADVISORY_FAIL_DAYS:-14}"
# Overridable purely so the thresholds themselves can be exercised in a test.
TODAY_EPOCH="${ADVISORY_TODAY_EPOCH:-$(date -u +%s)}"

if [[ ! -f "$CONFIG" ]]; then
    echo "no $CONFIG, nothing to check"
    exit 0
fi

# Pull "<id> <date>" pairs out of each [[IgnoredVulns]] block. Anchored to the
# start of a line so prose inside a reason string cannot be mistaken for a key.
PAIRS="$(awk '
    /^[[:space:]]*\[\[IgnoredVulns\]\]/ { id = ""; next }
    /^[[:space:]]*id[[:space:]]*=/      { if (match($0, /"[^"]+"/)) id = substr($0, RSTART+1, RLENGTH-2); next }
    /^[[:space:]]*ignoreUntil[[:space:]]*=/ {
        # Emit the raw value rather than only well-formed dates. If a malformed
        # value were skipped here the guard would silently disable itself,
        # which is the exact failure this script exists to prevent. Let the
        # shell parse it and report an unusable date as an error.
        val = $0
        sub(/^[^=]*=[[:space:]]*/, "", val)
        sub(/[[:space:]]*#.*$/, "", val)
        gsub(/[[:space:]]|"/, "", val)
        print (id == "" ? "(unknown-id)" : id), (val == "" ? "(empty)" : val)
    }
' "$CONFIG")"

if [[ -z "$PAIRS" ]]; then
    echo "no ignoreUntil deadlines in $CONFIG"
    exit 0
fi

STATUS=0
while read -r id expiry; do
    [[ -z "$id" ]] && continue

    # Validate the shape before handing it to date. GNU date parses loosely
    # (it accepts parenthesised text as a comment and quietly falls back to
    # today), so a malformed value would otherwise be read as "expires now"
    # instead of being reported as broken.
    if [[ ! "$expiry" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}$ ]]; then
        echo "::error::$CONFIG: $id has an unparseable ignoreUntil '$expiry' (expected YYYY-MM-DD)"
        STATUS=1
        continue
    fi

    if ! expiry_epoch="$(date -u -d "$expiry" +%s 2>/dev/null)"; then
        # BSD/macOS date needs an explicit input format.
        if ! expiry_epoch="$(date -u -j -f %Y-%m-%d "$expiry" +%s 2>/dev/null)"; then
            echo "::error::$CONFIG: $id has an unparseable ignoreUntil '$expiry'"
            STATUS=1
            continue
        fi
    fi

    days_left=$(( (expiry_epoch - TODAY_EPOCH) / 86400 ))

    if (( days_left < 0 )); then
        echo "::error::$id: ignoreUntil $expiry has LAPSED ($(( -days_left ))d ago). OSV Scan is failing and gate-osv is blocking non-beta promotions. Triage it, do not extend the date mechanically."
        STATUS=1
    elif (( days_left <= FAIL_DAYS )); then
        echo "::error::$id: ignoreUntil $expiry is ${days_left}d away. Once it lapses, gate-osv blocks every non-beta promotion. Resolve the advisory or make a deliberate call now."
        STATUS=1
    elif (( days_left <= WARN_DAYS )); then
        echo "::warning::$id: ignoreUntil $expiry is ${days_left}d away. It will block releases via gate-osv when it lapses."
        echo "  warn  $id expires in ${days_left}d ($expiry)"
    else
        echo "  ok    $id expires in ${days_left}d ($expiry)"
    fi
done <<< "$PAIRS"

exit "$STATUS"
