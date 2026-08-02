# #520: selective doc-lane reranking — threshold sweep (travsr)

**Decision: do not ship. Record the sweep, keep the current cost (always rerank).**

## Mechanism

`seed.rs::doc_rerank_ambiguity_threshold()` (env `TRAVSR_DOC_RERANK_AMBIGUITY_THRESHOLD`,
off by default): when set, `tools::rerank_doc_candidates` skips the cross-encoder pass
entirely whenever the top raw-cosine candidate already clears the threshold, falling
through to `cosine_floor_select` instead. Thresholds swept were calibrated to the
active model's own measured cosine range (`embed_cos_lo`/`embed_cos_hi` in graph.db
meta, written by `travsr embed calibrate`) rather than a fixed absolute scale — raw
cosine is model-relative, and an earlier design mistake elsewhere in this codebase
(pre-model-relative-floors) is exactly what a fixed cutoff would repeat.

## Result (travsr, arctic-embed-m-v1.5, cos range 0.337-0.509)

| threshold | hit@1 | hit@3 | latency ratio | flips |
|---|---|---|---|---|
| baseline (always rerank) | 0.90 | 0.90 | 1.00 | — |
| 0.337 | 0.90 | 0.90 | 0.967 | 0 |
| 0.377 | 0.90 | 0.90 | 0.879 | 0 |
| 0.417 | 0.90 | 0.90 | 1.101 | 0 |
| 0.457 | 0.90 | 0.90 | 0.941 | 0 |
| 0.497 | 0.90 | 0.90 | 0.908 | 0 |
| 0.538 | 0.90 | 0.90 | 0.993 | 0 |
| 0.578 | 0.90 | 0.90 | 1.036 | 0 |

Full per-run data: `report-selective-rerank-sweep-travsr.json`.

## Reading the result

Accuracy is perfectly preserved at every threshold (hit@1/hit@3 unchanged, zero
per-query hit-to-miss flips) — the strict half of #520's gate is cleared everywhere.
But the latency ratio never shows a material, consistent cut: every value sits in a
0.88-1.10 band, which is noise on this measurement (a single-process, unpinned local
run against a live cross-encoder), not a real effect of the skip condition firing.
None of the seven thresholds produces a repeatable win.

This is a legitimate outcome, not an inconclusive one: `flips=0` at every threshold
means the skip condition essentially never disagrees with the reranker when it does
fire, but it also is not firing often enough (or the candidate pool this repo's 20
doc queries produce is not ambiguous enough) to move the cost. Raising the threshold
range further would only *reduce* how often it fires, not increase it — the swept
range already spans the model's calibrated confidence band top to bottom.

## What this does not do

- **Does not rule out a win on a different corpus.** kubernetes was not swept in this
  pass (see the plan's own bench-first framing: the k8s side is worth running before
  a final close-out, but the travsr result alone already fails the "materially cuts"
  half of the ship bar, and both repos must clear it — so it does not change this
  decision).
- **Does not indict the mechanism.** The code (env-gated, off by default) stays in
  the tree since it cost little and is reusable if a future corpus or model shows a
  cleaner signal. It ships as dead weight until proven, per the plan's own framing:
  "the honest outcome is to record the sweep and keep the cost."

## Repro

```
cargo build --release
BENCH_LABEL=travsr node bench/sweep-selective-rerank.mjs
```
