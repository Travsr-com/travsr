#!/usr/bin/env bash
#
# Tests for .github/scripts/vscode-version-gate.sh (#882).
#
# Same shape as test-release-gates.sh (#871): zero dependencies beyond bash and
# the tools the gate itself shells out to, a PASS/FAIL tally, runnable from
# anywhere with `bash .github/scripts/test-vscode-version-gate.sh`.
#
# The gate decides whether a release can be cut and whether a pull request goes
# red, and every one of its interesting cases involves a state that is awkward
# to reach on purpose: a gallery outage, a WAF challenge page, a backported
# patch release, a release PR mid-flight. PR #888 shipped this logic inline in
# YAML and both defects found in review (a release-PR deadlock, and `set -e`
# killing the run before the authored error could print) were only findable by
# extracting the step out of the workflow by hand and running it. So the logic
# lives in a script and the states are simulated here.
#
# `gh` and `curl` are stubbed on PATH rather than behind a flag in the gate:
# the code under test is the same code that runs in CI, with no test-only
# branch in it. The repository is a real throwaway git repo with real tags,
# because the drift check reads a shipped pin with `git show <tag>:...`.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GATE="$HERE/vscode-version-gate.sh"
# Overridable so a doctored copy (a mutant) can be run against the wiring
# assertions at the end without editing the real workflow.
PUBLISH_YML="${PUBLISH_YML:-$HERE/../workflows/vscode-publish.yml}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

PASS=0
FAIL=0

ok()   { PASS=$((PASS + 1)); echo "  ok    $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  FAIL  $1" >&2; }

# --- stubs -----------------------------------------------------------------

mkdir -p "$WORK/bin"

cat > "$WORK/bin/gh" <<'STUB'
#!/usr/bin/env bash
if [ -n "${FAKE_GH_FAIL:-}" ]; then
  echo "gh: HTTP 403: API rate limit exceeded" >&2
  exit 1
fi
cat "$FAKE_RELEASES"
STUB

# Mimics curl's contract as the gate relies on it: --fail means an HTTP error
# is a non-zero exit with no body, and a transport failure is a non-zero exit
# too. 22 = HTTP >= 400, 28 = timeout, 6 = DNS failure.
cat > "$WORK/bin/curl" <<'STUB'
#!/usr/bin/env bash
rc="${FAKE_CURL_RC:-0}"
if [ "$rc" -ne 0 ]; then exit "$rc"; fi
cat "$FAKE_BODY"
STUB

chmod +x "$WORK/bin/gh" "$WORK/bin/curl"
export PATH="$WORK/bin:$PATH"

# --- fixtures --------------------------------------------------------------

RELEASES="$WORK/releases.json"
BODY="$WORK/body.json"
export FAKE_RELEASES="$RELEASES"
export FAKE_BODY="$BODY"
export GITHUB_REPOSITORY="Travsr-com/travsr"

# Ordered newest-created first, the way `gh release list` returns it. v0.11.1
# is a patch backported onto the 0.11 line AFTER v1.0.0 shipped, so it sits at
# index 0 while v1.0.0 is the highest version. Taking [0] here is the bug the
# reviewer flagged; every expectation below assumes v1.0.0 wins.
cat > "$RELEASES" <<'JSON'
[
  {"tagName":"vscode-v0.11.0","isPrerelease":false,"isDraft":false},
  {"tagName":"v0.11.1","isPrerelease":false,"isDraft":false},
  {"tagName":"v1.1.0","isPrerelease":true,"isDraft":false},
  {"tagName":"v1.0.1","isPrerelease":false,"isDraft":true},
  {"tagName":"v1.0.0","isPrerelease":false,"isDraft":false},
  {"tagName":"v0.9.0","isPrerelease":false,"isDraft":false}
]
JSON

marketplace_body() {  # $1 = published version
  cat > "$BODY" <<JSON
{"results":[{"extensions":[{"versions":[{"version":"$1"}]}]}]}
JSON
}

REPO="$WORK/repo"
mkdir -p "$REPO/packages/travsr-vscode/src"
git init -q "$REPO"
git -C "$REPO" config user.email ci@example.invalid
git -C "$REPO" config user.name CI

set_pin() {  # $1 = DOWNLOAD_VERSION literal to write into the working tree
  printf 'export const DOWNLOAD_VERSION = "%s";\n' "$1" \
    > "$REPO/packages/travsr-vscode/src/installer.ts"
}

# Two historical extension tags: 0.11.0 shipped a pin at 0.11.0 (the #882
# build), 0.12.0 shipped a pin at 1.0.0 (the republish that fixes it).
set_pin 0.11.0
git -C "$REPO" add -A && git -C "$REPO" commit -qm "vscode 0.11.0"
git -C "$REPO" tag vscode-v0.11.0
set_pin 1.0.0
git -C "$REPO" add -A && git -C "$REPO" commit -qm "vscode 0.12.0"
git -C "$REPO" tag vscode-v0.12.0

# --- harness ---------------------------------------------------------------

# run <event> <command>; leaves RC and OUT set.
run() {
  local event="$1" cmd="$2"
  OUT="$(cd "$REPO" && GITHUB_EVENT_NAME="$event" bash "$GATE" "$cmd" 2>&1)"
  RC=$?
}

expect() {  # <name> <want_rc> <want_substring>
  local name="$1" want_rc="$2" want="$3"
  if [ "$RC" != "$want_rc" ]; then
    fail "$name (want exit $want_rc, got $RC)"
    printf '%s\n' "$OUT" >&2
    return
  fi
  case "$OUT" in
    *"$want"*) ;;
    *)
      fail "$name (want output containing: $want)"
      printf '%s\n' "$OUT" >&2
      return
      ;;
  esac
  ok "$name"
}

# Guards against a raw tool error leaking through. "Does not contain X" is
# vacuously true of empty output, so a gate that printed nothing (a missing
# tool, a crash before the first echo) must not pass here; every refute
# therefore also demands that the run produced something.
refute() {  # <name> <unwanted_substring>
  local name="$1" unwanted="$2"
  if [ -z "$OUT" ]; then
    fail "$name (no output at all, so absence of '$unwanted' proves nothing)"
    return
  fi
  case "$OUT" in
    *"$unwanted"*)
      fail "$name (output must not contain: $unwanted)"
      printf '%s\n' "$OUT" >&2
      ;;
    *) ok "$name" ;;
  esac
}

# --- latest-stable ---------------------------------------------------------

unset FAKE_GH_FAIL FAKE_CURL_RC 2>/dev/null || true

# One run, one positive assertion on it, then the refutes ride on the same
# output. A refute on its own run could pass on a gate that failed silently.
run schedule latest-stable
expect "latest-stable takes the semver maximum, not the newest created" 0 "v1.0.0"
refute "latest-stable ignores the newer backport v0.11.1" "v0.11.1"
refute "latest-stable ignores the vscode-v* tag family" "vscode-"

FAKE_GH_FAIL=1 run schedule latest-stable
expect "a gh failure is reported, never treated as no releases" 1 "could not list releases"
unset FAKE_GH_FAIL

# The tools the gate shells out to are checked before anything else runs, so a
# machine without jq gets told that, rather than "no stable release found",
# which reads as a fact about the repository. PATH is reduced to the stub
# directory alone. The interpreter is named by absolute path ($BASH): bash
# applies a preceding `PATH=` assignment BEFORE searching for the command word,
# so a bare `bash` here would itself be "command not found" (exit 127) and
# the gate would never start.
OUT="$(cd "$REPO" && PATH="$WORK/bin" GITHUB_EVENT_NAME=schedule "$BASH" "$GATE" latest-stable 2>&1)"
RC=$?
expect "a missing tool is named, before any check runs" 1 "cannot find them"
expect "the missing tool named is jq" 1 " jq"
refute "a missing tool is never reported as an empty release list" "no stable release found"

# --- tree-pin --------------------------------------------------------------

set_pin 1.0.0
run pull_request tree-pin
expect "tree pin equal to the latest release passes" 0 "OK: DOWNLOAD_VERSION 1.0.0"

set_pin 0.11.0
run pull_request tree-pin
expect "tree pin behind the latest release fails the PR (#882)" 1 "::error::DOWNLOAD_VERSION 0.11.0 is behind"

set_pin 0.11.0
run schedule tree-pin
expect "tree pin behind the latest release fails on schedule too" 1 "is behind the latest stable release"

# The blocker from review: a release PR for v1.1.0 bumps the pin ahead of the
# published latest while release.yml is about to require exactly that value at
# tag time. Requiring equality here made the two constraints unsatisfiable.
set_pin 1.1.0
run pull_request tree-pin
expect "tree pin ahead does not deadlock a release PR" 0 "::notice::DOWNLOAD_VERSION 1.1.0 is ahead"

set_pin 1.1.0
run release tree-pin
expect "tree pin ahead does not fail a release run either" 0 "is ahead of the latest stable release"

set_pin "1.0"
run pull_request tree-pin
expect "a malformed pin fails on every trigger" 1 "is not a release version"

set_pin 1.0.0
printf 'const nothing = 1;\n' > "$REPO/packages/travsr-vscode/src/installer.ts"
run pull_request tree-pin
expect "an unreadable pin fails rather than passing empty" 1 "could not read DOWNLOAD_VERSION"

set_pin 1.0.0
FAKE_GH_FAIL=1 run pull_request tree-pin
expect "tree-pin says so when the release list cannot be read" 1 "could not resolve the latest stable release"
unset FAKE_GH_FAIL

# --- publish-pin -----------------------------------------------------------

set_pin 1.0.0
run push publish-pin
expect "publish pin equal to the latest release publishes" 0 "OK: DOWNLOAD_VERSION 1.0.0"

set_pin 0.11.0
run push publish-pin
expect "publish pin behind the latest release blocks the publish" 1 "!= latest stable release v1.0.0"

# Equality at publish time is what stops a pin naming a release that does not
# exist from reaching the Marketplace, which is why tree-pin can allow ahead.
set_pin 1.1.0
run push publish-pin
expect "publish pin ahead of any real release blocks the publish" 1 "!= latest stable release v1.0.0"

# --- marketplace-drift -----------------------------------------------------

set_pin 1.0.0

marketplace_body 0.12.0
run schedule marketplace-drift
expect "a published extension installing the current release is clean" 0 "OK: the published extension installs v1.0.0"

marketplace_body 0.11.0
run schedule marketplace-drift
expect "a stale published extension is drift on a scheduled run" 1 "::error::the published extension installs v0.11.0"

marketplace_body 0.11.0
run release marketplace-drift
expect "a stale published extension is drift on a post-release run" 1 "::error::the published extension installs v0.11.0"

marketplace_body 0.11.0
run workflow_dispatch marketplace-drift
expect "a stale published extension is drift on a manual run" 1 "::error::the published extension installs v0.11.0"

marketplace_body 0.11.0
run pull_request marketplace-drift
expect "drift is informational on a pull request, which cannot republish" 0 "::warning::the published extension installs v0.11.0"

marketplace_body 0.9.9
run pull_request marketplace-drift
expect "a published version with no tag here cannot be ruled clean" 0 "no vscode-v0.9.9 tag exists"

# The gallery failure modes. Each must reach the authored message and the
# event-aware routing, not die at a jq parse error under set -e.
FAKE_CURL_RC=22 run schedule marketplace-drift
expect "an HTTP error fails a scheduled run" 1 "could not determine the published extension version"
FAKE_CURL_RC=22 run schedule marketplace-drift
refute "an HTTP error does not surface as a raw jq error" "parse error"

FAKE_CURL_RC=28 run schedule marketplace-drift
expect "a timeout fails a scheduled run" 1 "marketplace request failed (curl exit 28)"

FAKE_CURL_RC=22 run pull_request marketplace-drift
expect "an unreachable gallery does not fail a pull request" 0 "::warning::could not determine the published extension version"
unset FAKE_CURL_RC

printf '<html><body>403 Forbidden</body></html>\n' > "$BODY"
run schedule marketplace-drift
expect "an HTML error page served with 200 is a failure, not no drift" 1 "could not determine the published extension version"
run schedule marketplace-drift
refute "an HTML error page does not surface as a raw jq error" "parse error"
run pull_request marketplace-drift
expect "an HTML error page only warns on a pull request" 0 "::warning::could not determine"

printf '{"results":[{"extensions":[]}]}\n' > "$BODY"
run schedule marketplace-drift
expect "valid JSON naming no extension is not read as no drift" 1 "could not determine the published extension version"

printf '{"results":[{"extensions":[{"versions":[{"version":"not-a-version"}]}]}]}\n' > "$BODY"
run schedule marketplace-drift
expect "an unusable version string is a failure" 1 "could not determine the published extension version"

marketplace_body 0.12.0
FAKE_GH_FAIL=1 run pull_request marketplace-drift
expect "drift cannot be judged without the release list, on any trigger" 1 "could not resolve the latest stable release"
unset FAKE_GH_FAIL

# --- vscode-publish.yml wiring ---------------------------------------------
#
# What the gate cannot see from the inside: whether publishing can happen on a
# ref the gates never ran on. The two version gates are steps in `build`,
# conditioned on vscode-v* tags; `release` is the job that actually publishes.
# Review found `release` on the wider `refs/tags/`, so a workflow_dispatch from
# a CLI tag with dry_run off would have skipped both gates and published. The
# three conditions must be one and the same predicate.

# job_if <job>: the `if:` of a top-level job, whitespace-normalised.
job_if() {
  awk -v job="$1" '
    $0 ~ "^  " job ":$" { injob = 1; next }
    injob && /^  [A-Za-z0-9_-]+:/ { exit }
    injob && /^    if:/ { sub(/^    if:[[:space:]]*/, ""); print; exit }
  ' "$PUBLISH_YML" | tr -s '[:space:]' ' ' | sed 's/ $//'
}

# step_if <step name>: the `if:` of the named step, whitespace-normalised.
step_if() {
  awk -v name="$1" '
    $0 ~ "^      - name: " name "$" { instep = 1; next }
    instep && /^      - / { exit }
    instep && /^        if:/ { sub(/^        if:[[:space:]]*/, ""); print; exit }
  ' "$PUBLISH_YML" | tr -s '[:space:]' ' ' | sed 's/ $//'
}

release_if="$(job_if release)"
pkg_if="$(step_if 'package.json version must match the tag')"
pin_if="$(step_if 'DOWNLOAD_VERSION must be the latest stable release')"

for pair in "release job:$release_if" "package.json gate:$pkg_if" "DOWNLOAD_VERSION gate:$pin_if"; do
  label="${pair%%:*}"; cond="${pair#*:}"
  if [ -z "$cond" ]; then
    fail "publish wiring: could not read the condition of the $label from $PUBLISH_YML"
  elif [[ "$cond" == *"refs/tags/vscode-v"* ]]; then
    ok "publish wiring: the $label only runs on vscode-v* tags"
  else
    fail "publish wiring: the $label runs on refs the version gates do not cover: $cond"
  fi
done
if [ -n "$release_if" ] && [ "$release_if" = "$pkg_if" ] && [ "$release_if" = "$pin_if" ]; then
  ok "publish wiring: publishing and both gates share one predicate, so neither can drift alone"
else
  fail "publish wiring: conditions differ. release: '$release_if' | package.json: '$pkg_if' | pin: '$pin_if'"
fi

# --- result ----------------------------------------------------------------

echo
echo "passed: $PASS  failed: $FAIL"
[ "$FAIL" -eq 0 ]
