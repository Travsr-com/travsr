#!/usr/bin/env python3
"""Tests for the harness itself.

    python3 travsrAutomation/selftest.py

Needs no travsr binary and no network, so unlike the suite it guards, this can
run on every pull request.

It exists because the harness has already had two real bugs, both found by
running it rather than by reading it:

  1. The corpus name was scraped from travsr's remediation line and kept a
     trailing backtick, which was then written into the caller's real
     ~/.travsr/lang.toml as a permanently unmatchable entry. A tool that
     corrupts the machine running it is the worst kind of bug for a checker.
  2. That scrape assumed the trust hint was present. The grant is global and
     persists, so on any machine that had already run the suite there is no
     hint, and every c/c++ check silently skipped. It degraded on exactly the
     machines that had run it most.

Both are pinned below. A harness whose failures are silent is worse than no
harness, so the parts that decide PASS/FAIL/SKIP are tested directly.
"""

import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent / "lib"))

from checks import _corpus_from_hint  # noqa: E402
from report import Outcome, Report, Result  # noqa: E402
from travsr import BUILD_ID_RE, Run, build_id_in_binary  # noqa: E402


class CorpusHintParsing(unittest.TestCase):
    """Bug 1: a corpus name is written to the caller's real config file."""

    def test_strips_the_backticks_travsr_wraps_the_hint_in(self):
        status = (
            "warning: 'cpp' is registered but this repository's corpus is not "
            "trusted for semantic indexing. Run `travsr lang add cpp --corpus "
            "local/fixture` to trust it"
        )
        self.assertEqual(_corpus_from_hint(status), "local/fixture")

    def test_no_hint_yields_empty_rather_than_a_wrong_guess(self):
        # Bug 2: this is the state on a machine where trust was already granted.
        # Returning "" makes the caller skip explicitly instead of granting trust
        # for a corpus name it invented.
        self.assertEqual(_corpus_from_hint("nodes: 3 | edges: 3 | semantic: complete"), "")

    def test_trailing_word_is_not_absorbed(self):
        status = "Run `travsr lang add c --corpus my/corpus` to trust it"
        self.assertEqual(_corpus_from_hint(status), "my/corpus")

    def test_dangling_corpus_flag_yields_empty(self):
        self.assertEqual(_corpus_from_hint("... --corpus"), "")


class BuildIdExtraction(unittest.TestCase):
    """#728: a release binary must carry <version>+<shortsha>."""

    def test_matches_a_release_build_id(self):
        self.assertEqual(
            BUILD_ID_RE.findall(b"junk\x00travsr 1.0.0+8b9af8f\x00more"),
            [b"1.0.0+8b9af8f"],
        )

    def test_does_not_match_a_bare_version(self):
        # The exact shape of the bug: a bare version is what every channel
        # reported before the fix, and must not be accepted as a build id.
        self.assertEqual(BUILD_ID_RE.findall(b"travsr 1.0.0"), [])

    def test_does_not_match_a_prerelease_suffix(self):
        # Baking the channel in would follow a promoted artifact into stable and
        # misreport it, so this shape must never be produced or accepted.
        self.assertEqual(BUILD_ID_RE.findall(b"1.0.0-beta.1"), [])

    def test_multi_component_versions(self):
        self.assertEqual(BUILD_ID_RE.findall(b"10.20.30+abcdef0"), [b"10.20.30+abcdef0"])

    def test_reads_a_binary_without_executing_it(self):
        # This is what lets one host verify all five targets, including the
        # cross-built Linux one that cannot run here.
        with tempfile.TemporaryDirectory() as d:
            f = Path(d) / "fake-binary"
            f.write_bytes(b"\x7fELF\x00\x00padding 2.3.4+deadbee trailing\x00")
            self.assertEqual(build_id_in_binary(f), "2.3.4+deadbee")

    def test_absent_build_id_is_none_not_an_exception(self):
        with tempfile.TemporaryDirectory() as d:
            f = Path(d) / "fake-binary"
            f.write_bytes(b"no version here")
            self.assertIsNone(build_id_in_binary(f))


class WarningStripping(unittest.TestCase):
    """`Run.answer` must strip warnings; `Run.out` must keep them."""

    def _run(self) -> Run:
        return Run(
            argv=["references", "mul"],
            code=0,
            stdout="resolved: fn:mul (function) - src/m.py:7\n1 reference(s):\nsrc/m.py:11\n",
            stderr="warning: [note: call-graph edges degraded - not authoritative]\n",
        )

    def test_answer_drops_warning_lines(self):
        # Without this, a staleness banner mentioning a path would satisfy an
        # assertion about where a symbol resolved, and #741's false warning would
        # have made unrelated checks pass for the wrong reason.
        self.assertNotIn("warning:", self._run().answer)
        self.assertIn("src/m.py:11", self._run().answer)

    def test_out_keeps_warnings_because_some_checks_assert_on_them(self):
        self.assertIn("not authoritative", self._run().out)


class ReportBehaviour(unittest.TestCase):
    def _report(self, *outcomes: Outcome) -> Report:
        r = Report("test-subject")
        for i, o in enumerate(outcomes):
            r.add(Result(name=f"check {i}", phase="p", outcome=o, detail="because"))
        return r

    def test_all_pass_exits_zero(self):
        self.assertEqual(self._report(Outcome.PASS, Outcome.PASS).exit_code(), 0)

    def test_any_failure_exits_one(self):
        self.assertEqual(self._report(Outcome.PASS, Outcome.FAIL).exit_code(), 1)

    def test_a_skip_alone_exits_zero_by_default(self):
        self.assertEqual(self._report(Outcome.PASS, Outcome.SKIP).exit_code(), 0)

    def test_strict_skip_turns_a_skip_into_a_failure(self):
        # The CI shape. A silently unverified guarantee is the thing this whole
        # harness exists to prevent, so CI must be able to refuse skips.
        self.assertEqual(
            self._report(Outcome.PASS, Outcome.SKIP).exit_code(strict_skip=True), 1
        )

    def test_skips_are_recorded_not_folded_into_passes(self):
        r = self._report(Outcome.PASS, Outcome.SKIP, Outcome.SKIP)
        self.assertEqual(r.count(Outcome.PASS), 1)
        self.assertEqual(len(r.skipped), 2)

    def test_issue_tag_is_rendered_for_attribution(self):
        r = Result(name="x", phase="p", outcome=Outcome.FAIL, issues=["741", "724"])
        self.assertEqual(r.tag, " (#741, #724)")

    def test_no_issues_means_no_tag(self):
        self.assertEqual(Result(name="x", phase="p", outcome=Outcome.PASS).tag, "")

    def test_json_and_junit_are_written_and_parseable(self):
        import json
        import xml.etree.ElementTree as ET

        r = self._report(Outcome.PASS, Outcome.FAIL, Outcome.SKIP)
        with tempfile.TemporaryDirectory() as d:
            j, x = Path(d) / "r.json", Path(d) / "r.xml"
            r.to_json(j)
            r.to_junit(x)
            payload = json.loads(j.read_text())
            self.assertEqual(payload["totals"]["fail"], 1)
            self.assertEqual(payload["totals"]["skip"], 1)
            suite = ET.parse(x).getroot()
            self.assertEqual(suite.get("failures"), "1")
            self.assertEqual(suite.get("skipped"), "1")


class CheckRegistry(unittest.TestCase):
    def test_every_check_has_a_known_phase(self):
        from checks import all_checks

        import run as runner  # the entry point owns the phase list

        for c in all_checks():
            self.assertIn(c.phase, runner.PHASES, f"{c.name} has unknown phase {c.phase}")

    def test_check_names_are_unique(self):
        from checks import all_checks

        names = [c.name for c in all_checks()]
        self.assertEqual(len(names), len(set(names)), "duplicate check names")

    def test_the_regressions_that_shipped_are_all_guarded(self):
        # Each of these reached a tagged artifact. If a check for one is ever
        # deleted, this fails rather than quietly reducing coverage.
        from checks import all_checks

        guarded = {i for c in all_checks() for i in c.issues}
        for issue in ("724", "726", "727", "728", "741"):
            self.assertIn(issue, guarded, f"#{issue} is no longer guarded by any check")


if __name__ == "__main__":
    unittest.main(verbosity=2)
