#!/usr/bin/env bash
# Compute the build id baked into the release binary as TRAVSR_BUILD_ID.
#
# Usage:  build-id.sh <ref_name> <commit_sha>
#         build-id.sh --self-test
#
# Output: <tag base>+<short commit>, e.g. `1.0.0+56c9329`.
#
# The prerelease suffix is deliberately stripped. `release.yml`'s `promote` job
# reuses the source channel's signed artifacts byte for byte and never rebuilds,
# so beta.1 -> rc.1 -> stable all ship the same binary. A suffix baked into a
# beta build would follow it into the promoted stable release and make that
# release report itself as a beta forever, which is worse than the version
# ambiguity this whole mechanism exists to remove.
#
# The tag base survives promotion unchanged (`v1.0.0-beta.1` and `v1.0.0` share
# the base `1.0.0`, which `verify-version` already pins to the crate version),
# and the commit identifies the build itself: beta.1 and beta.2 differ because
# they are different commits, while a promoted stable matches the beta it came
# from because it genuinely is the same bits.
#
# This lives in a script rather than inline in the workflow so the stripping is
# executable outside a release run, and therefore testable. Inline, a regression
# here (dropping the `%%-*` line, say) would only be discovered by a stable
# release misreporting itself months later.
set -euo pipefail

compute() {
    local ref="$1" sha="$2" base
    base="${ref#v}"      # v1.0.0-beta.1 -> 1.0.0-beta.1
    base="${base%%-*}"   # 1.0.0-beta.1  -> 1.0.0
    printf '%s+%s\n' "$base" "${sha:0:7}"
}

self_test() {
    local failed=0
    check() {
        local ref="$1" sha="$2" want="$3" got
        got="$(compute "$ref" "$sha")"
        if [[ "$got" != "$want" ]]; then
            echo "FAIL: $ref -> $got (want $want)" >&2
            failed=1
        else
            echo "ok: $ref -> $got"
        fi
    }

    local sha="56c9329bdeadbeef0123456789abcdef01234567"

    # Every channel of the same release must produce the SAME id, because
    # promotion republishes the same artifact under each of these tags.
    check "v1.0.0-beta.1" "$sha" "1.0.0+56c9329"
    check "v1.0.0-rc.1"   "$sha" "1.0.0+56c9329"
    check "v1.0.0"        "$sha" "1.0.0+56c9329"

    # Different builds of the same base must differ, which is the whole point:
    # a tester on beta.1 must be distinguishable from one on beta.2.
    check "v1.0.0-beta.2" "aaaaaaabbbbbbbcccccccdddddddeeeeeeefffffff" "1.0.0+aaaaaaa"

    # Multi digit and multi component versions must not be truncated.
    check "v10.20.30-rc.11" "$sha" "10.20.30+56c9329"

    # The invariant the promote path depends on: no channel ever leaks into the
    # baked id. Asserted over every tag shape the release workflow accepts,
    # rather than over one hand written string.
    local ref
    for ref in "v1.0.0-beta.1" "v1.0.0-rc.1" "v1.0.0" "v2.3.4-beta.99"; do
        local got base
        got="$(compute "$ref" "$sha")"
        base="${got%%+*}"
        if [[ "$base" == *-* ]]; then
            echo "FAIL: prerelease suffix leaked into build id for $ref: $got" >&2
            failed=1
        fi
    done
    [[ $failed -eq 0 ]] && echo "build-id.sh self-test passed"
    return $failed
}

if [[ "${1:-}" == "--self-test" ]]; then
    self_test
else
    if [[ $# -ne 2 ]]; then
        echo "usage: build-id.sh <ref_name> <commit_sha> | --self-test" >&2
        exit 2
    fi
    compute "$1" "$2"
fi
