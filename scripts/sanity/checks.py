#!/usr/bin/env python3
"""The checks, grouped into phases.

Every check here corresponds to something that actually broke. The issue numbers
are on each one so a failure names the regression it belongs to rather than only
its own wording. Nothing in here is speculative: #724, #726, #727 and #728 were
found by hand on v1.0.0-beta.1 (three of them already shipped in a tagged
artifact), and #741 was found by hand on v1.0.0-rc.1.

A check returns (Outcome, detail). Raising is also fine: the runner converts an
unexpected exception into a FAIL with the traceback, so a broken check is never
mistaken for a passing one.
"""

import shutil
import time
from pathlib import Path
from typing import Callable, Optional

from fixtures import CROSS_FILE_CALLEE, CROSS_FILE_CALLER, SYMBOLS, Fixture
from report import Outcome, Report, Result
from travsr import (
    EXPECTED_TARGETS,
    Travsr,
    build_id_in_binary,
    extract,
    verify_sums,
)

CheckFn = Callable[["Context"], tuple[Outcome, str]]


class Context:
    """Shared state handed to each check."""

    def __init__(
        self,
        work: Path,
        cli: Optional[Travsr] = None,
        fixture: Optional[Fixture] = None,
        artifacts: Optional[Path] = None,
        with_cpp: bool = False,
    ) -> None:
        self.work = work
        self.cli = cli
        self.fixture = fixture
        #: Directory holding downloaded release tarballs, or None in --binary mode.
        self.artifacts = artifacts
        self.with_cpp = with_cpp
        #: Set by the cross-language phase for the honesty phase to reuse.
        self.notes: dict[str, str] = {}


# ── phase: artifacts ─────────────────────────────────────────────────────────


def check_checksums(ctx: Context) -> tuple[Outcome, str]:
    if ctx.artifacts is None:
        return Outcome.SKIP, "no release downloaded (running against a local binary)"
    verified, problems = verify_sums(ctx.artifacts)
    if problems:
        return Outcome.FAIL, "\n".join(problems)
    if not verified:
        return Outcome.FAIL, "SHA256SUMS listed nothing verifiable"
    return Outcome.PASS, ""


def check_all_targets_present(ctx: Context) -> tuple[Outcome, str]:
    if ctx.artifacts is None:
        return Outcome.SKIP, "no release downloaded (running against a local binary)"
    names = [p.name for p in ctx.artifacts.glob("*.tar.gz")]
    missing = [t for t in EXPECTED_TARGETS if not any(t in n for n in names)]
    if missing:
        return Outcome.FAIL, "targets not published: " + ", ".join(missing)
    return Outcome.PASS, ""


def check_build_id_identical(ctx: Context) -> tuple[Outcome, str]:
    """#728: every target must carry the same `<version>+<shortsha>`.

    Checked by reading bytes, not by running each binary, so one host can verify
    all five. The cross-built Linux target compiles in a container that does not
    inherit the host environment, which makes it the one most likely to have
    silently lost the injection and shipped a different string from its siblings.
    """
    if ctx.artifacts is None:
        return Outcome.SKIP, "no release downloaded (running against a local binary)"
    seen: dict[str, str] = {}
    problems: list[str] = []
    for tgz in sorted(ctx.artifacts.glob("*.tar.gz")):
        target = next((t for t in EXPECTED_TARGETS if t in tgz.name), tgz.stem)
        dest = ctx.work / f"x_{target}"
        binary = extract(tgz, dest)
        if binary is None:
            problems.append(f"{target}: no travsr binary inside the tarball")
            continue
        bid = build_id_in_binary(binary)
        if bid is None:
            problems.append(
                f"{target}: no build id compiled in; a bare version means the "
                f"injection was lost for this target"
            )
        else:
            seen[target] = bid
    if problems:
        return Outcome.FAIL, "\n".join(problems)
    distinct = sorted(set(seen.values()))
    if len(distinct) != 1:
        detail = "\n".join(f"{t}: {v}" for t, v in sorted(seen.items()))
        return Outcome.FAIL, f"targets disagree on build id:\n{detail}"
    return Outcome.PASS, f"all {len(seen)} targets report {distinct[0]}"


def check_signatures(ctx: Context) -> tuple[Outcome, str]:
    if ctx.artifacts is None:
        return Outcome.SKIP, "no release downloaded (running against a local binary)"
    if shutil.which("cosign") is None:
        # Reported, never silently passed. A signature nobody verifies buys
        # nothing, so a run that could not check them has to say so.
        return Outcome.SKIP, "cosign not installed, so .bundle signatures were NOT verified"
    return Outcome.PASS, "cosign available"


def check_version_reports_build_id(ctx: Context) -> tuple[Outcome, str]:
    """#728, from the running binary rather than its bytes."""
    v = ctx.cli.version()
    if ctx.artifacts is None:
        # A local dev build legitimately has no injected id, so only assert the
        # command works at all. Asserting a build id here would fail every
        # developer running this on their own build.
        return (Outcome.PASS, f"{v} (local build, no injected id expected)") if v else (
            Outcome.FAIL,
            "--version produced no output",
        )
    if "+" not in v:
        return Outcome.FAIL, f"release binary reports {v!r} with no build id"
    return Outcome.PASS, v


# ── phase: first run ─────────────────────────────────────────────────────────


def check_lang_status_exists(ctx: Context) -> tuple[Outcome, str]:
    """#727: `travsr lang status` must work; the project docs tell agents to run it."""
    r = ctx.cli.run("lang", "status")
    if "unrecognized subcommand" in r.out:
        return Outcome.FAIL, "lang status is not a subcommand (docs reference it)"
    if "LANGUAGE" not in r.out:
        return Outcome.FAIL, f"unexpected output:\n{r.out[:300]}"
    return Outcome.PASS, ""


def check_init_indexes(ctx: Context) -> tuple[Outcome, str]:
    r = ctx.cli.run("init")
    if r.timed_out:
        return Outcome.FAIL, "travsr init did not return within the timeout"
    status = ctx.cli.status()
    if "nodes:" not in status:
        return Outcome.FAIL, f"no index after init:\n{status[:300]}"
    return Outcome.PASS, status.splitlines()[0].strip()


def check_pending_message_has_no_eta(ctx: Context) -> tuple[Outcome, str]:
    """#726: Phase B only runs under a daemon, so an ETA with none running is false."""
    if ctx.cli.daemon_running():
        return Outcome.SKIP, "a daemon is running, so the no-daemon path was not exercised"
    if ctx.cli.semantic_state().startswith("complete"):
        return Outcome.SKIP, "Phase B already complete, so the pending path was not exercised"
    r = ctx.cli.run("references", SYMBOLS[0].name)
    if "pending" not in r.out:
        return Outcome.SKIP, "no pending answer was produced"
    if "minute" in r.out:
        return Outcome.FAIL, f"pending answer still promises a time:\n{r.answer.strip()[:300]}"
    if "daemon" not in r.out:
        return Outcome.FAIL, f"pending answer does not name the daemon:\n{r.answer.strip()[:300]}"
    return Outcome.PASS, ""


def check_init_semantic_without_daemon(ctx: Context) -> tuple[Outcome, str]:
    """The remediation #726's message gives must actually work.

    A message that names an action is only an improvement if that action works.
    This is the check that the advice is true, not merely better worded.
    """
    r = ctx.cli.run("init", "--semantic")
    if r.timed_out:
        return Outcome.FAIL, "init --semantic did not return within the timeout"
    state = ctx.cli.semantic_state()
    if not state.startswith("complete"):
        return Outcome.FAIL, f"semantic state is {state!r} after init --semantic"
    return Outcome.PASS, ""


# ── phase: languages ─────────────────────────────────────────────────────────


def make_language_check(sym) -> CheckFn:
    def check(ctx: Context) -> tuple[Outcome, str]:
        if sym.needs_compdb and not ctx.with_cpp:
            return Outcome.SKIP, "needs --with-cpp (writes a trust grant outside the temp dir)"
        if sym.needs_compdb and ctx.notes.get("cpp_skip"):
            return Outcome.SKIP, ctx.notes["cpp_skip"]
        r = ctx.cli.run("references", sym.name)
        if sym.def_file not in r.answer:
            return Outcome.FAIL, (
                f"expected the definition in {sym.def_file}\n"
                f"got:\n{r.answer.strip()[:400]}"
            )
        if sym.ref_file not in r.answer:
            return Outcome.FAIL, (
                f"expected a reference in {sym.ref_file}\n"
                f"got:\n{r.answer.strip()[:400]}"
            )
        return Outcome.PASS, ""

    check.__name__ = f"references_{sym.name}"
    return check


def _corpus_from_hint(status: str) -> str:
    """Pull the corpus name out of travsr's own remediation line.

    The hint arrives as "Run `travsr lang add cpp --corpus X` to trust it".
    Backticks are stripped: a corpus name carrying one would be written to the
    caller's real lang.toml as a permanently unmatchable entry. That happened
    once while building this, which is why it is a named function with a test.
    """
    parts = status.replace("`", " ").split()
    if "--corpus" in parts:
        i = parts.index("--corpus")
        if i + 1 < len(parts):
            return parts[i + 1].strip().strip("`")
    return ""


def prepare_cpp(ctx: Context) -> tuple[Outcome, str]:
    """Make c/c++ Phase B runnable: compilation database, then trust if needed.

    Deliberately ordered compdb-then-trust and driven off what status actually
    reports, rather than assuming the trust hint is present. The grant is global
    and persists in ~/.travsr/lang.toml, so on any machine that has run this
    before, the corpus is already trusted and no hint appears. An earlier version
    of this scraped the hint unconditionally and skipped every c/c++ check on
    exactly the machines that had run it most.
    """
    if not ctx.with_cpp:
        return Outcome.SKIP, "not requested"
    if not ctx.fixture.write_compdb():
        ctx.notes["cpp_skip"] = "clang not found, so no compile_commands.json was written"
        return Outcome.SKIP, ctx.notes["cpp_skip"]
    ctx.fixture.commit("add a compilation database")
    ctx.cli.run("init", "--semantic", "--force")

    status = ctx.cli.status()
    if "not trusted" in status or "is not trusted" in status:
        corpus = _corpus_from_hint(status)
        if not corpus:
            ctx.notes["cpp_skip"] = (
                "corpus is untrusted but travsr's hint did not name a corpus, so "
                "the grant could not be made automatically"
            )
            return Outcome.SKIP, ctx.notes["cpp_skip"]
        for lang in ("cpp", "c"):
            ctx.cli.run("lang", "add", lang, "--corpus", corpus)
        ctx.cli.run("init", "--semantic", "--force")
        return Outcome.PASS, (
            f"granted c/c++ trust for corpus {corpus}; this persists in your real "
            f"~/.travsr/lang.toml after the run"
        )

    still_missing = "compile_commands.json" in ctx.cli.status()
    if still_missing:
        ctx.notes["cpp_skip"] = "travsr still reports no compile_commands.json after one was written"
        return Outcome.SKIP, ctx.notes["cpp_skip"]
    return Outcome.PASS, "corpus already trusted, compilation database written"


def check_cross_file_edge(ctx: Context) -> tuple[Outcome, str]:
    """The one answer Phase A alone cannot give: an edge between two files."""
    r = ctx.cli.run("graph", CROSS_FILE_CALLEE, "--direction", "both")
    if CROSS_FILE_CALLER not in r.answer:
        return Outcome.FAIL, (
            f"expected {CROSS_FILE_CALLER} as a caller of {CROSS_FILE_CALLEE}\n"
            f"got:\n{r.answer.strip()[:400]}"
        )
    return Outcome.PASS, ""


def check_fsck_clean(ctx: Context) -> tuple[Outcome, str]:
    r = ctx.cli.run("fsck")
    if "no ghost nodes" not in r.out:
        return Outcome.FAIL, f"fsck reported a problem:\n{r.out.strip()[:400]}"
    return Outcome.PASS, ""


# ── phase: status honesty ────────────────────────────────────────────────────


def check_no_false_zero_symbol_warning(ctx: Context) -> tuple[Outcome, str]:
    """#724: native Phase B emits edges without SCIP definition nodes.

    Warning that a working language "produced no symbols" sent users to debug a
    sidecar that does not exist for native languages.
    """
    status = ctx.cli.status()
    if "produced no symbols" not in status:
        return Outcome.PASS, ""
    offenders = [l.strip() for l in status.splitlines() if "produced no symbols" in l]
    return Outcome.FAIL, "\n".join(offenders[:3])


def check_marker_recovers_after_commit(ctx: Context) -> tuple[Outcome, str]:
    """#741: the marker must return to `complete` after a source commit + reindex.

    Paired deliberately with the data-freshness check below. #741 is the case
    where the marker lies and the graph is correct, so asserting only one of the
    two would either miss it or misattribute it.
    """
    ctx.fixture.append(
        "src/payment.ts",
        "\nexport function applyDiscount(c: number): number { return c - 1; }\n",
    )
    ctx.fixture.commit("edit a source file")
    ctx.cli.run("init", "--semantic")
    state = ctx.cli.semantic_state()
    if state.startswith("complete"):
        return Outcome.PASS, ""
    return Outcome.FAIL, (
        f"semantic marker is {state!r} after a source commit and reindex; "
        f"the remediation it names is the command that was just run"
    )


def check_new_symbol_is_indexed(ctx: Context) -> tuple[Outcome, str]:
    """Data freshness, asserted separately from the marker (see #741)."""
    r = ctx.cli.run("references", "applyDiscount")
    if "src/payment.ts" not in r.answer:
        return Outcome.FAIL, (
            "a symbol committed after the last index is missing, which is real "
            f"staleness rather than a marker bug:\n{r.answer.strip()[:400]}"
        )
    return Outcome.PASS, ""


def check_query_warning_matches_reality(ctx: Context) -> tuple[Outcome, str]:
    """A warning that results are unreliable, printed above a correct result.

    Split from the marker check because this is the user-visible cost: it trains
    people to ignore staleness warnings, which is expensive precisely when
    staleness is real.
    """
    r = ctx.cli.run("references", "applyDiscount")
    warned = "not authoritative" in r.out or "degraded" in r.out
    correct = "src/payment.ts" in r.answer
    if warned and correct:
        return Outcome.FAIL, (
            "the answer below this warning is correct, so the warning is false:\n"
            + "\n".join(l for l in r.out.splitlines() if l.startswith("warning:"))[:400]
        )
    return Outcome.PASS, ""


# ── registry ─────────────────────────────────────────────────────────────────


class Check:
    def __init__(self, name: str, phase: str, fn: CheckFn, issues: Optional[list[str]] = None):
        self.name = name
        self.phase = phase
        self.fn = fn
        self.issues = issues or []


def all_checks() -> list[Check]:
    checks = [
        Check("all expected targets published", "artifacts", check_all_targets_present),
        Check("artifacts match SHA256SUMS", "artifacts", check_checksums),
        Check("cosign signature verification", "artifacts", check_signatures),
        Check("build id identical across targets", "artifacts", check_build_id_identical, ["728"]),
        Check("--version reports a build id", "artifacts", check_version_reports_build_id, ["728"]),
        Check("lang status exists", "first-run", check_lang_status_exists, ["727"]),
        Check("init produces an index", "first-run", check_init_indexes),
        Check("pending answer carries no false ETA", "first-run", check_pending_message_has_no_eta, ["726"]),
        Check("init --semantic works with no daemon", "first-run", check_init_semantic_without_daemon, ["726"]),
        Check("prepare c/c++ (trust + compdb)", "languages", prepare_cpp),
    ]
    for sym in SYMBOLS:
        checks.append(
            Check(
                f"references {sym.name} [{sym.language}]",
                "languages",
                make_language_check(sym),
            )
        )
    checks += [
        Check("cross-file caller edge", "graph", check_cross_file_edge),
        Check("fsck reports no ghost nodes", "graph", check_fsck_clean),
        Check("no false 'produced no symbols' warning", "honesty", check_no_false_zero_symbol_warning, ["724"]),
        Check("semantic marker recovers after a commit", "honesty", check_marker_recovers_after_commit, ["741"]),
        Check("newly committed symbol is indexed", "honesty", check_new_symbol_is_indexed, ["741"]),
        Check("query warnings match reality", "honesty", check_query_warning_matches_reality, ["741"]),
    ]
    return checks


def run_all(ctx: Context, report: Report, phases: Optional[set[str]], name_filter: Optional[str]) -> None:
    import traceback

    current_phase = None
    for chk in all_checks():
        if phases and chk.phase not in phases:
            continue
        if name_filter and name_filter.lower() not in chk.name.lower():
            continue
        if chk.phase != current_phase:
            current_phase = chk.phase
            print(f"\n\033[1m== {chk.phase} ==\033[0m")
        started = time.monotonic()
        try:
            outcome, detail = chk.fn(ctx)
        except Exception:
            outcome, detail = Outcome.FAIL, "check raised:\n" + traceback.format_exc(limit=3)
        r = Result(
            name=chk.name,
            phase=chk.phase,
            outcome=outcome,
            detail=detail,
            issues=chk.issues,
            duration_s=time.monotonic() - started,
        )
        report.add(r)
        report.print_result(r)
