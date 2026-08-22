#!/usr/bin/env python3
"""Render a sanity run's JSON as a GitHub step summary (Markdown on stdout).

    python3 travsrAutomation/summarise.py sanity.json ubuntu-latest >> "$GITHUB_STEP_SUMMARY"

A separate script rather than a heredoc inside the workflow: the previous version
interpolated `${{ matrix.os }}` into embedded Python source, so a runner label
containing a quote or a backslash would have produced a syntax error inside the
YAML, at the exact moment someone is trying to read why a release check failed.
Taking it as argv removes that whole class of problem and makes this testable.

Prints failures and skips, never a bare total. A skipped check is a guarantee this
run did not verify, and folding it into a pass is the failure mode the whole suite
exists to avoid.
"""

import json
import sys
from pathlib import Path


def render(payload: dict, label: str) -> str:
    totals = payload.get("totals", {})
    out: list[str] = []
    subject = payload.get("subject", "unknown")
    out.append(f"### {subject} on {label}")
    out.append("")
    out.append(
        f"{totals.get('pass', 0)} passed, "
        f"{totals.get('fail', 0)} failed, "
        f"{totals.get('skip', 0)} skipped"
    )

    results = payload.get("results", [])
    for heading, key in (("Failures", "fail"), ("Skipped, so NOT verified by this run", "skip")):
        rows = [r for r in results if r.get("outcome") == key]
        if not rows:
            continue
        out.append("")
        out.append(f"**{heading}**")
        out.append("")
        for r in rows:
            issues = r.get("issues") or []
            tag = " (#" + ", #".join(issues) + ")" if issues else ""
            detail = (r.get("detail") or "").strip().splitlines()
            first = f": {detail[0]}" if detail else ""
            out.append(f"- `{r.get('phase', '?')}` {r.get('name', '?')}{tag}{first}")
    out.append("")
    return "\n".join(out)


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("usage: summarise.py <results.json> <label>", file=sys.stderr)
        return 2
    path, label = Path(argv[0]), argv[1]
    if not path.is_file():
        # Not an error: the caller already handles a missing file, and a summary
        # step must never be the thing that fails a run.
        print(f"### Sanity produced no results on {label}")
        return 0
    try:
        payload = json.loads(path.read_text())
    except json.JSONDecodeError as e:
        print(f"### Sanity results on {label} were unreadable\n\n`{e}`")
        return 0
    print(render(payload, label))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
