#!/usr/bin/env bash
# Compute the build id baked into the release binary as TRAVSR_BUILD_ID.
#
# Usage:  build-id.sh <ref_name> <commit_sha>
#         build-id.sh --self-test
#
# Output: a stable tag reports the bare version, a prerelease reports
#         <tag base>+<short commit>.
#
#           v1.0.0        -> 1.0.0
#           v1.0.0-beta.2 -> 1.0.0+98619a8
#
# A shipped stable version is a number users quote in bug reports and read in
# release notes, so it is the version and nothing else. `1.0.0+98619a8` is not
# that number.
#
# A prerelease still names its build, which is the ambiguity this script was
# written to remove: `v1.0.0-beta.1` reported a bare `1.0.0`, identical to what
# stable reports, so a tester's version string could not be attributed to a
# build. Keeping the suffix on prereleases only makes it a signal rather than
# noise: a `+` in a reported version means "not a stable release".
#
# The prerelease *suffix* is still stripped from the version part, so a beta
# reports `1.0.0+<sha>` rather than `1.0.0-beta.1+<sha>`. The base is what
# `verify-version` pins to the crate version, and the commit already identifies
# the build more precisely than the channel does.
#
# Promotion caveat. `release.yml`'s `promote` job republishes the source
# channel's signed artifacts byte for byte and never rebuilds, so promoting a
# prerelease to a stable tag would ship a binary that still reports
# `1.0.0+<sha>` under a stable tag, contradicting the rule above. `promote`
# guards against exactly that (see the stable-target check in release.yml) and
# has never been used: every release to date, v1.0.0 included, is a fresh build
# from its own tag pushed to the `push` trigger.
#
# This lives in a script rather than inline in the workflow so the stripping is
# executable outside a release run, and therefore testable. Inline, a regression
# here (dropping the `%%-*` line, say) would only be discovered by a stable
# release misreporting itself months later.
set -euo pipefail

compute() {
    local ref="$1" sha="$2" full base
    full="${ref#v}"      # v1.0.0-beta.1 -> 1.0.0-beta.1
    base="${full%%-*}"   # 1.0.0-beta.1  -> 1.0.0

    # A stable tag reports the version and nothing else. `travsr --version` on a
    # shipped stable release is a number a user quotes in a bug report and reads
    # in release notes, and `1.0.0+98619a8` is not that number.
    if [[ "$full" == "$base" ]]; then
        printf '%s\n' "$base"
        return
    fi

    # A prerelease still needs to name its build: the whole reason this script
    # exists is that v1.0.0-beta.1 reported a bare 1.0.0, so a tester's version
    # string could not be attributed to a build. Keeping `+<sha>` here and
    # dropping it on stable turns the suffix into a signal rather than noise:
    # a `+` means "not a stable release".
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

    # A stable release reports the bare version. This is the case the whole
    # change exists for: the number a user quotes is the number they were
    # shipped.
    check "v1.0.0"      "$sha" "1.0.0"
    check "v10.20.30"   "$sha" "10.20.30"

    # A prerelease still names its build, which is the ambiguity this script was
    # written to remove: v1.0.0-beta.1 used to report a bare 1.0.0.
    check "v1.0.0-beta.1" "$sha" "1.0.0+56c9329"
    check "v1.0.0-rc.1"   "$sha" "1.0.0+56c9329"

    # Different builds of the same base must differ: a tester on beta.1 must be
    # distinguishable from one on beta.2.
    check "v1.0.0-beta.2" "aaaaaaabbbbbbbcccccccdddddddeeeeeeefffffff" "1.0.0+aaaaaaa"

    # Multi digit and multi component versions must not be truncated.
    check "v10.20.30-rc.11" "$sha" "10.20.30+56c9329"

    # No channel ever leaks into the version part. Stable now has no `+` at all,
    # so this checks the part before it either way, over every tag shape the
    # release workflow accepts rather than one hand written string.
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

    # The distinguishing property, asserted directly rather than implied by the
    # cases above: a `+` in the reported version means "not a stable release".
    for ref in "v1.0.0" "v2.3.4" "v10.20.30"; do
        if [[ "$(compute "$ref" "$sha")" == *+* ]]; then
            echo "FAIL: stable $ref must report no build suffix" >&2
            failed=1
        fi
    done
    for ref in "v1.0.0-beta.1" "v1.0.0-rc.1" "v2.3.4-beta.99"; do
        if [[ "$(compute "$ref" "$sha")" != *+* ]]; then
            echo "FAIL: prerelease $ref must carry a build suffix" >&2
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
