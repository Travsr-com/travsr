#!/usr/bin/env bash
# Release sanity pass: verify a published release artifact actually works.
#
#   bash scripts/release-sanity.sh v1.0.0-rc.1
#   bash scripts/release-sanity.sh v1.0.0-rc.1 --keep    # keep the workdir
#
# Checks a *published* release, not the working tree, so it cannot run on a PR.
# Run it after a tag is cut and before promoting that tag to the next channel.
#
# Why this exists: the v1.0.0-beta.1 pass was done by hand and found four defects
# (#724 #726 #727 #728), three of which shipped in a tagged artifact. Doing it by
# hand again for rc.1 found a fifth (#741). Everything here is a check that
# already caught something real, so the cost of NOT running it is measured, not
# hypothetical.
#
# Deliberately not wired into CI: it needs a published release, and per the
# project's precedent for the language suite, manual first so anyone can run it
# on demand. Wiring it to fire on `release` completion is a one-line follow-up.
#
# Isolation: runs entirely under a temp dir with TRAVSR_DISABLE_REGISTRY=1, so it
# never touches the caller's own ~/.travsr registry or any real repo. It does
# read ~/.travsr/bin sidecars, which is intentional: that is what a user has.
set -uo pipefail

TAG="${1:-}"; shift || true
KEEP=""
WITH_CPP=""
for arg in "$@"; do
    case "$arg" in
        --keep)     KEEP=1 ;;
        --with-cpp) WITH_CPP=1 ;;
        *) echo "unknown flag: $arg" >&2; exit 2 ;;
    esac
done
if [[ -z "$TAG" ]]; then
    echo "usage: release-sanity.sh <tag> [--keep] [--with-cpp]" >&2
    echo "   e.g. release-sanity.sh v1.0.0-rc.1" >&2
    echo >&2
    echo "  --keep      keep the temp workdir for inspection" >&2
    echo "  --with-cpp  also exercise c/c++, which needs a per-corpus trust grant." >&2
    echo "              Off by default because that grant is written to the caller's" >&2
    echo "              real ~/.travsr/lang.toml, outside this script's temp dir." >&2
    exit 2
fi

REPO="${TRAVSR_REPO:-Travsr-com/travsr}"
WORK="$(mktemp -d)"
PASS=0
FAIL=0
declare -a FAILURES=()

cleanup() {
    if [[ -n "$KEEP" ]]; then
        echo "workdir kept: $WORK"
    else
        rm -rf "$WORK"
    fi
}
trap cleanup EXIT

# ── reporting ────────────────────────────────────────────────────────────────

ok()   { PASS=$((PASS + 1)); printf '  \033[32mPASS\033[0m  %s\n' "$1"; }
bad()  { FAIL=$((FAIL + 1)); FAILURES+=("$1"); printf '  \033[31mFAIL\033[0m  %s\n' "$1"; [[ -n "${2:-}" ]] && printf '        %s\n' "$2"; }
note() { printf '  \033[33mNOTE\033[0m  %s\n' "$1"; }
head1() { printf '\n\033[1m== %s ==\033[0m\n' "$1"; }

# `check <label> <expected-substring> <actual>`: the workhorse.
check() {
    local label="$1" want="$2" got="$3"
    if [[ "$got" == *"$want"* ]]; then
        ok "$label"
    else
        bad "$label" "wanted to see: $want${NL}got: $(printf '%s' "$got" | head -3 | tr '\n' '|')"
    fi
}
NL=$'\n'

# Portable SHA-256, same fallback as check-plugin-hashes.sh.
if command -v sha256sum &>/dev/null; then SHA256=sha256sum
elif command -v shasum &>/dev/null;   then SHA256="shasum -a 256"
else echo "ERROR: no sha256sum or shasum on PATH" >&2; exit 1
fi

# ── phase 1: artifacts ───────────────────────────────────────────────────────

head1 "Artifacts ($TAG)"
cd "$WORK" || exit 1
if ! gh release download "$TAG" --repo "$REPO" \
        --pattern '*.tar.gz' --pattern 'SHA256SUMS' >/dev/null 2>&1; then
    bad "download release $TAG" "gh release download failed; is the tag published?"
    echo; echo "aborting: nothing to check."; exit 1
fi
ok "downloaded release assets"

# Every artifact must match its recorded hash.
sums_bad=0
while read -r want file; do
    base="$(basename "$file")"
    [[ -f "$base" ]] || { sums_bad=1; continue; }
    got="$($SHA256 "$base" | awk '{print $1}')"
    [[ "$got" == "$want" ]] || sums_bad=1
done < SHA256SUMS
if [[ $sums_bad -eq 0 ]]; then ok "all artifacts match SHA256SUMS"
else bad "artifact checksums" "at least one artifact is missing or does not match"; fi

# Cosign bundles are published; verifying them needs cosign. Say so rather than
# silently skipping, because a signature nobody verifies buys nothing.
if command -v cosign &>/dev/null; then
    ok "cosign present (signature verification available)"
else
    note "cosign not installed: .bundle signatures NOT verified by this run"
fi

# Version identity must be the same in EVERY target. This is the #728 property,
# and the cross-built linux target is the one that can silently diverge, because
# it compiles in a container that does not inherit the host environment.
head1 "Version identity (#728)"
declare -a SEEN=()
for tgz in travsr-"$TAG"-*.tar.gz; do
    t="${tgz#travsr-$TAG-}"; t="${t%.tar.gz}"
    d="x_$t"; mkdir -p "$d"; tar xzf "$tgz" -C "$d" 2>/dev/null
    bin="$(find "$d" -maxdepth 2 -type f \( -name travsr -o -name travsr.exe \) | head -1)"
    if [[ -z "$bin" ]]; then bad "$t: binary not found in tarball"; continue; fi
    v="$(strings "$bin" 2>/dev/null | grep -oE '[0-9]+\.[0-9]+\.[0-9]+\+[0-9a-f]{7}' | sort -u | head -1)"
    if [[ -z "$v" ]]; then
        bad "$t: no build id compiled in" "expected <version>+<shortsha>; a bare version means the injection was lost for this target"
    else
        ok "$t -> $v"
        SEEN+=("$v")
    fi
done
uniq_count="$(printf '%s\n' "${SEEN[@]:-}" | sort -u | grep -c . || true)"
if [[ "$uniq_count" == "1" ]]; then
    ok "all targets report one identical build id"
else
    bad "targets disagree on build id" "$(printf '%s ' "${SEEN[@]:-}")"
fi

# Pick this host's binary for the functional phase.
case "$(uname -s)/$(uname -m)" in
    Darwin/arm64)  HOST=aarch64-apple-darwin ;;
    Darwin/*)      HOST=x86_64-apple-darwin ;;
    Linux/aarch64) HOST=aarch64-unknown-linux-gnu ;;
    Linux/*)       HOST=x86_64-unknown-linux-gnu ;;
    *)             echo "unsupported host for the functional phase" >&2; exit 1 ;;
esac
B="$WORK/x_$HOST/travsr"
[[ -x "$B" ]] || { bad "host binary $HOST not runnable"; exit 1; }
export TRAVSR_DISABLE_REGISTRY=1

check "travsr --version reports a build id" "+" "$("$B" --version 2>&1)"

# ── fixture ──────────────────────────────────────────────────────────────────

R="$WORK/fixture"
mkdir -p "$R/src"; cd "$R" || exit 1
git init -q && git config user.email sanity@travsr.test && git config user.name sanity
printf '.travsr/\n' > .gitignore

cat > src/payment.ts <<'EOF'
export interface Charge { amountCents: number; currency: string; }
export class Ledger {
  private rows: Charge[] = [];
  record(c: Charge): void { this.rows.push(c); }
}
export class PaymentService {
  constructor(private ledger: Ledger) {}
  charge(c: Charge): boolean {
    if (!validateCharge(c)) return false;
    this.ledger.record(c);
    return true;
  }
}
export function validateCharge(c: Charge): boolean {
  return c.amountCents > 0 && c.currency.length === 3;
}
EOF
cat > src/checkout.ts <<'EOF'
import { PaymentService, Ledger, Charge } from "./payment";
export function runCheckout(amount: number): boolean {
  const svc = new PaymentService(new Ledger());
  return svc.charge({ amountCents: amount, currency: "usd" });
}
EOF
# .mjs specifically: ES modules were the gap the language suite was written for.
cat > src/util.mjs <<'EOF'
export function computeTax(cents) { return Math.round(cents * 0.2); }
export function applyTax(cents) { return cents + computeTax(cents); }
EOF
cat > src/report.py <<'EOF'
class ReportBuilder:
    def __init__(self, rows):
        self.rows = rows
    def build_summary(self):
        return summarize_rows(self.rows)

def summarize_rows(rows):
    return {"count": len(rows), "total": sum(rows)}
EOF
cat > src/retry.rs <<'EOF'
pub struct RetryPolicy { pub max_attempts: u32 }
impl RetryPolicy {
    pub fn should_retry(&self, attempt: u32) -> bool { attempt < self.max_attempts }
}
pub fn run_with_retry(policy: &RetryPolicy) -> bool {
    let mut attempt = 0;
    while policy.should_retry(attempt) { attempt += 1; }
    true
}
EOF
cat > src/buffer.c <<'EOF'
#include <stddef.h>
size_t buffer_capacity(size_t used, size_t total) { return total - used; }
int buffer_has_room(size_t used, size_t total) { return buffer_capacity(used, total) > 0; }
EOF
cat > src/engine.cpp <<'EOF'
class Engine {
public:
  Engine(int seed) : seed_(seed) {}
  int next() { return advance(seed_); }
private:
  static int advance(int s) { return s * 1103515245 + 12345; }
  int seed_;
};
int run_engine(int seed) { Engine e(seed); return e.next(); }
EOF
git add -A && git commit -qm seed

# ── phase 2: pre-daemon behaviour ────────────────────────────────────────────

head1 "First-run behaviour (no daemon)"

out="$("$B" lang status 2>&1)"
check "lang status exists (#727)" "LANGUAGE" "$out"

"$B" init >/dev/null 2>&1
out="$("$B" status 2>&1)"
check "init produced an index" "nodes:" "$out"

# #726: with no daemon, Phase B is not scheduled. The message must not promise a
# time, and must name what actually builds the index.
out="$("$B" references validateCharge 2>&1)"
if [[ "$out" == *"pending"* ]]; then
    if [[ "$out" == *"~"*"minute"* ]]; then
        bad "pending message promises a false ETA (#726)" "$(printf '%s' "$out" | head -1)"
    else
        ok "pending message carries no ETA (#726)"
    fi
    check "pending message names the daemon (#726)" "travsr daemon start" "$out"
else
    note "Phase B already complete; #726 pending-path not exercised"
fi

# The remediation that message gives must actually work with no daemon running.
"$B" init --semantic >/dev/null 2>&1
out="$("$B" status 2>&1)"
check "init --semantic completes Phase B without a daemon (#726 advice is true)" \
      "semantic: complete" "$out"

# ── phase 3: per-language correctness ────────────────────────────────────────

head1 "Cross-language resolution"
# symbol:expected-definition-path
for pair in \
    "validateCharge:src/payment.ts" \
    "computeTax:src/util.mjs" \
    "summarize_rows:src/report.py" \
    "should_retry:src/retry.rs" \
; do
    sym="${pair%%:*}"; want="${pair##*:}"
    out="$("$B" references "$sym" 2>&1 | grep -v '^warning')"
    check "references $sym -> $want" "$want" "$out"
done

# C/C++ need a per-corpus trust grant and a compile_commands.json. Report what is
# missing rather than silently skipping, since "not run" reads like "passed".
if [[ -n "$WITH_CPP" ]]; then
    # scip-clang needs a compilation database. The SDK path matters on macOS:
    # without -isysroot the sidecar cannot resolve system headers, and the
    # resulting miss looks exactly like an upstream analyzer bug. That
    # misdiagnosis cost real time during the language suite work, so the
    # fixture supplies the flag rather than leaving it to chance.
    SDK="$(xcrun --show-sdk-path 2>/dev/null || true)"
    SYSROOT=""; [[ -n "$SDK" ]] && SYSROOT="-isysroot $SDK"
    cat > compile_commands.json <<EOF
[
  {"directory":"$R","file":"$R/src/buffer.c","command":"clang $SYSROOT -c src/buffer.c -o /dev/null"},
  {"directory":"$R","file":"$R/src/engine.cpp","command":"clang++ -std=c++17 $SYSROOT -c src/engine.cpp -o /dev/null"}
]
EOF
    git add -A && git commit -qm "compdb"
    # The hint arrives inside backticks ("Run `travsr lang add cpp --corpus X`"),
    # so strip them; writing a corpus name with a stray backtick would put a
    # permanently unmatchable entry in the caller's real lang.toml.
    corpus="$("$B" status 2>&1 | grep -oE -- '--corpus [^ `]+' | head -1 | awk '{print $2}' | tr -d '`')"
    if [[ -n "$corpus" ]]; then
        "$B" lang add cpp --corpus "$corpus" >/dev/null 2>&1
        "$B" lang add c   --corpus "$corpus" >/dev/null 2>&1
        note "granted c/c++ trust for corpus $corpus in ~/.travsr/lang.toml (persists after this run)"
    fi
    "$B" init --semantic --force >/dev/null 2>&1
    for pair in "buffer_capacity:src/buffer.c" "advance:src/engine.cpp"; do
        sym="${pair%%:*}"; want="${pair##*:}"
        check "references $sym -> $want" "$want" "$("$B" references "$sym" 2>&1 | grep -v '^warning')"
    done
else
    note "c/c++ not exercised: needs --with-cpp (writes a trust grant outside the temp dir)"
fi

head1 "Graph traversal"
out="$("$B" graph PaymentService --direction both 2>&1 | grep -v '^warning')"
check "graph finds the cross-file caller" "runCheckout" "$out"

out="$("$B" fsck 2>&1)"
check "fsck reports no ghost nodes" "no ghost nodes" "$out"

# ── phase 4: status honesty ──────────────────────────────────────────────────

head1 "Status honesty"

# #724: native Phase B emits edges without SCIP definition nodes. Warning that a
# working language "produced no symbols" sends users to debug a sidecar that does
# not exist for it.
out="$("$B" status 2>&1)"
if [[ "$out" == *"produced no symbols"* ]]; then
    bad "false 'produced no symbols' warning (#724)" "$(printf '%s' "$out" | grep 'produced no symbols' | head -2)"
else
    ok "no false 'produced no symbols' warning (#724)"
fi

# #741: the marker must return to `complete` after a source commit + reindex, and
# the remediation it names must be the one that works.
printf '\nexport function applyDiscount(c: number): number { return c - 1; }\n' >> src/payment.ts
git add -A && git commit -qm "edit a source file"
"$B" init --semantic >/dev/null 2>&1
out="$("$B" status 2>&1)"
if [[ "$out" == *"semantic: complete"* ]]; then
    ok "marker returns to complete after a source commit (#741)"
else
    bad "marker stuck after a source commit (#741)" \
        "$(printf '%s' "$out" | head -1 | grep -o 'semantic: [a-z ()]*')"
fi

# The data must be fresh regardless, which is what separates a marker bug from
# real staleness. Worth asserting separately so a future regression in EITHER is
# attributable.
out="$("$B" references applyDiscount 2>&1 | grep -v '^warning')"
check "newly committed symbol is indexed" "src/payment.ts" "$out"

# ── summary ──────────────────────────────────────────────────────────────────

head1 "Summary"
printf '  %d passed, %d failed\n' "$PASS" "$FAIL"
if [[ $FAIL -gt 0 ]]; then
    printf '\n  failures:\n'
    for f in "${FAILURES[@]}"; do printf '    - %s\n' "$f"; done
    printf '\n  Re-run with --keep to inspect the workdir.\n'
    exit 1
fi
printf '\n  %s looks sane.\n' "$TAG"
