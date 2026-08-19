# travsrAutomation

Release sanity and regression automation for the `travsr` CLI.

Verifies that a travsr build actually works, end to end, against a real
multi-language repository.

```bash
# regression: a locally built binary, before tagging anything
cargo build --release -p travsr-cli
python3 travsrAutomation/run.py --binary target/release/travsr

# sanity: a published release, before promoting it to the next channel
python3 travsrAutomation/run.py --tag v1.0.0-rc.1

# everything, with machine-readable output for CI
python3 travsrAutomation/run.py --tag v1.0.0 --with-cpp --strict-skip \
    --json sanity.json --junit sanity.xml
```

Stdlib only. Needs `git`, and `gh` for `--tag`.

## Why it exists

Every check corresponds to something that actually broke.

| Issue | What it was | Found by |
| --- | --- | --- |
| #724 | `status` warned that working languages "produced no symbols" | manual pass on `v1.0.0-beta.1` |
| #726 | Phase B queries promised a wait that never ends with no daemon | manual pass on `v1.0.0-beta.1` |
| #727 | `travsr lang status` did not exist, though the docs told agents to run it | manual pass on `v1.0.0-beta.1` |
| #728 | every build reported a bare `1.0.0`, so testers were indistinguishable from stable users | manual pass on `v1.0.0-beta.1` |
| #741 | the semantic marker never returns to `complete` after the first source commit | manual pass on `v1.0.0-rc.1` |

Three of the four beta findings had already shipped in a tagged artifact that
testers were being asked to install. The suite exists so the next pass is not
manual and not dependent on someone remembering to look.

## Modes

`--binary` and `--tag` run the same functional checks. The difference is what can
be verified:

- `--tag` additionally checks artifact checksums, that every expected target was
  published, and that the build id is **identical across all five targets**. That
  last one matters because the cross-built Linux target compiles in a container
  which does not inherit the host environment, so it is the one that can silently
  lose the build-id injection and ship a version string different from its
  siblings.
- `--binary` skips those, since there are no artifacts, and does not require a
  build id (a local build legitimately has none).

Checks that cannot run report **SKIP**, and every skip is listed in the summary.
A check that did not run must never read as a check that passed; that is the
specific way a suite like this stops being worth running. `--strict-skip` turns
skips into failures, which is what you want in CI.

## Phases

| Phase | Verifies |
| --- | --- |
| `artifacts` | checksums, target coverage, build-id identity (`--tag` only) |
| `first-run` | `lang status`, `init`, the no-daemon pending message, and that `init --semantic` genuinely completes Phase B with no daemon |
| `languages` | `references` resolves to the right file for typescript, javascript (`.mjs`), python, rust, and with `--with-cpp` also c and c++ |
| `graph` | a cross-file caller edge, which is the one answer Phase A alone cannot give; `fsck`; `--format json` is parseable |
| `mcp` | handshake, every documented tool advertised with a description and a well-formed schema, the `<travsr-data>` envelope, CLI/MCP agreement on the same symbol, path-traversal refusal, unknown-tool error, survival of a malformed frame, and `serverInfo.version` matching the CLI |
| `cli-surface` | smoke coverage for `ask`, `explain`, `pattern`, `repos`, `config get`, `synonym list`, `embed status`, `rerank status`, and that a missing symbol never panics or hangs |
| `honesty` | the marker and the data are asserted **separately** |

MCP is not an optional extra here: it is the only external interface travsr has
(principle 4, no REST and no GraphQL), so it is the surface an agent actually
uses. Adding this phase immediately found that `serverInfo.version` still
reported a bare crate version after #728 gave the CLI and the daemon an injected
build id, meaning the one interface agents talk to was the one that could not say
which build it was.

Run a subset with `--phase honesty --phase languages`, or `--filter references`.

## Why the honesty phase splits the marker from the data

#741 is the case where the marker lies and the graph is correct. A single
combined assertion would either miss it or blame the wrong thing, so there are
three checks:

- the marker returns to `complete` after a source commit and reindex
- a symbol committed after the last index is queryable (real staleness, if it fails)
- a query does not print "results are not authoritative" above a correct answer

On `v1.0.0-rc.1` the first and third fail and the second passes, which is exactly
the signature of a status bug rather than data loss.

## Side effects

Everything runs under `mktemp -d` with `TRAVSR_DISABLE_REGISTRY=1`, so a run
never touches your repo registry or any real repository.

The one exception is `--with-cpp`. C and C++ need a per-corpus trust grant
(ADR-017), and that grant is written to your real `~/.travsr/lang.toml`, outside
the temp dir. It is off by default and the suite says so when it fires.

## Layout

```
travsrAutomation/
├── run.py          entry point
├── selftest.py     tests for the harness itself (no travsr binary needed)
├── lib/
│   ├── checks.py   the checks, each tagged with the issues it guards
│   ├── fixtures.py the multi-language fixture repo and its expected answers
│   ├── report.py   outcomes, console output, JSON, JUnit
│   └── travsr.py   CLI wrapper, release download, checksums, build-id extraction
└── README.md
```

## Testing the harness

```bash
python3 travsrAutomation/selftest.py
```

23 tests, no travsr binary and no network, so unlike the suite it guards this can
run on every pull request.

It exists because the harness has already had two real bugs, both found by
running it rather than reading it. It scraped a corpus name out of travsr's
remediation line and kept a trailing backtick, writing a permanently
unmatchable entry into the caller's real `~/.travsr/lang.toml`; and that scrape
assumed the hint was present, so on any machine where trust had already been
granted every c/c++ check silently skipped, degrading on exactly the machines
that had run it most. Both are pinned.

It also asserts that #724, #726, #727, #728 and #741 each still have a check
guarding them, so deleting one fails rather than quietly reducing coverage.

## Adding a check

Write a function taking `Context` and returning `(Outcome, detail)`, then
register it in `all_checks()` with its phase and the issue numbers it guards:

```python
def check_something(ctx: Context) -> tuple[Outcome, str]:
    r = ctx.cli.run("references", "validateCharge")
    if "src/payment.ts" not in r.answer:
        return Outcome.FAIL, f"got:\n{r.answer[:300]}"
    return Outcome.PASS, ""
```

Use `r.answer` rather than `r.out` when asserting on a result: it strips
`warning:` lines, so a staleness banner cannot defeat an assertion about the
answer underneath it. Assert on `r.out` when the warning *is* the subject.

Raising is fine. The runner turns an unexpected exception into a FAIL with a
traceback, so a broken check is never mistaken for a passing one.

## CI

All in `.github/workflows/ci.yml`. No scheduled runs.

| Job | When | Mode |
| --- | --- | --- |
| `automation self-test` | every PR and push | `selftest.py`, seconds, no binary |
| `automation regression` | every PR and push | `--binary target/debug/travsr` |
| `automation sanity (ubuntu / macos)` | a release is published, or a manual run naming a tag | `--tag` |

The sanity job is guarded by

```yaml
if: github.event_name == 'release' || github.event.inputs.sanity_tag != ''
```

so it is skipped on every ordinary push and pull request, where `--tag` has
nothing to download. To verify a tag by hand: Actions, CI, "Run workflow", fill in
`sanity_tag` (and tick `sanity_with_cpp` if the runner has the scip-clang
sidecar).

Adding the `release: published` trigger means the rest of CI runs once per
release too. That is a few extra runner minutes, and `preflight` in `release.yml`
already required those checks green on the tagged commit, so treat it as
confirmation rather than new signal.

The regression job builds **debug**, not release: it indexes a seven-file
fixture, so compile time dominates and a release build would roughly double the
job for no extra signal.

No CI job passes `--strict-skip`. On a bare runner `cosign` and the c/c++
sidecars are legitimately absent, and the suite lists every skip in its summary
rather than folding one into a pass, so failing on those would make a job red for
reasons that have nothing to do with the release. Verified on a simulated bare
runner (empty `HOME`, so no `~/.travsr/bin`): 33 passed, 0 failed, 8 skipped.
Native Phase B needs no sidecar, so the cross-language, MCP and honesty phases
are real coverage there rather than smoke.

Results are uploaded as JSON and JUnit artifacts, and `summarise.py` renders a
job summary listing every failure and every skip, so a red release check is
readable without opening the log.
