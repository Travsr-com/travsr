# Issue #393 — Retrieval cascade short-circuit + top-K starvation: RRF fusion redesign

**Status:** Plan (not started)
**Author:** Tech Lead
**Date:** 2026-07-04
**Crate(s) affected:** `travsr-store` (primary), `travsr-mcp` (call-site verification only), `bench/` (A/B)
**Issue:** https://github.com/Travsr-com/travsr/issues/393
**Size:** L (3–4 days) — RFC-sized because it touches the core retrieval contract, but the fix is localised to one module.

---

## 1. Problem (live-confirmed on this repo, 2026-07-04)

```
travsr ask "workflow"   → 12 rows, ALL functions in crates/travsr-analysis/src/data_format.rs
                          0 of the .github/workflows/*.yml files
travsr ask "workflows"  → 14 rows, ALL .github/workflows/*.yml files
                          0 code symbols
```

The singular and plural of the same word return **completely disjoint** sets, and the
singular query returns **zero** of the workflow files — the single most on-topic artifact
in the repo for that word. This is the "confident but incomplete context" failure the
retrieval layer exists to prevent.

## 2. Root cause — three compounding mechanisms

All three live in the fuzzy-search cascade in `crates/travsr-store/src/lib.rs`.

### 2.1 Early-return short-circuit (structural)
`search_nodes_fuzzy` (lib.rs:2847) is a **first-nonempty-stage cascade**:

```
Step 1  search_nodes_by_name   exact substring (signature/path LIKE '%q%')   → if non-empty RETURN
Step 2  fts_query_nodes        FTS5 BM25 on T0 token union                   → if non-empty RETURN
Step 3  expand_query + FTS5    L2-A vocabulary-grounded expansion            → if non-empty RETURN
Step 4  embed_knn_hook         semantic ANN (RFC-018)                        → if non-empty RETURN
```

The first stage that returns *anything* wins and the rest never run. A partial Step-1
match blocks the FTS/embed stages that would have surfaced the missing nodes.

### 2.2 Asymmetric substring match (no stemming)
Step 1 is `signature LIKE '%q%' OR path LIKE '%q%'`.
- `"workflow"` ⊂ path `.github/workflows/…` **and** ⊂ fn name `…_workflow_…` → **both** match.
- `"workflows"` ⊂ paths only, **not** ⊂ `…_workflow_…` → code symbols are a hard non-match.

There is no singular↔plural normalization anywhere in the pipeline.

### 2.3 Rank + top-K starvation (kind penalty)
The Step-1 rank formula (lib.rs:2999–3033, mirrored in `_with_lang`) adds a per-`kind`
penalty: `function`/`method` = **0**, and files fall into `ELSE 4` (plus a base `ELSE 40`
for signature-miss and a `LENGTH(path)/32` length penalty). So for `"workflow"` the `.yml`
files *are* retrieved (LIMIT 100, no SQL truncation) but sort **behind** the ~12 function
nodes, which saturate the visible top-K. The files are retrieved-then-hidden.

## 3. Divergent implementations (must be unified)

There are **three** public cascade entry points with **inconsistent** stage coverage — a
latent correctness bug independent of #393:

| Function | lib.rs | Returns | Stages | Caller |
|---|---|---|---|---|
| `search_nodes_fuzzy` | 2847 | `Vec<Node>` | 1,2,3,4 | `ask` fallback — `travsr-mcp/tools.rs:2125` |
| `search_nodes_fuzzy_scored` | 3127 | `Vec<(Node,f32)>` | 1,2,3 (**no embed**) | `get_context` seed — `travsr-mcp/seed.rs:976` |
| `search_nodes_fuzzy_filtered` | 3254 | `Vec<Node>` | 1,2,3,4 (+lang) | `search_symbol` — `travsr-mcp/tools.rs:907,920` |

Shared helpers: `search_nodes_by_name` (1584) / `_with_lang` (2987), `fts_query_nodes`
(2943) / `_scored` (3073) / `_with_lang` (3203), `expand_query` (3352).

**Design decision:** collapse all three onto a single scored, optionally-lang-scoped core
so the fusion logic and guards live in exactly one place.

## 4. Design — Reciprocal Rank Fusion (RRF) replaces short-circuit

### 4.1 Why RRF, not score normalization
The stages emit **incomparable scales**: Step 1 is an integer SQL rank *penalty*
(lower = better), Step 2/3 are `bm25()` (negative floats, corpus-dependent), Step 4 is
cosine (0..1). Min-max/z-score fusion is fragile with single-result stages and outliers.
RRF is rank-based, score-scale-agnostic, deterministic, and rewards nodes that appear in
multiple retrievers (a workflow file that hits both substring **and** FTS):

```
RRF(d) = Σ_i  w_i / (k + rank_i(d))          k = 60,  rank_i is 1-based, tie-break on node id
```

Per-stage weights (starting point, tune on bench): `w_exact = 2.0`, `w_fts = 1.0`,
`w_l2a = 0.7`, `w_embed = 1.0`.

### 4.2 Control flow (replaces the cascade in the unified core)

```
fn fused_search_scored(query, lang_filter) -> Vec<(Node, f32)>:
    stage1 = search_nodes_by_name[_with_lang](query)          # ranked, cheap
    # GUARD 1 — confident-exact fast path
    if stage1.first() is an exact-signature hit (rank == 0):
        return stage1 scored                                  # crisp symbol lookup, unchanged behaviour
    stage2 = fts_query_nodes[_with_lang](T0(query))           # cheap
    fused  = RRF([stage1, stage2])                            # ALWAYS union the two cheap SQLite stages
    if is_weak(fused):                                        # GUARD 2 — gate the costly stages
        stage3 = l2a_expand(query)                            # cheap-ish (vocab scan)
        stage4 = embed_knn_hook(query)  if present            # ONLY here — never on every ask
        fused  = RRF([stage1, stage2, stage3, stage4])
    return diversify_topk(fused, per_kind_cap)                # GUARD 3 — no kind can zero out another
```

### 4.3 The three non-negotiable guards

- **G1 Confident-exact fast path.** If Step 1's top row is a rank-0 exact `signature`
  match, short-circuit and return Step 1 only. Exact symbol lookups (`travsr ask
  handle_tool_call`) stay crisp and cannot be diluted by fuzzy neighbours. Preserves TL3
  ("exact never regresses").
- **G2 Gate the embed stage.** Always union the two cheap SQLite stages; fire L2-A + the
  embed-sidecar KNN **only** when the cheap fused union `is_weak`. Running embeddings on
  every `ask` re-introduces the known embed-KNN latency wedge
  ([[project_embed_knn_latency_fix]]). `is_weak` = `fused.len() < WEAK_MIN_RESULTS (=5)`
  **OR** `top RRF score < WEAK_SCORE_FLOOR`. (Measure-first: warm KNN ~0.02s, cold-spawn
  is the risk — see [[project_measure_before_claiming]].)
- **G3 Top-K diversity.** Interleave/round-robin by `kind` bucket with a per-kind cap
  (`ceil(K * 0.6)`) so functions can't starve out files and vice-versa. This is the direct
  fix for the reported disjointness.

### 4.4 Singular↔plural (separate, optional lever)
Light stemming or a synonym-table entry (`workflow`↔`workflows`). Deferred to a follow-up
issue — RRF + diversity already surfaces both sets when either query is issued because the
files are retrieved via FTS/substring on the path regardless of the trailing `s`. Track as
a nice-to-have, not a blocker for closing #393.

## 5. Scope & file-level changes

**In scope — `crates/travsr-store/src/lib.rs` only:**
1. New private `fused_search_scored(&self, query, lang_filter: Option<&str>, include_embed: bool) -> Result<Vec<(Node, f32)>>` — the single fusion core (RRF + G1/G2/G3). `include_embed=false` for the get_context path (§5.1); `true` (gated) for `ask`/`search_symbol`.
2. New private helpers: `rrf_fuse(stages: &[Vec<Node>], weights) -> Vec<(Node, f32)>`, `is_weak_union(&fused) -> bool`, `diversify_topk(fused, cap) -> Vec<(Node, f32)>`.
3. Rewrite the three public entry points as thin adapters over the core:
   - `search_nodes_fuzzy` → `fused_search_scored(q, None).map(node)`
   - `search_nodes_fuzzy_scored` → `fused_search_scored(q, None, include_embed=false)` — **in scope (decided 2026-07-04), with two guards from §5.1.** This is the `get_context` seed path (`travsr-mcp/seed.rs:976` `lexical_scored`). It gets RRF fusion of Stages 1–2 + diversity (the actual #393 fix for seed poisoning) but **must not** add the embed stage here — get_context already has a dedicated KNN seed channel — and **must preserve BM25-scale scores** for the abstention floor. See §5.1.
   - `search_nodes_fuzzy_filtered` → `fused_search_scored(q, lang)` (None short-circuits to unfiltered as today)
4. Extract a `search_nodes_by_name_ranked_scored` that exposes the SQL `rank` int per row (needed for G1 rank-0 detection + RRF rank ordering). Reuse existing SQL; add the `rank` column to the returned tuple.
5. New constants at module top: `RRF_K=60`, `WEAK_MIN_RESULTS=5`, `WEAK_SCORE_FLOOR`, per-stage weights, `PER_KIND_CAP_FRACTION=0.6`.

**Out of scope (do not touch):** the Step-1 rank SQL formula (keep it as the *within-stage*
order feeding RRF), `get_context` PPR/knapsack, embed sidecar internals, stemming.

### 5.1 get_context seed fold-in — two guards (verified in `travsr-mcp/src/seed.rs`)

Folding `search_nodes_fuzzy_scored` onto the shared core is **in scope**, but `get_context`
seeding (`assemble_seeds`, seed.rs:868) is not a naive consumer. Two facts from the code
constrain the fold-in:

1. **get_context already has a dedicated KNN seed channel.** `assemble_seeds` receives
   `knn_pairs` separately (seed.rs:868) and builds `knn_raw` from them (seed.rs:1029),
   independent of the lexical path. Adding the gated embed stage *inside* the fused lexical
   result would embed-KNN **twice** for get_context (double-counted seeds + a redundant
   sidecar call + latency). → **The core takes `include_embed`; get_context passes `false`.**
   The lexical path contributes RRF-fused Stages 1–2 (+ L2-A when weak); the embed signal
   stays single-sourced through the existing `knn_pairs` channel.

2. **The lexical score feeds a BM25-scale abstention floor.** Downstream, `top_bm25`
   (seed.rs:978) drives per-batch normalization (`bm25/max_bm25`, seed.rs:1005) **and** a
   relevance-floor abstention gate (`top_bm25 >= bm25_floor`, seed.rs:618). Raw RRF scores
   are tiny (~0.03) and on an incomparable scale — returning them directly would silently
   break `bm25_floor` (the R1 relevance-floor logic, [[project_context_quality_gaps]]).
   → **`search_nodes_fuzzy_scored` must keep emitting BM25-comparable scores**: RRF decides
   *ordering/inclusion*, but each returned row carries its underlying FTS `-bm25()` value
   (Step-1-only rows keep their existing descending synthetic score). Normalization is
   per-batch relative, so ordering changes are safe; the absolute floor stays calibrated.
   A test asserts `top_bm25` for a known grounded query stays in the historical band.

**Blast radius (verified via `travsr graph` + call-site scan):** the three functions are
only called from `travsr-mcp` (`search_symbol`, `get_context` seed, `ask` fallback) and
tests. No `travsr-retrieval`/`travsr-daemon` direct callers. Public signatures are
**unchanged** (return types preserved), so no cross-crate churn.

## 6. Rollout (measure-first, per issue + [[project_tool_benchmark]])

- **Phase 1 — Stages 1–2 RRF + G1 + G3, no behavioural embed change.** Zero added latency
  (both stages pure-SQLite). This alone fixes the reported `workflow`/`workflows` bug.
  Land first behind the unified core; embed stage stays gated exactly as the old cascade
  reached it (empty-only), i.e. G2 initially uses the old "Steps 1–3 empty" trigger.
- **Phase 1 also folds in get_context** (§5.1): route `search_nodes_fuzzy_scored` through
  the core with `include_embed=false` and BM25-scale scores preserved. Verify with the
  `bench/` get_context harness ([[project_tool_benchmark]]) that hit@1/hit@10 and the
  abstention rate do **not** regress — the seed set should only improve (no more disjoint
  seed poisoning), and the `bm25_floor` behaviour must be unchanged.
- **Phase 2 — Loosen G2 to `is_weak` (few/low-score results, not only empty).** A/B on
  `bench/queries.json` + `bench/queries-k8s.json`: gate must not regress warm p50 latency
  and must improve hit@k on multi-kind/conceptual queries before it becomes default.
- **Phase 3 (optional/follow-up).** Singular↔plural stemming lever; re-tune RRF weights
  from bench sweep.

## 7. Test plan (`crates/travsr-store/tests/fuzzy.rs`)

Regression + new:
- **#393 repro:** `ask "workflow"` returns ≥1 `.github/workflows/*.yml` file **and** ≥1
  code symbol; `ask "workflows"` likewise returns both kinds. Assert the two result sets
  **overlap** (no longer disjoint).
- **G1:** exact `search_nodes_fuzzy("handle_tool_call")` still returns the exact node first,
  no fuzzy dilution (existing fuzzy.rs:367 must stay green).
- **G3 diversity:** a query matching many functions + few files surfaces ≥1 file in top-K.
- **G2 gating:** with `embed_knn_hook = None`, a weak cheap union returns the cheap results
  (no panic, no hang); embed hook is invoked **zero** times on a strong exact hit
  (spy hook asserts call count).
- **Parity:** `search_nodes_fuzzy` / `_scored` / `_filtered(None)` return the same node set
  for the same query (unification invariant), modulo `_scored` carrying BM25-scale scores.
- **get_context score contract (§5.1):** `search_nodes_fuzzy_scored` top score for a known
  grounded query stays in the historical BM25 band (not collapsed to RRF ~0.03), so the
  downstream `bm25_floor` abstention gate is unchanged. Assert with a fixed corpus fixture.
- **get_context no double-embed (§5.1):** with a spy embed hook, `assemble_seeds` invokes
  the sidecar KNN exactly once (via `knn_pairs`), never a second time through the lexical
  core (`include_embed=false`).
- **Determinism:** repeated calls return identical order (RRF tie-break on node id).
- **Lang filter:** `_filtered(Some(lang))` returns only that language (existing invariant).

**Gates before commit (per [[feedback_ci_local_before_push]]):** `cargo fmt`, `cargo clippy
-p travsr-store`, `cargo test -p travsr-store`, `cargo test -p travsr-mcp` (callers),
`cargo deny`. Then build the release binary and **re-run the live `ask "workflow"` /
`ask "workflows"` repro end-to-end** ([[feedback_test_locally_before_commit]]) before the
user tests.

## 8. Risks & mitigations

| Risk | Mitigation |
|---|---|
| Embed KNN latency wedge re-introduced | G2 keeps it gated to weak-union; Phase-1 keeps old empty-only trigger; A/B before loosening |
| Exact-lookup precision regresses | G1 rank-0 fast path preserves current behaviour; parity test locks it |
| RRF weights overfit to this repo | Tune on `bench/` (both this repo + k8s corpus), not by eye |
| Silent divergence returns | Unification invariant test forces all three entry points to agree |
| get_context double-embeds seeds | `include_embed=false` on the get_context path; spy-hook test asserts single sidecar call |
| get_context abstention floor breaks | `_scored` keeps BM25-scale scores (RRF orders, doesn't rescore); `top_bm25`-band test |

## 9. Open questions
- `WEAK_SCORE_FLOOR` value — derive from bench score distributions, not a guess (RRF top
  scores are small: `~2.0/(60+1) ≈ 0.033` for a dual-stage rank-1 hit). Likely gate on
  **result count** primarily and treat score-floor as secondary.
- Should G3 per-kind cap be absolute or fractional at very small K (K<5)? Default:
  fractional with a floor of 1 per present kind.

---

## 10. Implementation outcome (2026-07-04) — what actually shipped

Implemented on `fix/travsr-store-393-rrf-fusion` (uncommitted, awaiting user test).

**10.1 Store layer (Phase 1) — as designed.** `search_nodes_fuzzy` / `_scored` /
`_filtered` unified onto `fused_search_scored(query, lang_filter, include_embed)` with
G1 (exact fast path), G2 (RRF-fuse cheap stages, gate L2-A/embed to combined miss), G3
(round-robin kind diversity). `_scored` (get_context path) passes `include_embed=false`,
BM25-scale scores preserved. 3 new `issue393_*` fuzzy tests.

**10.2 The headline `ask`/`get_context` repro needed a second fix, in `seed.rs`.**
Measured that Phase 1 alone did **not** fix `ask "workflow"`: that path (`build_seed_set`)
never ranks raw fuzzy output — it uses per-token anchors + a structural scope gate. Root
cause was three-channel (exact kind-bias, lexical scope gate, semantic excludes file
nodes / #391 P2). Fix applied:
- **Score-aware scope gate** (`build_seed_set`, seed.rs): an out-of-scope lexical seed is
  dropped only when its per-batch normalised score `< scope_strong_floor()` (default 0.3,
  env `TRAVSR_SCOPE_STRONG_FLOOR`, ≥1.0 = old hard gate). Strong lexical evidence overrides
  structural scope → multi-domain queries work; weak drift stays gated. Generic, no strings.
- **Stopword gap** (`seed_lexicon.rs`): added filler verbs `works`/`uses`/`runs` to
  `STOPWORDS` (kept sorted). Measured that neither score (inWorkspace norm ≥0.95 = workflow
  files) nor IDF (`works` 0.665 vs `workflow` 0.688) separates the legit multi-domain match
  from filler-word noise — the noise is a stopword problem, orthogonal to scope. +2 tests.

**10.3 Validation (all measured).**
- `get_context("workflow")`: 12 fns → **12 fns + 14 workflow files + 22 CI-action packages**.
  `workflows` overlaps (files+packages). `how ppr works`: 16 fns, **0 noise**. Grounded
  controls (`knapsack`, `handle tool call`, `rbac`) unchanged.
- **Bench neutral (zero regression):** `bench/run.mjs` master-vs-fix on the same local index
  → byte-identical (hit@1 0.438 / hit@3 0.625 / hit@10 0.688 / abstain 2/3). Scope-gate A/B
  (floor 2.0 vs 0.3) also identical — the gate only fires on multi-domain queries (not in the
  labeled set), so it fixes the repro without moving hit@k.
- Gates: fmt, clippy clean; store 124+47+2, mcp 203+25 all green.

**10.4 Deferred / tracked.** #391 P2 (embed `file`-kind nodes) remains the principled
structural fix that would let the semantic channel surface data-format nodes directly and
reduce reliance on the scope heuristic. `SCOPE_STRONG_FLOOR` default (0.3) is env-tunable for
future bench sweeps on a larger multi-domain query set.
