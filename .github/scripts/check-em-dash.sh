#!/usr/bin/env bash
# CI gate: no em-dash or en-dash in user-facing strings.
#
# CLAUDE.md forbids em-dashes (and en-dashes) in anything a user reads. PR #748
# swept 474 of them out of error messages, help text, log lines and printed
# output. This keeps them out: it fails if a dash comes back in a string an
# agent or user sees.
#
# What it checks: Rust and TypeScript source, excluding test paths (fixtures
# there legitimately reproduce the node-header wire format). It matches BOTH the
# literal characters and their escape spellings, because the sweep's own blind
# spot was that a search for the literal character never saw `\u{2014}`:
#
#     em-dash  U+2014  ->  literal  or  \u{2014}
#     en-dash  U+2013  ->  literal  or  \u{2013}
#
# Comments are out of scope (the PR deliberately left ~2750 in `//` lines), so a
# dash after `//` on a line does not count; a dash in the code/string part does.
#
# Allowlist (the only places a dash is legitimate, all documented at their
# definition):
#   - the node-header wire format `<sig> (<kind>) \u{2014} <path>`, printed by
#     many tools and parsed back by splitting on that exact separator;
#   - the git-hook marker line `installed by travsr \u{2014} do not edit`,
#     matched byte-for-byte to recognise travsr's own hooks;
#   - single-glyph placeholders for an empty cell / unset value (`"\u{2014}"`);
#   - the em-dash guard assertions in observability.rs, which name the character.
#
# To allow a new dash, prefer rewording. If a keep is genuinely required, add a
# pattern below with a comment saying why, next to the ones already here.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

# Files in scope: tracked .rs and .ts, minus test paths, examples and benches.
mapfile -t FILES < <(
  git ls-files '*.rs' '*.ts' \
    | grep -vE '(^|/)tests?/|/test/|\.test\.ts$|_test\.rs$|/examples/|/benches/' \
    || true
)

fail=0

for f in "${FILES[@]}"; do
  [ -f "$f" ] || continue
  # Per-line: drop the comment tail, then look for a dash in what remains. A
  # line whose dash survives comment-stripping and is not allowlisted fails.
  # -CSD: decode input and encode output as UTF-8, so a literal U+2014 byte
  # sequence matches \x{2014} and is not invisible to the check.
  perl -CSD -ne '
    my $line = $_;
    # Strip inline // and /* comments (errs toward missing a dash inside a
    # string that contains "//", never toward a false positive).
    my $code = $line;
    $code =~ s{//.*$}{};
    $code =~ s{/\*.*$}{};
    # Pure block-comment continuation lines.
    next if $code =~ /^\s*\*/;
    # Dash in the code/string part? (literal or escaped, em or en)
    next unless $code =~ /\x{2014}|\x{2013}|\\u\{2014\}|\\u\{2013\}/;

    # --- Allowlist (see header) --------------------------------------------
    # node-header wire format: "<sig> (<kind>) — <path>" and its parsers.
    next if $code =~ /\)\s(?:\x{2014}|\\u\{2014\})\s/;      # printers
    next if $code =~ /"\s(?:\x{2014}|\\u\{2014\})\s"/;      # Rust parser split
    next if $code =~ /(?:indexOf|includes)\("\s\x{2014}\s"/; # TS parser split
    next if $code =~ /\\s\+\x{2014}\\s\+/;                  # TS parser regex
    next if $code =~ /\[[^\]]*\x{2014}/;                    # TS char class [-—,;.]
    # git-hook marker, matched byte-for-byte to detect travsr-installed hooks.
    next if $code =~ /installed by travsr/;
    # single-glyph placeholders (empty cell / unset value) and the bare dash
    # named as a banned token in the no-dash guard tests. A lone dash in quotes
    # is never prose.
    next if $code =~ /(?:"|'"'"'|>)[\x{2013}\x{2014}](?:"|'"'"'|<)/;
    # the em-dash guard assertions, which reference the character on purpose.
    next if $code =~ /contains\(.?\\u\{2014\}/;

    print "$ARGV:$.: $line";
    $found = 1;
    END { exit 1 if $found }
  ' "$f" || fail=1
done

if [ "$fail" -ne 0 ]; then
  echo ""
  echo "check-em-dash: em-dash or en-dash found in a user-facing string (above)."
  echo "CLAUDE.md forbids them. Reword the string, or if the dash is a genuine"
  echo "wire-format/marker keep, add an allowlist pattern in this script."
  exit 1
fi

echo "check-em-dash: OK (no em/en-dash in user-facing strings)"
