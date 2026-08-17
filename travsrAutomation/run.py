#!/usr/bin/env python3
"""Travsr release sanity / regression suite.

Two modes, because the same checks are worth running at two different times:

    # sanity: against a published release, before promoting it
    python3 travsrAutomation/run.py --tag v1.0.0-rc.1

    # regression: against a locally built binary, before tagging anything
    cargo build --release -p travsr-cli
    python3 travsrAutomation/run.py --binary target/release/travsr

Every check corresponds to something that actually broke. #724, #726, #727 and
#728 were found by a manual pass on v1.0.0-beta.1, three of them already shipped
in a tagged artifact that testers were being asked to use. #741 was found by a
manual pass on v1.0.0-rc.1. The suite exists so the third pass is not manual.

Exit codes: 0 all good, 1 at least one failure (or a skip under --strict-skip),
2 bad usage or the suite could not start.

Isolation: everything runs under a temp directory with
TRAVSR_DISABLE_REGISTRY=1, so a run never touches the caller's repo registry or
any real repository. The single exception is --with-cpp, which needs a per-corpus
trust grant that lands in the caller's real ~/.travsr/lang.toml; it is off by
default and says so when it fires.

Stdlib only, matching the other scripts in this directory.
"""

import argparse
import os
import shutil
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent / "lib"))

import checks  # noqa: E402
from fixtures import Fixture  # noqa: E402
from report import Outcome, Report  # noqa: E402
from travsr import Travsr, download_release, extract, host_target  # noqa: E402

PHASES = ["artifacts", "first-run", "languages", "graph", "mcp", "cli-surface", "honesty"]


def parse_args(argv: list[str]) -> argparse.Namespace:
    p = argparse.ArgumentParser(
        prog="sanity",
        description="Verify a travsr release, or a local build, actually works.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "examples:\n"
            "  python3 travsrAutomation/run.py --tag v1.0.0-rc.1\n"
            "  python3 travsrAutomation/run.py --binary target/release/travsr\n"
            "  python3 travsrAutomation/run.py --tag v1.0.0 --with-cpp --junit sanity.xml\n"
        ),
    )
    src = p.add_mutually_exclusive_group(required=True)
    src.add_argument("--tag", help="published release tag to check, e.g. v1.0.0-rc.1")
    src.add_argument("--binary", help="path to a locally built travsr binary")
    p.add_argument("--repo", default=os.environ.get("TRAVSR_REPO", "Travsr-com/travsr"))
    p.add_argument(
        "--with-cpp",
        action="store_true",
        help="also exercise c/c++. Needs a per-corpus trust grant, which is written "
        "to your real ~/.travsr/lang.toml, outside this run's temp dir.",
    )
    p.add_argument("--phase", action="append", choices=PHASES, help="run only these phases (repeatable)")
    p.add_argument("--filter", help="run only checks whose name contains this substring")
    p.add_argument("--keep", action="store_true", help="keep the temp workdir for inspection")
    p.add_argument("--json", metavar="PATH", help="write machine-readable results here")
    p.add_argument("--junit", metavar="PATH", help="write JUnit XML here, for CI rendering")
    p.add_argument(
        "--strict-skip",
        action="store_true",
        help="treat skipped checks as failures. Use in CI, where a silently "
        "unverified guarantee is the thing you are trying to avoid.",
    )
    return p.parse_args(argv)


def resolve_binary(args: argparse.Namespace, work: Path, report: Report) -> tuple[Path, Path | None]:
    """Return (binary, artifacts_dir). artifacts_dir is None in --binary mode."""
    if args.binary:
        b = Path(args.binary).resolve()
        if not b.is_file() or not os.access(b, os.X_OK):
            print(f"error: {b} is not an executable file", file=sys.stderr)
            sys.exit(2)
        return b, None

    artifacts = work / "artifacts"
    ok, err = download_release(args.tag, args.repo, artifacts)
    if not ok:
        print(f"error: {err}", file=sys.stderr)
        sys.exit(2)

    target = host_target()
    if target is None:
        print("error: unsupported host platform", file=sys.stderr)
        sys.exit(2)
    matches = list(artifacts.glob(f"*{target}.tar.gz"))
    if not matches:
        print(f"error: release {args.tag} has no artifact for this host ({target})", file=sys.stderr)
        sys.exit(2)
    binary = extract(matches[0], work / f"x_{target}")
    if binary is None:
        print(f"error: no travsr binary inside {matches[0].name}", file=sys.stderr)
        sys.exit(2)
    return binary, artifacts


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    if shutil.which("git") is None:
        print("error: git is required to build the fixture repo", file=sys.stderr)
        return 2

    subject = args.tag or f"local build {args.binary}"
    report = Report(subject)
    work = Path(tempfile.mkdtemp(prefix="travsr-sanity-"))

    try:
        print(f"\033[1msubject:\033[0m {subject}")
        print(f"\033[1mworkdir:\033[0m {work}")

        binary, artifacts = resolve_binary(args, work, report)

        fixture = Fixture(work / "fixture")
        fixture.build()
        cli = Travsr(binary, fixture.root)

        ctx = checks.Context(
            work=work,
            cli=cli,
            fixture=fixture,
            artifacts=artifacts,
            with_cpp=args.with_cpp,
        )

        phases = set(args.phase) if args.phase else None
        try:
            checks.run_all(ctx, report, phases, args.filter)
        finally:
            # A daemon the suite started must not outlive it, whatever happened.
            try:
                cli.stop_daemon()
            except Exception:
                pass

        report.print_summary(strict_skip=args.strict_skip)
        if args.json:
            report.to_json(Path(args.json))
            print(f"  wrote {args.json}")
        if args.junit:
            report.to_junit(Path(args.junit))
            print(f"  wrote {args.junit}")
        return report.exit_code(strict_skip=args.strict_skip)
    finally:
        if args.keep:
            print(f"\n  workdir kept: {work}")
        else:
            shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
