# L4: doc-sweep cost at 10^4 chunks (kubernetes/website)

**Corpus:** `github.com/kubernetes/website`, shallow clone (`--depth 1`, commit `b035ea8`),
English-only via `.travsrignore` excluding every non-`en` locale under `content/`.
Never committed to or pushed from — the clone is local-only, `.travsrignore`/`.travsr/`
stay untracked, and the one test edit made for the single-edit pass was reverted
before this report was written.

## 0. A blocking bug found before any measurement was possible

`travsr init` reported **110 indexed files, 53 doc-chunk nodes** on the first attempt —
two orders of magnitude short of the corpus this repo actually has. Root cause:
`KNOWN_SOURCE_DIRS` (`travsr-daemon/src/lib.rs`) did not include `content`, the
directory kubernetes/website (a Hugo site) stores its *entire* documentation tree
in. The auto-exclusion heuristic (`detect_large_dep_dir`: a top-level directory with
>=1000 files and >=15% of the repo, not in the known-source allowlist, is treated as
a probable vendor/dependency dump) fired on `content/` and silently excluded it. In
a non-TTY run — any CI, or this session — the decision is only logged via
`tracing::info!`, never in the command's own visible output, so the exclusion was
invisible without inspecting the auto-generated `.travsrignore` by hand.

Fixed by adding `"content"` to `KNOWN_SOURCE_DIRS` (committed separately, see
`ed63d5d`). This is a real product bug independent of this measurement — any
Hugo/Jekyll/Gatsby/Next.js-content-collection repo with a top-level `content/`
directory would have hit it.

## 1. Chunk count after indexing

Post-fix: **2,966 files indexed** (7,436 skipped, mostly non-English locales
already excluded via `.travsrignore`), **11,580 doc-chunk nodes**, 12,612 total
embeddable nodes (11,580 doc + 1,032 code — this repo has some Go/JS tooling
alongside its Markdown content).

**11,580 >= 10^4.** This clears the scale bar cleanly — not a 10^3 corpus
mislabeled 10^4.

## 2. Cold pass: wall-clock and peak RSS

**~80 minutes total, ~798 MB peak RSS** (single sample window resolution; not
continuous instrumentation — see caveat below).

The pass was interrupted once by an external kill of the background shell (not a
travsr crash or hang — embed.db's incremental commits meant progress was not
lost) at 72% / 9,094 of 12,612 nodes, ~54 minutes in. Resuming with a second
plain `travsr embed reindex` picked up from exactly where it left off (the
sidecar's `WHERE NOT EXISTS` resume filter) and completed the remaining 3,518
nodes in a further 25m45s. Total: ~54 + ~26 = **~80 minutes**, consistent with
the plan's expectation that a first post-upgrade doc-space pass is measured in
hours-to-an-hour, not minutes, at this scale — and confirms the resume-from-partial
property holds at 10^4 scale, not just in the RFC-020 unit tests.

RSS was sampled at each progress check (roughly every 5-20 minutes), not
continuously monitored, so **798 MB is the highest *observed* sample, not a
verified peak** — the true peak could be modestly higher between samples. Given
F-A's fix (#376) digests text in the row mapper specifically to keep memory flat
with corpus size, and RSS was observed to fluctuate (410-798 MB) rather than climb
monotonically across the run, this is consistent with "flat with corpus size" as
intended, not a slow leak — but a follow-up with continuous sampling (e.g. `/usr/bin/time -l`
end-to-end, once a run isn't expected to need a mid-flight resume) would give an
exact number rather than a sampled one.

## 3. Warm pass: no changes

**8.55 s wall-clock**, all 12,612 nodes already embedded, nothing to do. This is
the number that matters most — it is what every user pays on every commit
thereafter, and the cold pass is paid exactly once. Two consecutive runs (one
immediately after the cold pass, one after the single-edit pass below) both
measured 8.55s, so this is stable, not a one-off.

## 4. Single-edit pass: one chunk re-embeds at scale

Edited one sentence inside an *existing* doc-chunk's body text (not the heading,
to keep chunk boundaries stable) in
`content/en/docs/concepts/overview/_index.md`, chunk `doc:what-kubernetes-is-not`
(node id `-9189790398674127224`), then reverted it after measuring.

**Caveat this surfaced:** `travsr init`'s change detection is git-commit-based
(matches CLAUDE.md: "the graph updates on every git commit via hooks") — an
uncommitted working-tree edit is invisible to it (`travsr init` reported "up to
date" with the edit present on disk). Since committing to this clone was out of
scope for this measurement, the daemon's live file watcher (a separate,
commit-independent mechanism backed by the `notify` crate) was used instead:
`travsr daemon start` picked up the uncommitted save directly and dropped
coverage to 12,611/12,612 within seconds.

Result, comparing a full before/after dump of every `(node_id, text_hash)` pair
in `embed.db`:

- **Exactly one** existing row's `text_hash` changed
  (`8af06bd6...` -> `80681b82...`), matching the one edited chunk precisely.
- Zero other existing rows changed.
- 22 brand-new rows appeared — unrelated to the edit: this repo's Phase B
  (semantic call-edge indexing) had never run before this session ("pending" at
  the end of the cold pass), and starting the daemon triggered it for the first
  time, adding a handful of previously-unindexed symbols. Noted and excluded from
  the single-edit measurement, not part of it.

§18.3 A proved single-chunk re-embed at 10^3 scale; this confirms the same
property holds unchanged at 10^4 scale (12,612 nodes) — one edit, one re-embed,
nothing else touched.

## 5. `DOC_VERIFY_MAX_ROWS` fallback branch

Not exercised in this pass. `DOC_VERIFY_MAX_ROWS` (`travsr-embed/src/freshness.rs`)
lives in the `travsr-embed` repo, not this one, and this corpus (11,580 doc rows)
sits far under the existing 200,000 default regardless. Making the constant
env-overridable and range-validated, then exercising the fallback branch by
setting it below this corpus's real size, is `travsr-embed`-side work and is
being tracked as a follow-up rather than folded into this pass.

## Summary

| measurement | result |
|---|---|
| doc-chunk count | 11,580 (>= 10^4 target) |
| cold pass | ~80 min wall-clock, ~798 MB peak RSS (sampled, not continuous) |
| warm pass | 8.55 s (stable across two runs) |
| single-edit pass | exactly 1 of 12,612 rows re-embedded, isolated and confirmed |
| blocking bug found | `content/` auto-excluded by default (fixed, committed `ed63d5d`) |
| fallback branch (`DOC_VERIFY_MAX_ROWS`) | not exercised — travsr-embed-side follow-up |

The corpus is real, at scale, and the core lifecycle properties (resume-from-partial,
warm-pass cost, single-edit isolation) hold at 10^4 chunks. The one genuinely open
item is the `DOC_VERIFY_MAX_ROWS` fallback exercise, which belongs in the
travsr-embed repo.
