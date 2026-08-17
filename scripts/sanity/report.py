#!/usr/bin/env python3
"""Result model and output formats for the sanity suite.

SKIP is a first-class outcome, not a quiet pass. A check that did not run must
never read as a check that succeeded: that is the specific way a sanity suite
stops being worth running.
"""

import json
import sys
import xml.etree.ElementTree as ET
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import Optional


class Outcome(str, Enum):
    PASS = "pass"
    FAIL = "fail"
    SKIP = "skip"


@dataclass
class Result:
    """One check's outcome."""

    name: str
    phase: str
    outcome: Outcome
    #: Why it failed, or why it was skipped. Empty on a pass.
    detail: str = ""
    #: Issue numbers this check guards, e.g. ["741"]. Printed so a failure names
    #: the regression it belongs to instead of only its own wording.
    issues: list[str] = field(default_factory=list)
    duration_s: float = 0.0

    @property
    def tag(self) -> str:
        return f" (#{', #'.join(self.issues)})" if self.issues else ""


class Report:
    def __init__(self, subject: str) -> None:
        #: What was tested: a tag, or a path to a locally built binary.
        self.subject = subject
        self.results: list[Result] = []

    def add(self, r: Result) -> Result:
        self.results.append(r)
        return r

    def count(self, outcome: Outcome) -> int:
        return sum(1 for r in self.results if r.outcome is outcome)

    @property
    def failed(self) -> list[Result]:
        return [r for r in self.results if r.outcome is Outcome.FAIL]

    @property
    def skipped(self) -> list[Result]:
        return [r for r in self.results if r.outcome is Outcome.SKIP]

    # ── console ──────────────────────────────────────────────────────────────

    def print_result(self, r: Result, color: bool = True) -> None:
        marks = {
            Outcome.PASS: ("PASS", "32"),
            Outcome.FAIL: ("FAIL", "31"),
            Outcome.SKIP: ("SKIP", "33"),
        }
        label, code = marks[r.outcome]
        stamp = f"\033[{code}m{label}\033[0m" if color else label
        print(f"  {stamp}  {r.name}{r.tag}")
        if r.detail:
            for line in r.detail.splitlines():
                print(f"        {line}")
        sys.stdout.flush()

    def print_summary(self, color: bool = True, strict_skip: bool = False) -> None:
        bold = (lambda s: f"\033[1m{s}\033[0m") if color else (lambda s: s)
        print()
        print(bold("== Summary =="))
        print(
            f"  {self.count(Outcome.PASS)} passed, "
            f"{self.count(Outcome.FAIL)} failed, "
            f"{self.count(Outcome.SKIP)} skipped"
        )
        if self.failed:
            print("\n  failures:")
            for r in self.failed:
                print(f"    - {r.name}{r.tag}")
        if self.skipped:
            # Always listed. A skip is a gap in coverage for this run, and the
            # reader needs to know which guarantees were not actually checked.
            print("\n  skipped (NOT verified by this run):")
            for r in self.skipped:
                print(f"    - {r.name}{r.tag}: {r.detail or 'no reason given'}")
        print()
        if self.failed:
            print(f"  {self.subject}: FAILED")
        elif self.skipped and strict_skip:
            print(f"  {self.subject}: FAILED (--strict-skip, and this run skipped checks)")
        else:
            print(f"  {self.subject}: looks sane")

    # ── machine-readable ─────────────────────────────────────────────────────

    def to_json(self, path: Path) -> None:
        payload = {
            "subject": self.subject,
            "totals": {o.value: self.count(o) for o in Outcome},
            "results": [
                {
                    "name": r.name,
                    "phase": r.phase,
                    "outcome": r.outcome.value,
                    "detail": r.detail,
                    "issues": r.issues,
                    "duration_s": round(r.duration_s, 3),
                }
                for r in self.results
            ],
        }
        path.write_text(json.dumps(payload, indent=2) + "\n")

    def to_junit(self, path: Path) -> None:
        """JUnit XML so CI can render this like any other test run."""
        suite = ET.Element(
            "testsuite",
            name="travsr-release-sanity",
            tests=str(len(self.results)),
            failures=str(self.count(Outcome.FAIL)),
            skipped=str(self.count(Outcome.SKIP)),
        )
        for r in self.results:
            case = ET.SubElement(
                suite,
                "testcase",
                classname=f"sanity.{r.phase}",
                name=r.name + r.tag,
                time=f"{r.duration_s:.3f}",
            )
            if r.outcome is Outcome.FAIL:
                ET.SubElement(case, "failure", message=r.detail or "check failed").text = r.detail
            elif r.outcome is Outcome.SKIP:
                ET.SubElement(case, "skipped", message=r.detail or "skipped")
        ET.ElementTree(suite).write(path, encoding="utf-8", xml_declaration=True)

    def exit_code(self, strict_skip: bool = False) -> int:
        if self.failed:
            return 1
        if strict_skip and self.skipped:
            return 1
        return 0
