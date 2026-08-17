# Release sanity / regression suite

Verifies that a travsr build actually works, end to end, against a real
multi-language repository.

```bash
# regression: a locally built binary, before tagging anything
cargo build --release -p travsr-cli
python3 scripts/sanity/main.py --binary target/release/travsr

# sanity: a published release, before promoting it to the next channel
python3 scripts/sanity/main.py --tag v1.0.0-rc.1

# everything, with machine-readable output for CI
python3 scripts/sanity/main.py --tag v1.0.0 --with-cpp --strict-skip \
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
| `graph` | a cross-file caller edge, which is the one answer Phase A alone cannot give; `fsck` |
| `honesty` | the marker and the data are asserted **separately** |

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

## Not wired into CI

`--tag` needs a published release, so it cannot run on a pull request. `--binary`
can, and is the mode to wire up if the team wants this gating merges; it takes a
few minutes because it indexes a real repo and waits on Phase B.
