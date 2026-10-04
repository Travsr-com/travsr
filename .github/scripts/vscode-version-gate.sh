#!/usr/bin/env bash
#
# #882: the VS Code extension pins the binary it offers to download
# (`DOWNLOAD_VERSION` in packages/travsr-vscode/src/installer.ts). Three
# different workflows have to reason about that pin against "the current stable
# CLI release", and getting the direction of the comparison wrong in any of them
# either lets a stale pin ship or deadlocks the release process. The logic lives
# here, once, so all three agree and so the failure modes can be tested
# (.github/scripts/test-vscode-version-gate.sh) instead of only being
# exercised by cutting a release.
#
# The four gates on the pin, and who owns which direction:
#
#   release.yml       tag base == DOWNLOAD_VERSION when a stable CLI release is
#                     cut. Authoritative for "the version being released".
#   publish-pin       DOWNLOAD_VERSION == latest stable release when the
#                     extension is published. Authoritative for "what this
#                     .vsix will install".
#   tree-pin          DOWNLOAD_VERSION is not BEHIND the latest stable release.
#                     Catches the pin going stale on master between releases,
#                     which is the #882 defect, without contradicting the two
#                     equality gates above (see cmd_tree_pin).
#   marketplace-drift what is ALREADY published installs the current release.
#                     The only check that can see a release cut after an
#                     extension shipped, which is how #882 actually happened.
#
# Usage: vscode-version-gate.sh <latest-stable|tree-pin|publish-pin|marketplace-drift>
set -euo pipefail

GALLERY_URL='https://marketplace.visualstudio.com/_apis/public/gallery/extensionquery'
EXTENSION_ID='travsr.travsr-vscode'
INSTALLER_TS='packages/travsr-vscode/src/installer.ts'
SEMVER_RE='^[0-9]+\.[0-9]+\.[0-9]+$'

# Before anything else, and before the `cd` below shells out to git. Every
# tool failure further down is deliberately reported in terms of what could
# not be learned ("no stable release found", "response was not the expected
# JSON"), because the data being wrong and the data being unreadable must both
# fail. That wording is only honest if the tools themselves are known to be
# present: with jq absent, `jq ... 2>/dev/null` fails exactly like an empty
# release list, and a machine without jq would have read "no stable release"
# as a fact about the repository. GitHub's runners ship all four; a laptop may
# not, and that is where a maintainer reproduces a red run.
missing=""
for tool in git gh curl jq; do
  command -v "$tool" >/dev/null 2>&1 || missing="$missing $tool"
done
if [ -n "$missing" ]; then
  echo "::error::vscode-version-gate.sh needs these tools on PATH and cannot find them:$missing" >&2
  exit 1
fi

cd "$(git rev-parse --show-toplevel)"

# --- reporting -------------------------------------------------------------
#
# Drift, an unreachable gallery and a missing tag are one kind of finding: a
# fact about state outside this repository that no pull request can change. A
# pull request reports them and passes; every other trigger fails, because
# those runs are the ones a human can act on. Note this is the routing for
# "could not determine" too: learning nothing must never be reported as no
# drift.
report_external() {
  if [ "${GITHUB_EVENT_NAME:-}" = "pull_request" ]; then
    echo "::warning::$1 (informational on a pull request: no pull request can change this)"
    return 0
  fi
  echo "::error::$1"
  return 1
}

finish_external() {
  if report_external "$1"; then exit 0; fi
  exit 1
}

# --- semver ----------------------------------------------------------------

# Echoes -1, 0 or 1 for a<b, a==b, a>b. Both arguments must already have passed
# SEMVER_RE, so there is no prerelease ordering to get wrong here.
semver_cmp() {
  local -a A B
  local i av bv
  IFS=. read -r -a A <<<"$1"
  IFS=. read -r -a B <<<"$2"
  for i in 0 1 2; do
    av=$((10#${A[i]}))
    bv=$((10#${B[i]}))
    if [ "$av" -lt "$bv" ]; then echo -1; return 0; fi
    if [ "$av" -gt "$bv" ]; then echo 1; return 0; fi
  done
  echo 0
}

# --- inputs ----------------------------------------------------------------

# `gh release list` is ordered by creation date, so the [0] this replaced was
# the newest-CREATED release, not the highest version. A patch backported onto
# an older line after a newer minor shipped would take that slot, and every
# check in this file would then demand a downgrade of a correct pin. Take the
# semver maximum instead. The 100-release window is still creation-ordered, so
# it holds the true maximum unless more than 100 releases are cut after it.
#
# Stable only, for the same reason release.yml exempts prereleases from its own
# lockstep check: DOWNLOAD_VERSION is the one recovery path a user with no
# binary gets, so it names a stable release or nothing. Comparing against a beta
# would either demand a pin at a tag ordinary users should not be sent to, or a
# pin at a stable tag that does not exist yet, which is a guaranteed 404.
#
# vscode-v* is a separate tag family and must not compete here; the shape
# filter drops it, and also drops any prerelease whose isPrerelease flag was
# never set.
latest_stable() {
  local json tags best t
  if ! json=$(gh release list --repo "${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is unset}" \
                --limit 100 --json tagName,isPrerelease,isDraft 2>&1); then
    echo "could not list releases: $json" >&2
    return 1
  fi
  tags=$(printf '%s' "$json" | jq -er '
           .[] | select(.isPrerelease == false and .isDraft == false)
               | .tagName | select(test("^v[0-9]+\\.[0-9]+\\.[0-9]+$"))' 2>/dev/null) || {
    echo "no stable release found in the release list" >&2
    return 1
  }
  best=""
  for t in $tags; do
    if [ -z "$best" ] || [ "$(semver_cmp "${t#v}" "${best#v}")" = "1" ]; then
      best="$t"
    fi
  done
  [ -n "$best" ] || { echo "no stable release found in the release list" >&2; return 1; }
  printf '%s\n' "$best"
}

# Reads DOWNLOAD_VERSION out of an installer.ts arriving on stdin, so the same
# parse serves the working tree and `git show <tag>:...`.
read_pin() {
  local pin
  pin=$(sed -n 's/.*export const DOWNLOAD_VERSION[^"]*"\([^"]*\)".*/\1/p' | head -n1)
  [ -n "$pin" ] || return 1
  printf '%s\n' "$pin"
}

# curl --fail turns an HTTP error into a non-zero exit instead of letting a
# 200-shaped error page reach jq; --retry covers the 5xx, 429 and timeout
# shapes a gallery blip takes. The `|| rc=$?` matters as much as the flags: a
# bare `X=$(curl ... | jq ...)` under `set -euo pipefail` aborts the whole run
# at the first parse error, so the event-aware reporting below never executes
# and a WAF challenge page reddens a pull request with a raw jq trace.
published_version() {
  local body ver rc=0
  body=$(curl -sS --fail --max-time 30 --retry 3 --retry-delay 2 \
           -X POST "$GALLERY_URL" \
           -H 'Content-Type: application/json' \
           -H 'Accept: application/json;api-version=7.2-preview.1' \
           -d "{\"filters\":[{\"criteria\":[{\"filterType\":7,\"value\":\"$EXTENSION_ID\"}]}],\"flags\":914}" \
        ) || rc=$?
  if [ "$rc" -ne 0 ]; then
    echo "marketplace request failed (curl exit $rc)" >&2
    return 1
  fi
  # -e so an absent field exits non-zero instead of printing "null"; invalid
  # JSON exits here too, which is the whole point.
  ver=$(printf '%s' "$body" | jq -er '.results[0].extensions[0].versions[0].version' 2>/dev/null) || {
    echo "marketplace response was not the expected JSON" >&2
    return 1
  }
  if ! [[ "$ver" =~ $SEMVER_RE ]]; then
    echo "marketplace returned an unusable version: $ver" >&2
    return 1
  fi
  printf '%s\n' "$ver"
}

# --- commands --------------------------------------------------------------

cmd_latest_stable() { latest_stable; }

# What a pull request CAN fix. The pin must not fall behind the latest stable
# release, which is #882 arriving on master between releases.
#
# Ahead is deliberately legal, and requiring equality here is what deadlocked
# the release process: a release PR for v1.1.0 bumps this pin to 1.1.0 while
# v1.0.0 is still the latest published release, and release.yml then requires
# tag base == pin when the tag is cut. An equality check here would tell the
# release engineer to revert the exact bump release.yml is about to demand,
# and the paths filter on this workflow is precisely what a release PR touches.
#
# Nothing is lost by allowing ahead. Both equality gates compare against a real
# tag (release.yml at tag time, publish-pin at publish time), so a pin naming a
# version that never becomes a release still cannot reach the Marketplace.
cmd_tree_pin() {
  local pin latest cmp
  pin=$(read_pin <"$INSTALLER_TS") || {
    echo "::error::could not read DOWNLOAD_VERSION from $INSTALLER_TS"
    exit 1
  }
  # A malformed pin is a bug in this tree on every trigger, never an external
  # fact, so it fails everywhere rather than routing through report_external.
  if ! [[ "$pin" =~ $SEMVER_RE ]]; then
    echo "::error::DOWNLOAD_VERSION \"$pin\" in $INSTALLER_TS is not a release version (expected X.Y.Z)"
    exit 1
  fi
  latest=$(latest_stable) || { echo "::error::could not resolve the latest stable release"; exit 1; }
  latest="${latest#v}"

  cmp=$(semver_cmp "$pin" "$latest")
  if [ "$cmp" = "-1" ]; then
    echo "::error::DOWNLOAD_VERSION $pin is behind the latest stable release v$latest"
    echo "the extension would install a superseded binary; bump DOWNLOAD_VERSION in $INSTALLER_TS" >&2
    exit 1
  fi
  if [ "$cmp" = "1" ]; then
    echo "::notice::DOWNLOAD_VERSION $pin is ahead of the latest stable release v$latest, which is what a release in flight looks like; release.yml gates it at tag time"
    exit 0
  fi
  echo "OK: DOWNLOAD_VERSION $pin is the latest stable release"
}

# Publish time, so equality: this is the moment the pin becomes what users get,
# and by now v$pin has to be a release that actually exists.
cmd_publish_pin() {
  local pin latest
  pin=$(read_pin <"$INSTALLER_TS") || {
    echo "could not read DOWNLOAD_VERSION from $INSTALLER_TS" >&2
    exit 1
  }
  latest=$(latest_stable) || { echo "could not resolve the latest stable release" >&2; exit 1; }
  latest="${latest#v}"
  [ "$pin" = "$latest" ] || {
    echo "DOWNLOAD_VERSION $pin != latest stable release v$latest" >&2
    echo "the extension would install a binary that is not the current release" >&2
    echo "fix $INSTALLER_TS" >&2
    exit 1
  }
  echo "OK: DOWNLOAD_VERSION $pin is the latest stable release"
}

# The release-side detector. Asks the Marketplace what is published, resolves
# that version's vscode-v* tag to read the pin that actually shipped (cheaper
# and more exact than downloading and unpacking a .vsix for one constant), and
# compares against the current stable release.
cmd_marketplace_drift() {
  local latest published tag dl
  latest=$(latest_stable) || { echo "::error::could not resolve the latest stable release"; exit 1; }
  echo "latest stable binary release: $latest"

  published=$(published_version) ||
    finish_external "could not determine the published extension version from the Marketplace, so drift cannot be ruled out"
  echo "published extension version: $published"

  tag="vscode-v$published"
  git rev-parse -q --verify "refs/tags/$tag^{commit}" >/dev/null 2>&1 ||
    finish_external "the Marketplace serves extension $published but no $tag tag exists here, so what that build installs cannot be established"

  dl=$(git show "$tag:$INSTALLER_TS" 2>/dev/null | read_pin) || dl=""
  [ -n "$dl" ] ||
    finish_external "could not read DOWNLOAD_VERSION from $tag"
  echo "that build installs: v$dl"

  if [ "v$dl" = "$latest" ]; then
    echo "OK: the published extension installs $latest, the current release"
    exit 0
  fi

  cat >&2 <<REPORT

  Marketplace extension  $published  (tag $tag)
    DOWNLOAD_VERSION     $dl
  Current stable release $latest

  Every fresh install that reaches the download prompt gets a v$dl binary
  instead of ${latest#v}. Republish the extension: bump
  packages/travsr-vscode/package.json, confirm DOWNLOAD_VERSION is
  ${latest#v}, and cut a new vscode-v* tag.
REPORT

  finish_external "the published extension installs v$dl but the current release is $latest"
}

case "${1:-}" in
  latest-stable)      cmd_latest_stable ;;
  tree-pin)           cmd_tree_pin ;;
  publish-pin)        cmd_publish_pin ;;
  marketplace-drift)  cmd_marketplace_drift ;;
  *)
    echo "usage: $0 <latest-stable|tree-pin|publish-pin|marketplace-drift>" >&2
    exit 2
    ;;
esac
