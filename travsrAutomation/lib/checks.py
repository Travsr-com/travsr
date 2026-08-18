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

import json
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
    """Verify each artifact against its Sigstore bundle with `cosign verify-blob`.

    This used to return PASS the moment `cosign` was on PATH, having verified
    nothing, and the bundles were not even downloaded. That is the exact failure
    `report.py` says this suite exists to prevent, a check that did not run
    reading as a check that succeeded, and it was worse than a SKIP because it was
    invisible in both the summary and the JSON.
    """
    if ctx.artifacts is None:
        return Outcome.SKIP, "no release downloaded (running against a local binary)"
    if shutil.which("cosign") is None:
        return Outcome.SKIP, "cosign not installed, so .bundle signatures were NOT verified"

    bundles = sorted(ctx.artifacts.glob("*.tar.gz.bundle"))
    if not bundles:
        return Outcome.SKIP, "the release published no .bundle files to verify"

    import subprocess

    verified, problems = 0, []
    for bundle in bundles:
        blob = bundle.with_suffix("")  # strip `.bundle`
        if not blob.is_file():
            problems.append(f"{bundle.name}: no matching artifact to verify")
            continue
        proc = subprocess.run(
            [
                "cosign", "verify-blob",
                "--bundle", str(bundle),
                # Keyless signing: the release workflow uses GitHub OIDC, so the
                # identity is the workflow itself rather than a key.
                "--certificate-oidc-issuer", "https://token.actions.githubusercontent.com",
                "--certificate-identity-regexp", r"^https://github\.com/Travsr-com/travsr/",
                str(blob),
            ],
            capture_output=True, text=True, timeout=300,
        )
        if proc.returncode == 0:
            verified += 1
        else:
            tail = (proc.stderr or proc.stdout).strip().splitlines()
            problems.append(f"{blob.name}: {tail[-1][:160] if tail else 'verification failed'}")

    if problems:
        return Outcome.FAIL, "\n".join(problems)
    return Outcome.PASS, f"{verified} artifact signature(s) verified"


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
        _ensure_indexed(ctx)
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
    _ensure_indexed(ctx)
    r = ctx.cli.run("graph", CROSS_FILE_CALLEE, "--direction", "both")
    if CROSS_FILE_CALLER not in r.answer:
        return Outcome.FAIL, (
            f"expected {CROSS_FILE_CALLER} as a caller of {CROSS_FILE_CALLEE}\n"
            f"got:\n{r.answer.strip()[:400]}"
        )
    return Outcome.PASS, ""


def check_fsck_clean(ctx: Context) -> tuple[Outcome, str]:
    _ensure_indexed(ctx)
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
    _ensure_indexed(ctx)
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
    _ensure_indexed(ctx)
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
        Check("graph --format json is parseable", "graph", check_graph_json_is_parseable),
        Check("fsck reports no ghost nodes", "graph", check_fsck_clean),
        # MCP: the only external interface, so these are not optional extras.
        Check("mcp handshake", "mcp", check_mcp_handshake),
        Check("mcp advertises every documented tool", "mcp", check_mcp_lists_tools),
        Check("mcp tools carry descriptions and schemas", "mcp", check_mcp_tools_have_schemas),
        Check("mcp responses are enveloped", "mcp", check_mcp_tool_call_is_enveloped),
        Check("mcp and cli agree on the same symbol", "mcp", check_mcp_find_references_agrees_with_cli),
        Check("mcp refuses path traversal", "mcp", check_mcp_rejects_path_traversal),
        Check("mcp errors on an unknown tool", "mcp", check_mcp_unknown_tool_errors),
        Check("mcp survives a malformed frame", "mcp", check_mcp_survives_a_malformed_frame),
        Check("mcp serverInfo.version matches the cli", "mcp", check_mcp_server_version_matches_cli, ["728"]),
        # cli-surface: shallow smoke coverage for the subcommands nothing else
        # touches. A release can break argument parsing anywhere.
        Check("ask answers", "cli-surface", check_ask_runs),
        Check("explain runs", "cli-surface", check_explain_runs),
        Check("pattern searches", "cli-surface", check_pattern_runs),
        Check("repos runs", "cli-surface", check_repos_runs),
        Check("config get runs", "cli-surface", check_config_get_runs),
        Check("synonym list runs", "cli-surface", check_synonym_list_runs),
        Check("embed status runs", "cli-surface", check_embed_status_runs),
        Check("rerank status runs", "cli-surface", check_rerank_status_runs),
        Check("a missing symbol never panics or hangs", "cli-surface", check_no_command_panics_on_a_missing_symbol),
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


# ── phase: mcp ───────────────────────────────────────────────────────────────
#
# MCP is the only external interface travsr exposes (principle 4). Everything
# above this point tests the CLI, which is a convenience wrapper; an agent talks
# to the server. Leaving this phase out meant the primary surface was the least
# verified one.


def _ensure_indexed(ctx: "Context") -> None:
    """Index the fixture if it is not already.

    Every phase after `first-run` queries a graph, so none of them can assume an
    earlier phase ran. Without this, `--phase honesty` or `--filter references`
    (the invocations someone iterating on one check actually types) ran against a
    fixture that was never indexed and came back red for reasons that had nothing
    to do with the product.

    `travsr mcp --stdio` additionally refuses to start without an index and exits,
    so the mcp phase would not even reach its assertions.
    """
    if "nodes:" not in ctx.cli.status():
        ctx.cli.run("init", "--semantic")


def _mcp(ctx: "Context"):
    from mcp import McpClient

    _ensure_indexed(ctx)
    return McpClient(ctx.cli.binary, ctx.fixture.root)


def check_mcp_handshake(ctx: Context) -> tuple[Outcome, str]:
    from mcp import PROTOCOL_VERSION

    with _mcp(ctx) as c:
        info = c.initialize()
        if info.get("protocolVersion") != PROTOCOL_VERSION:
            return Outcome.FAIL, f"protocolVersion is {info.get('protocolVersion')!r}"
        # `"tools": {}` is a valid declaration: tools are supported, with no
        # sub-capabilities. Presence is the contract, not truthiness.
        if "tools" not in info.get("capabilities", {}):
            return Outcome.FAIL, f"server advertises no tools capability: {info}"
        return Outcome.PASS, f"{info.get('serverInfo', {})}"


def check_mcp_server_version_matches_cli(ctx: Context) -> tuple[Outcome, str]:
    """#728, on the interface that matters.

    `travsr --version` reports the injected build id, but `serverInfo.version`
    comes from the crate version. An agent talking over MCP therefore cannot tell
    which build it is attached to, which is the exact ambiguity #728 removed from
    the CLI. Skipped for a local build, which legitimately has no injected id.
    """
    if ctx.artifacts is None:
        return Outcome.SKIP, "local build has no injected build id to compare against"
    cli_version = ctx.cli.version().replace("travsr", "").strip()
    with _mcp(ctx) as c:
        server_version = c.initialize().get("serverInfo", {}).get("version", "")
    if server_version != cli_version:
        return Outcome.FAIL, (
            f"MCP serverInfo.version is {server_version!r} but the CLI reports "
            f"{cli_version!r}; an agent cannot identify the build it is talking to"
        )
    return Outcome.PASS, server_version


def check_mcp_lists_tools(ctx: Context) -> tuple[Outcome, str]:
    """The documented query tools must all be advertised, not merely some."""
    expected = {
        "get_dependencies", "get_callers", "find_references", "find_pattern",
        "get_blast_radius", "get_execution_path", "get_context", "get_snippets",
        "search_symbol", "get_repo_map", "get_graph_stats", "get_graph_json",
    }
    with _mcp(ctx) as c:
        c.initialize()
        names = {t.get("name") for t in c.list_tools()}
    missing = sorted(expected - names)
    if missing:
        return Outcome.FAIL, f"tools/list is missing: {', '.join(missing)}"
    return Outcome.PASS, f"{len(names)} tools advertised"


def check_mcp_tools_have_schemas(ctx: Context) -> tuple[Outcome, str]:
    """A tool without an input schema cannot be called correctly by a model."""
    with _mcp(ctx) as c:
        c.initialize()
        tools = c.list_tools()
    problems: list[str] = []
    documented_params = 0
    for t in tools:
        name = t.get("name", "<unnamed>")
        if not t.get("description"):
            problems.append(f"{name}: no description")
        schema = t.get("inputSchema") or {}
        # An empty `properties` is correct for a zero-argument tool such as
        # get_repo_map or get_graph_stats, so the contract is a well-formed object
        # schema, not a non-empty one. Requiring parameters here failed nine
        # perfectly correct tools on the first run of this check.
        if schema.get("type") != "object":
            problems.append(f"{name}: inputSchema is not an object schema")
            continue
        for param, spec in (schema.get("properties") or {}).items():
            documented_params += 1
            if not (spec or {}).get("description"):
                # A parameter a model cannot understand is a parameter it will
                # guess at, which is the failure mode this catches.
                problems.append(f"{name}.{param}: parameter has no description")
    if problems:
        return Outcome.FAIL, "\n".join(problems[:10])
    return Outcome.PASS, f"{len(tools)} tools, {documented_params} documented parameters"


def check_mcp_tool_call_is_enveloped(ctx: Context) -> tuple[Outcome, str]:
    """Every response reaching a model must be wrapped in <travsr-data>.

    The envelope is the boundary that keeps repo content from being read as
    instructions. A tool answering outside it is a prompt-injection surface.
    """
    from mcp import ENVELOPE_CLOSE, ENVELOPE_OPEN

    with _mcp(ctx) as c:
        c.initialize()
        r = c.call_tool("search_symbol", {"name": "validateCharge"})
        if "error" in r:
            return Outcome.FAIL, f"search_symbol errored: {r['error']}"
        text = c.text_of(r)
    if ENVELOPE_OPEN not in text or ENVELOPE_CLOSE not in text:
        return Outcome.FAIL, f"response not enveloped:\n{text[:300]}"
    if "validateCharge" not in text:
        return Outcome.FAIL, f"envelope present but the answer is wrong:\n{text[:300]}"
    return Outcome.PASS, ""


def check_mcp_find_references_agrees_with_cli(ctx: Context) -> tuple[Outcome, str]:
    """The two interfaces must not disagree about the same graph.

    A divergence here is worse than either being wrong alone, because it means
    the answer depends on how you asked.
    """
    with _mcp(ctx) as c:
        c.initialize()
        text = c.text_of(c.call_tool("find_references", {"symbol": "summarize_rows"}))
    cli = ctx.cli.run("references", "summarize_rows").answer
    if "src/report.py" not in text:
        return Outcome.FAIL, f"MCP find_references missed src/report.py:\n{text[:300]}"
    if ("src/report.py" in cli) != ("src/report.py" in text):
        return Outcome.FAIL, "CLI and MCP disagree about the same symbol"
    return Outcome.PASS, ""


def check_mcp_rejects_path_traversal(ctx: Context) -> tuple[Outcome, str]:
    """SEC-002: arguments are validated before any store access.

    Asserted as "does not leak", not "returns an error": a refusal and an empty
    answer are both acceptable, reading /etc/passwd is not.
    """
    attempts = ["../../../../etc/passwd", "/etc/passwd", "..\\..\\windows\\system32"]
    with _mcp(ctx) as c:
        c.initialize()
        for probe in attempts:
            r = c.call_tool("get_dependencies", {"file": probe})
            text = c.text_of(r)
            if "root:" in text or "/bin/bash" in text or "Administrator" in text:
                return Outcome.FAIL, f"{probe!r} appears to have escaped the repo:\n{text[:300]}"
    return Outcome.PASS, f"{len(attempts)} traversal attempts refused or empty"


def check_mcp_unknown_tool_errors(ctx: Context) -> tuple[Outcome, str]:
    """An unknown tool must be a protocol error, not a silent empty success."""
    with _mcp(ctx) as c:
        c.initialize()
        r = c.call_tool("definitely_not_a_tool", {})
    if "error" not in r and not (r.get("result") or {}).get("isError"):
        return Outcome.FAIL, f"unknown tool did not error: {json.dumps(r)[:300]}"
    return Outcome.PASS, ""


def check_mcp_survives_a_malformed_frame(ctx: Context) -> tuple[Outcome, str]:
    """A garbage line must not kill the session.

    An agent that sends one bad frame should not have to reconnect, and a server
    that exits here turns a client bug into an outage.
    """
    with _mcp(ctx) as c:
        c.initialize()
        assert c._proc.stdin is not None
        c._proc.stdin.write("this is not json\n")
        c._proc.stdin.flush()
        try:
            tools = c.list_tools()
        except Exception as e:
            return Outcome.FAIL, f"session died after a malformed frame: {e}"
    return Outcome.PASS, f"still serving after garbage input ({len(tools)} tools)"


# ── phase: cli-surface ───────────────────────────────────────────────────────
#
# Smoke coverage for the subcommands the suite otherwise never touches. Shallow
# by design: the point is that they run, parse their arguments and do not crash
# on a real index, which is the class of breakage a release can introduce
# anywhere. Depth belongs in each area's own tests.


def _smoke(ctx: Context, *args: str, expect: str = "") -> tuple[Outcome, str]:
    _ensure_indexed(ctx)
    r = ctx.cli.run(*args)
    if r.timed_out:
        return Outcome.FAIL, f"`{' '.join(args)}` did not return within the timeout"
    if "panicked" in r.out or "RUST_BACKTRACE" in r.out:
        return Outcome.FAIL, f"`{' '.join(args)}` panicked:\n{r.out[:300]}"
    # The exit code is the load-bearing assertion, not an extra. A removed or
    # renamed subcommand exits 2 with clap's "unrecognized subcommand" and none of
    # the other conditions here trip, so without this every check that passes no
    # `expect` would report PASS for a command that no longer exists. That is
    # #727 exactly, which is the regression this phase exists to catch.
    if r.code != 0:
        return Outcome.FAIL, f"`{' '.join(args)}` exited {r.code}:\n{r.out[:300]}"
    if expect and expect not in r.out:
        return Outcome.FAIL, f"expected {expect!r} in output of `{' '.join(args)}`:\n{r.out[:300]}"
    return Outcome.PASS, ""


def check_ask_runs(ctx: Context) -> tuple[Outcome, str]:
    # Asserts it answers, not that it ranks well. Retrieval quality is a measured
    # open problem (hit@1 0.208), so pinning ranking here would freeze numbers
    # the project is actively trying to move.
    return _smoke(ctx, "ask", "validateCharge", expect="validateCharge")


def check_explain_runs(ctx: Context) -> tuple[Outcome, str]:
    # `explain` takes <QUERY> <SYMBOL>, not one argument. The single-argument
    # form exits 2 on a missing required argument, and this check reported PASS
    # for it until `_smoke` started asserting the exit code.
    return _smoke(ctx, "explain", "validateCharge", "validateCharge")


def check_pattern_runs(ctx: Context) -> tuple[Outcome, str]:
    return _smoke(ctx, "pattern", "amountCents", expect="src/payment.ts")


def check_repos_runs(ctx: Context) -> tuple[Outcome, str]:
    return _smoke(ctx, "repos")


def check_config_get_runs(ctx: Context) -> tuple[Outcome, str]:
    # Must be a key in travsr-config's registry. `reindex.max_parallelism` does
    # not exist anywhere in the tree, so this check was asserting against an
    # `unknown key` error and reporting PASS, because `_smoke` ignored the exit
    # code at the time.
    # `config get` prints the value, not the key, so asserting on the key name
    # would fail on correct output. The exit code is what matters here: an
    # unknown key exits non-zero, which is how the previous
    # `reindex.max_parallelism` version was silently failing.
    return _smoke(ctx, "config", "get", "embed.capacity")


def check_synonym_list_runs(ctx: Context) -> tuple[Outcome, str]:
    return _smoke(ctx, "synonym", "list")


def check_embed_status_runs(ctx: Context) -> tuple[Outcome, str]:
    return _smoke(ctx, "embed", "status")


def check_rerank_status_runs(ctx: Context) -> tuple[Outcome, str]:
    return _smoke(ctx, "rerank", "status")


def check_graph_json_is_parseable(ctx: Context) -> tuple[Outcome, str]:
    """`--format json` must emit JSON, since renderers consume it."""
    _ensure_indexed(ctx)
    r = ctx.cli.run("graph", "PaymentService", "--format", "json")
    payload = r.stdout.strip()
    if not payload:
        return Outcome.FAIL, "no stdout from graph --format json"
    try:
        json.loads(payload)
    except json.JSONDecodeError as e:
        return Outcome.FAIL, f"graph --format json emitted invalid JSON: {e}\n{payload[:300]}"
    return Outcome.PASS, ""


def check_no_command_panics_on_a_missing_symbol(ctx: Context) -> tuple[Outcome, str]:
    """A symbol that does not exist must be answered, not crashed on."""
    problems = []
    for args in (("references", "zzz_no_such_symbol"),
                 ("graph", "zzz_no_such_symbol"),
                 ("ask", "zzz_no_such_symbol"),
                 ("explain", "zzz_no_such_symbol")):
        r = ctx.cli.run(*args)
        if "panicked" in r.out:
            problems.append(f"{' '.join(args)} panicked")
        if r.timed_out:
            problems.append(f"{' '.join(args)} hung")
    return (Outcome.FAIL, "\n".join(problems)) if problems else (Outcome.PASS, "")
