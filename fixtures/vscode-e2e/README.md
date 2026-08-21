# vscode-e2e fixture

Test data for validating the VS Code extension against a real graph, not mocks.

The extension's existing fixture workspace
(`packages/travsr-vscode/src/test/fixtures/workspace/`) is 12 lines across two
files and contains no call edges, so nothing the extension actually shows -
caller counts, CodeLens, tree, blast radius - is exercised end to end. This
fixture exists to close that gap and to stay useful for future runs.

## The one rule

**`expected.json` is hand-written. Never regenerate it from travsr output.**

A generated expectations file records whatever the tool does today, including
its bugs, and then asserts the tool still does that. The suite goes green
forever and validates nothing. Every path and line number in `expected.json`
was read off the source by a human and must be edited by hand.

For the same reason the fixture sources carry a `FROZEN FIXTURE` header:
reformatting a file shifts line numbers and silently invalidates the ground
truth. Add new files rather than editing existing ones.

## Layout

```
ts/orders.ts     OrderService.submit - 2 callers
ts/handlers.ts   calls submit and util.format
ts/jobs.ts       calls submit
ts/main.ts       entry point, 0 callers
ts/types.ts      type-only, no callables
ts/util.ts       format #1, 1 caller
ts/report.ts     format #2, 0 callers
py/pipeline.py   format #3 plus its caller, second language
rb/legacy.rb     no Phase B provider by default
```

## What each case is for

Four of the cases are ordinary and any fixture would cover them. The other four
are the ones worth having, because they are where the extension answers
confidently and wrongly:

| Case | Failure it catches |
|---|---|
| `main-zero-callers` | empty result rendered as an error or an endless spinner |
| `types-no-codelens` | a "0 callers" lens attached to a type declaration |
| `format-three-defs-same-name` | three definitions collapsed into one, so `report.ts` inherits `util.ts`'s caller |
| `ruby-partial-coverage-softening` | a definitive "0 references" for a file Phase B never analysed (#450 / #551) |

The last one is the regression test that the `file_has_occurrences` fix in #551
does not currently have anywhere.

## Runtime cases

`ghost-node-after-delete` and `stale-status-surfaces` cannot be expressed as
static files - they are index-then-mutate sequences. `expected.json` records
them under `runtime` with the exact procedure.

## Running it

The fixture is source only. A harness must copy it to a temp directory,
`git init`, index it, and compare against `expected.json`. It is deliberately
not indexed in place: `travsr init` inside the travsr checkout would register a
second repo and pollute the developer's own graph.

Use a **short** temp root such as `/tmp/tvfx`. The daemon binds a unix socket
inside `.travsr/` and fails with `path must be shorter than SUN_LEN` under a
long path, which is easy to hit with a generated temp directory name.

`ruby-partial-coverage-softening` asserts a precondition - that no Ruby Phase B
provider is installed. On a runner where `scip-ruby` is present the case must
be **skipped, not failed**, and the skip reason recorded. A run where a case
could not be exercised must never read the same as a run where it passed.
