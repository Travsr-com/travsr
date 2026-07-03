# Plan — Model-relative semantic floors

**Status:** IMPLEMENTED (Approach C, auto-calibrated) on branch
`feature/travsr-plugin-host-config-catalog-arctic`. Gates green; benches rerun.
**Origin:** k8s arctic-256 bench (`bench/report-k8s-arctic.md`) — failure mode #1,
over-abstention. Correct nodes are retrieved but demoted to "speculative guesses"
because the confidence classifier's absolute cosine floors were calibrated on
bge-small and arctic-256's cosine scale is lower/compressed.

## Outcome (measured)

Auto-calibration measured arctic-embed-256's anchors on k8s: **cos_lo (nonsense p95)
= 0.363, cos_hi (self-match p50) = 0.560**. This is the smoking gun — arctic-256's
*entire* answerable band tops out at ~0.56, so the old absolute floors
(`confirm_anchor_floor` 0.66, `semantic_promote_strong` 0.72) sat **above everything
the model could produce** → guaranteed abstention on every conceptual query.

Reference anchors are bge's `{REF_LO 0.55, REF_HI 0.77}` (the veto/abstain floor and
the answerable-band floor). They MUST bracket the authored floors; an earlier attempt
with `REF_LO 0.64` put the veto floor *below* `lo` and made abstention vanish
(0/3 nonsense) — corrected to 0.55.

k8s arctic-256 result (get_context, budget 2000), before → after:
- literal hit@1 4/6 → **5/6** (L4 `Scheduler ScheduleOne` MISS → rank1 strong)
- conceptual **grounded** ~2/8 → **8/8** (was abstaining "no grounded match")
- conceptual hit@10 3/8 → **4/8**
- nonsense abstain 2/3 → **1/3** — residual leak (N1/N2 return *weak*, not strong);
  rooted in arctic-256's thin nonsense↔answerable separation (0.363 vs 0.560), i.e.
  failure-mode #2, which needs a stronger model (arctic-768), not floor tuning.

bge-small (reference): repo carries no `embed_cos_*` meta → `Calibration::IDENTITY`
→ floors byte-for-byte unchanged (verified: travsr graph.db has no calibration meta;
`calibration_map_is_identity_at_reference` unit test asserts the identity).

## Problem

`classify_confidence` (`crates/travsr-mcp/src/seed.rs:387`) decides
Exact/Strong/Weak/None from the embed oracle's cosines using **absolute** cutoffs.
Every one was measured on bge-small (doc comments cite "answerable ≥ 0.77,
nonsense ≤ 0.64 on bge-small"):

| symbol | loc | default | role |
|---|---|---|---|
| `SEMANTIC_ORACLE_MIN` | seed.rs:567 | 0.58 | oracle trusted at all (`oracle_confident`) |
| `SEMANTIC_ABS_FLOOR` | seed.rs:575 | 0.55 | absolute keep-floor for `floor` |
| `SEMANTIC_REL_DELTA` | seed.rs:573 | 0.13 | band below `oracle_top` |
| `semantic_veto_floor()` | seed.rs:125 | 0.55 | veto Weak→None |
| `confirm_anchor_floor()` | seed.rs:112 | 0.66 | low-coverage anchor rescue |
| `semantic_promote_strong()` | seed.rs:87 | 0.72 | oracle-alone Strong |

Because arctic-256 answerable queries score below these, `classify_confidence`
returns `None` → `abstain_with_guesses` path → correct node demoted (bench L4/C4).
`semantic_validate` (seed.rs:588) uses the same `floor`, compounding the cut.

The cosine scale that these floors assume depends on the **model** (arch, dim,
query/passage prefix asymmetry) and, to a lesser degree, the **corpus**. It must
therefore be derived automatically per (model, corpus), not hardcoded — see
Approach C. The per-repo `embed.db` (where reindex writes and the oracle reads) is
the natural home for the derived calibration.

## Approaches considered

### A. Per-model floor set in the descriptor (explicit)
Add all six floors as optional fields on `ModelDescriptor`; `classify_confidence`
reads the active model's values (falling back to today's bge-small defaults).
- **+** Maximally transparent; each model's floors are auditable in `model.toml`.
- **−** Six numbers to calibrate per model; easy to get internally inconsistent
  (e.g. veto_floor > confirm_floor).

### B. Hand-calibrated per-model constants
Add the two (or six) reference cosines to `ModelDescriptor` / `embed_catalog.toml`
as literals, filled in by whoever adds the model.
- **+** Transparent, auditable in `model.toml`.
- **− REJECTED: not generic.** Every new model needs a human to measure and enter
  numbers. A model added by a user (via `~/.travsr/embed_catalog.toml`) or shipped
  in a future release would fall back to bge-small floors and silently over/under
  abstain until someone calibrates it. Violates the "works for any model, present
  or future, with zero manual tuning" requirement.

### C. Auto-calibrated model-relative floors (RECOMMENDED — fully generic)
Keep the six floors, but express them **once** in a model-independent `[0,1]`
**answerability** coordinate, and derive each model's cosine→answerability mapping
**automatically at reindex time**, label-free, from the model+corpus itself. No
per-model constants, no human step — a brand-new model calibrates itself the first
time its HNSW is built.

**Two anchors, both measured automatically (no labels, no ground truth):**
- `cos_hi` — the "this query matches its answer" scale. Sample N≈256 nodes; embed
  each node's own signature/name through the **full query path** (query prefix,
  truncation, pooling — identical to a real query) and take the cosine to that same
  node's stored **passage** vector. `cos_hi = p50` of these self-match cosines.
  Because it runs the real query path, it captures each model's query/passage
  **asymmetry** (e.g. arctic's `Represent this sentence…` prefix depresses absolute
  cosine) — exactly the effect that breaks absolute floors.
- `cos_lo` — the "confident but unrelated" background scale. Same N sampled query
  vectors vs **random other** node vectors; `cos_lo = p95` of those cross cosines
  (p95, not max, so a fluke near-duplicate doesn't inflate it).

Normalize every measured cosine into answerability space:
```
a(cos) = clamp01( (cos − cos_lo) / (cos_hi − cos_lo) )
```
The six floors become fixed **fractions of the band**, derived once from bge-small's
known-good points so the reference model is unchanged:
```
bge-small anchors ≈ { cos_lo: 0.64, cos_hi: 0.77 }  (from its own auto-calibration)
ORACLE_MIN 0.58 → a ≈ -0.46 (clamped 0)   ABS_FLOOR/veto 0.55 → a ≈ -0.69 (0)
confirm 0.66 → a ≈ 0.15    promote_strong 0.72 → a ≈ 0.62    REL_DELTA 0.13 → Δa ≈ 1.0
```
(exact fractions pinned in step 3 so bge-small reproduces today byte-for-byte.)
At classify time compare `a(measured_cos)` to the band-fractions instead of raw cosine.

- **+ Fully generic.** Any model in the catalog — bundled, future-release, or a
  user's own `embed_catalog.toml` entry — self-calibrates on first reindex. Zero
  manual numbers. Captures query/passage asymmetry and dim-truncation compression
  automatically. bge-small is provably unchanged (its auto-anchors regenerate its
  own floors). Small blast radius: wrap cosines through `a()`; logic/tests barely move.
- **−** One assumption: cosine→answerability is ~linear between the two anchors
  (fine for a gate, not a ranking). Adds a ~1-2 s calibration pass at reindex
  (N×2 cosine ops + N query embeds on a sample — negligible vs full embed).

**Recommendation: C.** It is the only option that satisfies "generic for every model,
present and future." B is explicitly rejected for needing a human per model.

## Implementation sketch (Approach C)

1. **Auto-calibration pass (sidecar, travsr-embed).** After HNSW build in `reindex`,
   run the label-free probe above → `{cos_lo, cos_hi}`. Store in **per-repo** embed
   metadata (a `calibration` row in `embed.db` meta) — the scale depends on model AND
   corpus, and `embed.db` is where reindex already writes and where the oracle reads.
   Recompute on every reindex (cheap). If skipped/absent → fall back to bge-small
   `{0.64, 0.77}` (identity for the reference model, safe for others until first reindex).
2. **Surface the anchors to `seed.rs`.** The oracle maps already flow into
   `classify_confidence`; carry `{cos_lo, cos_hi}` along the same path (two `f32`s,
   read from `embed.db` meta when the oracle is loaded). No new crate dependency —
   pass floats, not the catalog type, to respect crate dep rules
   (travsr-mcp must not import travsr-plugin-host internals).
3. **Answerability transform.** Add `fn answerability(cos, lo, hi) -> f32` and a
   `Calibration{lo,hi}` with `Default = {0.64, 0.77}`. Replace the raw-cosine
   comparisons in `classify_confidence` (seed.rs:415-496) and the `floor` in
   `semantic_validate` (seed.rs:596) with band-fraction comparisons on `a(cos)`.
   Pin the six fractions so bge-small defaults reproduce current cosine cutoffs
   **exactly** (assert in a unit test). `TRAVSR_*` env overrides stay, interpreted
   in cosine space pre-transform, for back-compat.
4. **Verify auto-calibration matches the hand numbers on bge-small.** Run the probe
   on the travsr repo under bge-small; assert measured `{cos_lo, cos_hi} ≈ {0.64,
   0.77}` (±0.03). This validates the label-free estimator against the values the
   floors were originally tuned to.
5. **Validate — no-regression gate.** Rerun BOTH benches:
   - travsr self-bench (bge-small): metrics **unchanged** (transform is identity).
   - k8s arctic-256: grounded hit@1 recovers L4+C4 (target ≥0.5 grounded; raw recall
     unchanged), abstain on N1–N3 must NOT drop below 2/3.
   - (When available) arctic-768 as a third auto-calibrated model — confirms the
     mechanism generalizes without touching code.
6. **Tests (seed.rs).** `answerability` identity at anchors; low-cosine synthetic
   model (lo=0.2,hi=0.4) no longer over-abstains on a mid-band cosine; nonsense
   (cos ≤ lo) still vetoes; env overrides still honoured.

## Non-goals
- Not changing KNN/PPR/knapsack. Not re-embedding beyond the tiny calibration probe.
  Not touching failure mode #2 (weak conceptual/cross recall — model/expansion issue,
  separate track).

## Risks
- **Bad auto-anchors → wrong floors.** Mitigated: p50/p95 (robust to outliers);
  identity check in step 4; abstain gate on N1–N3 in step 5. If `cos_hi ≤ cos_lo`
  (degenerate model), fall back to bge-small defaults and log a warning.
- **Sampling variance** across reindexes. Mitigated: N≈256, fixed RNG seed for the
  random-pair draw, percentiles not means.
- Calibration cost at reindex — bounded to N query embeds + N cosine ops; measured
  against total embed time (expected < 1%).
