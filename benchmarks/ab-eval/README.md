# A/B eval — graph context vs. file-tools context (#318 O9)

The receipt for Travsr's headline claim: **fewer tokens, zero structural
hallucination**. This harness answers the same structural question two ways and
compares the answer quality and the context-token cost each approach imposes on
an LLM agent.

## The two arms

| Arm | What it models | Answer | Context cost |
|---|---|---|---|
| **graph** | An agent wired to the Travsr MCP server | the file nodes returned by one `travsr graph` query | tokens of the **answer payload** (resolved paths + signatures) — 0 source files read |
| **files-only** | An agent with only `grep` + `read` (Copilot/Cursor-style) | every file that textually mentions the symbol (`git grep -l`) | tokens of **all** those files, read in full |

The graph arm's cost is the answer Travsr returns, not the `--format json` CLI
debug envelope — an MCP-wired agent forwards the resolved answer, not the raw
framing. That keeps the comparison about the real asymmetry: the graph hands you
the answer, so you read **zero** source files; without it you read every
candidate in full to derive the same answer.

The files-only arm is the honest baseline for a code agent without a graph: to
decide whether a textual match is a real call site — versus a comment, a string,
or an unrelated same-named token — the agent has to *read the file*. So its
context cost is the sum of every candidate file, and its answer is a textual
superset of the truth (precision drops when the symbol appears in files that
don't actually call it).

The graph arm reads only the answer Travsr computed from the graph: exact set,
minimal tokens.

## What it gates

Ground truth is exact (the fixtures are the same ones the O8 accuracy suite
pins), so the graph arm is held to a hard standard:

- `graph_recall == 1` and `graph_precision == 1` — **no structural
  hallucination**, nothing missing.
- graph context tokens `≤` files-only context tokens on every task.

The **mean token reduction** is reported, not gated: the in-repo fixtures are
deliberately tiny, so the percentage here is illustrative. On a real repo the
files-only arm reads whole modules per query and the gap is dramatic; this
harness is the methodology and the regression guard, and the published figure
comes from running it against a large corpus.

## Run it

```bash
cargo build --release -p travsr-cli
node benchmarks/ab-eval/run.js --out ab-report.json
```

Exit code is non-zero if any task's graph arm regresses (inexact or more
expensive than reading files). The JSON report records per-task recall,
precision, context tokens, and token reduction for both arms, plus a summary
with each arm's success rate and the mean token reduction.

Runs nightly via `.github/workflows/ab-eval.yml` and on demand from the Actions
UI. Add tasks in `tasks.json`; reuse the O8 fixtures or point at new ones.
