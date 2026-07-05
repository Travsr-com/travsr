# Accuracy benchmark (#318 O8)

Nightly regression gate that turns Travsr's accuracy and performance claims
into machine-checked facts. Runs in CI via
`.github/workflows/accuracy-nightly.yml` and locally with:

```sh
cargo build --release -p travsr-cli
node benchmarks/accuracy/run.js
```

## What it measures

| Class | Ground truth | Metric |
|---|---|---|
| `method-recall` | every definition in the corpus source | recall |
| `caller-set` | who calls/imports a symbol | recall + precision |
| `import-set` | what a file imports | recall + precision |
| `seed-resolution` | query resolves to the right node | exact match |
| `token-budget` | `--budget` caps output and reports truncation | invariant |
| perf (#295-T6) | cold `travsr init` wall time, node/edge counts | threshold |
| latency | every timed CLI invocation | p50 / p95 |

Thresholds live in `manifest.json`; any violation fails the run (exit 1), so a
deliberately broken ranking or caller patch fails CI. `report.json` is uploaded
as a workflow artifact for trend inspection.

## Corpora

- `fixtures/ts-callers`, `fixtures/ts-small` - pinned in-repo polyglot
  fixtures; ground truth is exact and the gate fully deterministic. These run
  on the Phase A (tree-sitter) structural graph, so the suite passes with no
  external language tooling installed.
- `kubernetes-pkg` - staged behind `enabled: false`. The corpus that motivated
  WS2/WS3 (613K-node index): a sparse clone of `kubernetes/kubernetes@<pinned
  SHA>` limited to `pkg/kubelet`, indexed with scip-go Phase B, with
  caller-set ground truth sampled per the RFC-014 verification protocol.
  Enable once scip-go is provisioned in the nightly runner.

## Adding a corpus or case

Add fixture files (or a pinned `clone` spec), then append to
`manifest.json`: `expected_definitions` drives `method-recall`; each entry in
`cases` declares its `class`, the exact CLI `args`, and the expectation. The
runner copies the corpus into a temp git repo, runs `travsr init`, and asserts
on the `--format json` output - no store internals are touched.
