# RFC-021 — Cross-Encoder Relevance Arbiter for Seed Selection

- **Status:** Draft — **Principal Architect end-to-end review complete (§15).** ML-in-core boundary amended & approved; in-process reranker ratified as v1; 1 blocker + 7 gaps resolved.
- **Authors:** Principal Architect + Solution Architect (personas)
- **Affects:** `travsr-rerank` (NEW crate, in-process `tract`), `travsr-mcp` (`seed.rs`, `tools.rs`, `query.rs`), `travsr-cli` (model bundling), `bench/` (negative set). travsr-embed sidecar rerank (#8) rescoped to a **future GPU/cloud variant**, not a v1 dependency.
- **Supersedes in part:** the bi-encoder confidence machinery of RFC-018 (embeddings) and RFC-019 (cosine oracle / contamination gate) — those become *optional recall*, not the relevance signal.
- **Related:** travsr-embed issue #6 (EmbedBackend/BackendFactory capability resolver) — this RFC rides that machinery.

---

## 1. Problem statement (evidence-based)

`get_context` returns confidently-labelled, irrelevant seeds for broad or off-topic natural-language queries. Reproduced on the travsr repo itself with a fully-built arctic-embed-m index (embeddings **on**):

```
query: "delete all user accounts and drop the database"
→ [retrieval: exact+lexical+semantic | coverage 3/5 | confidence: STRONG ]
  fn:Guard.drop            crates/travsr-daemon/src/lib.rs:3370   [score: 1.00]
  fn:SqliteStore.delete_file  crates/travsr-store/src/lib.rs:1540 [score: 0.82]
  ... 80 nodes
```

This query has no meaning against a code-graph daemon (no user accounts, no user DB), yet the system reports **STRONG** confidence over 80 nodes. Two independent defects combine:

### Defect A — false STRONG via coincidental generic-vocabulary anchors
`crates/travsr-mcp/src/seed.rs:595`, in `classify_confidence`:
```rust
} else if has_specific_anchor && coverage >= cov_strong {
    Confidence::Strong
```
Generic programming verbs (`delete`, `drop`, `get`, `insert`, `spawn`) are legitimate developer words that are *also* exact method names. On a code corpus their IDF is moderate, so they clear `idf_coverage_min` and count as "specific anchors." `embed_agrees` (seed.rs:520) is then satisfied because the anchor node **shares the literal query token**, which lexically inflates its cosine — a hazard the RFC-019 comment at seed.rs:552 explicitly acknowledges ("lexical-overlap-inflated cosine"). Three such coincidental anchors → `coverage ≥ cov_strong` → **STRONG**. The RFC-019 answerable-band guard (`confirm_anchor_floor`) is applied **only** to the low-coverage `embed_confirmed_specific` branch (seed.rs:604), never to this high-coverage branch.

### Defect B — per-node score is relative, not absolute
`crates/travsr-mcp/src/tools.rs:3149-3162`: displayed score is `raw / max_raw_score`, so **the top node is always `1.00` by construction** (the comment says so). Even a weak result set presents every node at ~0.9-1.0. A consuming LLM reads that as high confidence; the single `confidence: weak` header line is easy to miss.

### Why "on some repos"
Both defects are corpus- and model-dependent. A repo with more generic-verb method names yields more false anchors; per-model cosine calibration shifts the semantic gates. The behaviour is therefore inconsistent across repos — which is what the field report describes.

---

## 2. Root cause (architectural, not tuning)

The confidence gate approximates *reasoned relevance* using two **geometric** signals — BM25 term overlap and bi-encoder cosine. Both are token-overlap-driven. **Neither can distinguish "the user's words appear as symbol names" from "the user's intent matches this code."** No threshold tuning closes that gap; it only relocates the false positives.

- A **bi-encoder** encodes query and candidate *separately* into vectors and compares by cosine. The two texts never attend to each other, so for short code identifiers the vector is dominated by surface tokens (`drop`, `Pod`). RFC-019's own comment records BGE scoring `InterPodAffinity.RemovePod` at cosine **0.751** to query `GetWarningsForPod` purely on shared tokens — above any sane floor.
- The fix requires a model that encodes query and candidate **jointly** so it can judge intent, homonyms, and negation: a **cross-encoder**.

---

## 3. Design thesis

> Graph algorithms do all **structural** retrieval (exact-anchor, BFS/PPR, blast radius) — deterministic, zero-hallucination, unchanged. A single **cross-encoder** provides one thing the geometric stack cannot: an **absolute, model-agnostic relevance score** over the candidates the graph already retrieved. That score becomes the sole seed-selection / abstention signal.

The unified pipeline:

```
ALWAYS ON  ┌ exact-anchor (search_nodes_by_name) ┐
           │ lexical BM25 (search_nodes_fuzzy)    ├─► top-K candidates ─► CROSS-ENCODER ─► absolute score ─► confidence / ABSTAIN
           │ graph/PPR structural expansion       │      (K ≈ 20-40)      rerank(q, cand)   in [0,1]         one gate, no calibration
OPT-IN ····└ embedding KNN (recall booster)  ·····┘
```

`get_context`'s three-source RRF fusion (`build_seed_set`, seed.rs:896) already produces candidates **without** embeddings (`embed_knn: Option<_>`). The cross-encoder attaches to that always-on first stage. Embeddings are demoted from *relevance signal* to *optional recall source that adds more candidates into the same reranker*.

---

## 4. Principal Architect ruling (philosophy compliance)

The reranker is an ML model, so it is evaluated against the invariants.

| Invariant | Verdict |
|---|---|
| #3 — LLM/ML must not determine graph edges or structure | **UPHELD.** The cross-encoder never sees or produces an edge. It scores relevance of already-graph-retrieved candidate nodes for an NL query — the "translate queries / format results" allowance. All structural retrieval (exact-anchor, PPR, blast radius) stays pure-algorithm. |
| "Algorithms First, LLM Last" | **UPHELD.** Algorithms retrieve; the model only *ranks* the retrieved set. It cannot invent a node the graph did not surface. |
| #5 — token budget hard constraint | Unaffected; rerank runs before knapsack. |
| "Add vector embeddings to the stack" sign-off gate | This RFC *reduces* reliance on embeddings (demoted to optional). Net philosophy risk is **lower** than the status quo. |

**Ruling:** A cross-encoder used strictly as a post-retrieval relevance ranker is *more* aligned with Algorithms-First than the current bi-encoder-as-confidence-oracle design, because it removes probabilistic cosine from the *confidence* decision and replaces per-model calibration with one absolute floor. **Approved in principle**, subject to the acceptance criteria in §12.

---

## 5. Component architecture (Solution Architect)

### 5.1 Rides issue #6, does not fork it

Issue #6 introduces `EmbedBackend` + `BackendFactory` with a capability-based resolver (`can_run(family)`), a shared `encode.rs` (tokenize/truncate/pad), and per-target ORT/tract selection. The reranker is a **sibling backend**, not a new subsystem:

```rust
// travsr-embed
pub trait RerankBackend: Send + Sync {
    /// One batched forward pass over (query, candidate) pairs → relevance in [0,1].
    fn rerank(&self, query: &str, candidates: &[&str]) -> Result<Vec<f32>>;
    fn backend_name(&self) -> &str;      // "tract", "ort/CoreML", ...
    fn is_accelerated(&self) -> bool;
}
```

- Reuses `encode.rs` tokenization; the only structural difference from `EmbedBackend` is the input is a **pair** `[CLS] query [SEP] candidate [SEP]` (+ `token_type_ids`) and the output head is a **1-logit → sigmoid** instead of a pooled vector.
- Selected by the same capability resolver keyed on `family`. `TractFactory.can_run` already lists `bert`/`minilm`; the reranker is `family = "bert"` → runs on the default tract build **and** OCI aarch64, and is ORT-accelerated on CoreML/CUDA builds for free.

### 5.2 Model selection

**`cross-encoder/ms-marco-MiniLM-L6-v2`**, tagged `family = "bert"`.

| Property | Value | Why it wins |
|---|---|---|
| Architecture | standard BERT, 6 layers, 22.7M params | Only standard BERT runs on tract (issue #6). DeBERTa/JinaBERT/ModernBERT rerankers **fail at graph load** → unavailable on default + OCI builds. |
| Accuracy | NDCG@10 74.30 (TREC DL19) | Ties `L12-v2` (74.31); distilled *from* L12. |
| Throughput | ~1800 docs/s CPU | 2× `L12-v2`. |
| **Primary artifact** | `model_fp16.onnx` = **45.6 MB** | Under the 60 MB budget; best tract compatibility below fp32. |
| ORT-target artifact | `model_int8.onnx` = **23 MB** | int8 is solid on ORT; per-target artifacts already supported by #6. |
| Rejected | fp32 (91 MB, over budget); `L12` (fp16 ~66 MB over budget → forces risky int8-on-tract) | — |

Precision rule: **fp16 on tract targets, int8 on ORT targets.** tract's int8 QDQ support is the weakest path; fp16 avoids that while staying under budget. Both validated at build time (parity ≥ 0.999 vs fp32).

### 5.3 Sidecar protocol

New op over the existing `embed_sidecar.rs` line protocol (same framing as embed / cancel-sentinel):

```
REQUEST : {"op":"rerank","query":"<str>","candidates":["<str>", ...]}   // len ≤ K
RESPONSE: {"scores":[0.83, 0.02, 0.41, ...]}                            // aligned to candidates, [0,1]
```

- One process, one model load; reuses the spawned sidecar. No second sidecar.
- Candidate text = `signature + up to ~10 lines of the node body` (skeleton), **truncated to 256 tokens** — the dominant latency lever.
- Batched: all K candidates in one forward pass (batch dim = K).

---

## 6. `travsr-mcp` integration

### 6.1 `build_seed_set` (seed.rs:896)

After RRF fusion (seed.rs:1096) and before returning the `SeedSet`:

1. Take the top-K fused candidates (K = `rerank_topk()`, default 30).
2. Fetch each candidate's rerank text (signature + skeleton) — reuse the snippet extractor already used by `include_snippets`.
3. Call `rerank(query, candidates)` through the sidecar (guarded — see §8).
4. Attach the absolute score to each `Seed` as `Seed.rerank_score: Option<f32>`.
5. Re-order seeds by rerank score (exact-anchor priority from seed.rs:1231 still floats a *literally-named* symbol to the top; rerank orders the rest).

### 6.2 `classify_confidence` collapses (seed.rs:477)

The entire cosine/calibration lattice reduces to an absolute-score gate. New logic (embeddings-independent):

```rust
// r = max rerank score over candidates; None when reranker unavailable.
match r {
    Some(r) if r >= STRONG_FLOOR  => Confidence::Strong,   // e.g. 0.60
    Some(r) if r >= WEAK_FLOOR    => Confidence::Weak,     // e.g. 0.30
    Some(_)                        => Confidence::None,     // ABSTAIN — honest "no match"
    None => classify_confidence_lexical_fallback(...),      // reranker off: today's gate, unchanged
}
```

- Floors are **absolute and model-agnostic** — no `Calibration`, no per-corpus anchors, no `semantic_veto`/`confirm_anchor_floor`/`disjoint_rescue`. The cross-encoder score means the same thing on every repo and model.
- `"delete all user accounts and drop the database"`: `Guard.drop`/`delete_file` score low against the *intent* (destructive DB op vs. Rust destructor / graph-node deletion) → below `WEAK_FLOOR` → **abstain**. Bug closed at the root.

### 6.3 Per-node score fix (tools.rs:3149-3162)

Replace the relative `raw / max_raw` display score with the **absolute rerank score** when available. A node that reranked at 0.12 displays `[score: 0.12]`, not `[score: 1.00]`. When the reranker is off, keep today's normalization but **suppress `1.00` down-weighting** by prefixing the header confidence (Defect B mitigated even on the fallback path).

---

### 6.4 `travsr ask` CLI parity (query.rs) — same fix, second surface

`ask_query` (query.rs:191) runs its **own** pipeline parallel to `get_context_body`, but calls the **shared** `build_seed_set` (query.rs:236). So the fix reaches `ask` in two parts:

**Inherited for free (via the shared choke point):**
- Rerank-driven **seed reordering** and `Seed.rerank_score` (§6.1).
- The collapsed **absolute-floor confidence** (§6.2). `ask_query` *already* abstains on `Confidence::None` (query.rs:240) — so the moment `classify_confidence` uses the rerank floor, `ask "delete all user accounts and drop the database"` returns `matched: false` (honest no-match) instead of a confident table. No new abstain code needed; the existing guard simply becomes correct.

**`ask`-specific work (Defect B, second instance):**
- `ask` computes its **own** per-row score — raw PPR × degree-damping (query.rs:299-306) — surfaced as the CLI table's `Score` column (the meaningless `0.006`/`0.014` values). This must carry the **absolute rerank score** for seed rows, the same way §6.3 fixes the `get_context` display:
  - **Seed rows** (nodes that were reranked): `AskRow.score` = the node's `rerank_score` ∈ [0,1].
  - **Structurally-expanded rows** (added by `enrich_seeds_with_callers` / `dedup_adjacent_seeds`, query.rs:253-256 — callers/callees that were never reranked): keep the PPR-derived score but **cap it at the parent seed's rerank score**, and tag the row so the CLI can render it as `[via: caller of …]` rather than a peer relevance score. A structural neighbour must never display a higher relevance than the seed that pulled it in.
- `AskPayload` gains a `confidence: &'static str` field (mirrors `get_context`'s header) so the CLI prints `confidence: strong|weak` and downstream consumers (VS Code graph panel, which calls this path with `:`-prefixed queries per query.rs:197) get the honest signal. `matched: false` already covers abstain.

**Contract:** `ask` and `get_context` now share one relevance signal, one confidence definition, and one abstain rule — divergence between the two surfaces (a long-standing footgun) is eliminated by construction.

## 7. What gets deleted / demoted

Once the reranker is the arbiter, the following become dead weight on the *confidence* path (kept only inside the optional bi-encoder recall source, if at all):

- `Calibration{lo,hi}` model-relative floor mapping (RFC-019 / model-relative-floors) — **removed from confidence**.
- `semantic_veto`, `semantic_strong`, `embed_confirmed_specific`, `confirm_anchor_floor`, `disjoint_rescue_cos`, `anchor_neighborhood` contamination gate — **removed from confidence** (structural contamination is now moot because relevance is judged directly).
- The embed-on vs embed-off **branching** in `classify_confidence` — collapses to one path. This also removes a *source* of the "some repos" inconsistency.

Net: `seed.rs` confidence logic shrinks by an estimated ~40% and stops depending on embeddings being present or calibrated.

---

## 8. Failure modes & graceful degradation

| Condition | Behaviour |
|---|---|
| Reranker not installed (opt-in path) | `r = None` → today's lexical/cosine gate (`classify_confidence_lexical_fallback`). No regression. |
| Reranker installed (default-on path) | Absolute-floor gate; embeddings optional for recall only. |
| Rerank sidecar timeout | Circuit-breaker (mirror `knn_budget_ms`, e.g. `rerank_budget_ms = 250`) → fall back to lexical gate for that query; log `rerank_degraded`. |
| First-stage recall miss (pure paraphrase, no shared token, embeddings off) | Honest **miss** (`no grounded match`), never a confident wrong seed. Graph/PPR expansion partially recovers; opting into embeddings recovers the rest. |

Degradation is always toward **abstain / lexical**, never toward confident salad.

---

## 9. Recall vs precision (explicit)

- The reranker replaces the bi-encoder for **precision** and abstention. It does **not** provide **recall** — it can only rank candidates the first stage surfaced.
- **Recall sources, in order of always-on-ness:** exact-anchor + lexical BM25 (always) → graph/PPR structural expansion (always) → embedding KNN (**opt-in**).
- Therefore: **the reranker is bundled/default-on** (it is the correctness fix); **embeddings remain opt-in** (a recall luxury for vocabulary-mismatch conceptual queries). This is the one architecture; A/B were never a genuine fork once embeddings are recognised as optional.

---

## 10. Sequence (get_context, default-on reranker, embeddings opt-in)

```
client ──get_context(q, budget)──► get_context_body (tools.rs:2923)
   build_seed_set (seed.rs:896):
     exact-anchor  ─┐
     lexical BM25  ─┼─ RRF fuse ─► top-K
     [KNN if opted]─┘
     ── sidecar.rerank(q, top-K texts) ──► scores[]        (§5.3, guarded §8)
     ── attach Seed.rerank_score; reorder
     ── classify_confidence: absolute floor ─► Strong / Weak / ABSTAIN   (§6.2)
   if ABSTAIN → resolution map + speculative guesses (existing abstain_message path)
   else → PPR walk over seeds ─► kcore ─► knapsack(budget) ─► format
          per-node [score] = absolute rerank score                     (§6.3)
```

---

## 11. Performance budget

| Item | Cost |
|---|---|
| Model on disk | 45.6 MB (fp16 tract) / 23 MB (int8 ORT) |
| Resident RAM | ~40-80 MB (model + activations) |
| Added latency, K=30, 256 tok, CPU | ~20-60 ms (M-series / OCI A1 2-OCPU) |
| Accelerated (CoreML/CUDA) | lower; single-submit bucketed batch per issue #6 |
| Storage per node | **0** — nothing indexed (vs HNSW for embeddings) |

Fits the Solution Architect NFR (`Query latency P95 < 10ms local` is exceeded by rerank, so rerank is gated to `get_context` — the NL-query tool — not to the deterministic `get_callers`/`get_dependencies`/`get_blast_radius` tools, which never rerank).

---

## 12. Acceptance criteria

- [ ] `RerankBackend` trait + `rerank` sidecar op added in `travsr-embed`, selected by the issue-#6 capability resolver; `family="bert"` runs on default tract build and OCI aarch64.
- [ ] Default build compiles the MiniLM-L6 reranker (fp16); ORT builds use int8; parity ≥ 0.999 across backends.
- [ ] `build_seed_set` reranks top-K after RRF; `Seed.rerank_score` populated.
- [ ] `classify_confidence` uses the absolute-floor gate when rerank score present; the reproduction query `"delete all user accounts and drop the database"` returns **abstain / no grounded match**, not STRONG.
- [ ] A genuinely-relevant query (`"how is the knapsack budget optimizer implemented"`) still returns Strong with the correct seeds.
- [ ] Per-node display score is the absolute rerank score (no forced 1.00) when reranker is on.
- [ ] **`travsr ask` parity:** `ask "delete all user accounts and drop the database"` returns no-match (`matched: false`); `ask` `Score` column shows absolute rerank score for seed rows and capped/tagged scores for structurally-expanded rows; `AskPayload.confidence` populated and printed by the CLI.
- [ ] `ask` and `get_context` return the **same** confidence label and abstain decision for the same query (cross-surface parity test).
- [ ] Reranker-off path is byte-for-byte today's behaviour (fallback gate) on **both** surfaces.
- [ ] `Calibration` / contamination-gate code removed from the confidence path; embed-on/off branching collapsed.
- [ ] Circuit-breaker degrades to lexical gate on rerank timeout.
- [ ] Bench harness (bench/) hit@1 / usefulness re-run: precision up on off-topic + broad queries; no recall regression on symbol queries.

## 13. Decisions (locked)

1. **Reranker is default-on / bundled.** ✅ Decided. The ~45 MB MiniLM-L6 reranker ships with the binary so the confident-salad bug is fixed for every user out of the box; embeddings remain opt-in (recall luxury). The lexical-only fallback gate is retained solely for the rerank-timeout / degraded path (§8), not as a user-selectable mode.
2. **Floors measured from the bench harness before ship.** ✅ Decided. `STRONG_FLOOR` / `WEAK_FLOOR` are authored empirically from `bench/` on a labelled query set (positives = on-topic symbol + conceptual queries; negatives = the off-topic/generic-vocabulary set incl. the reproduction query). No provisional constants ship. Pick the boundary that maximises abstention on negatives while holding recall on positives; record the chosen values + the ROC point in the RFC before merge. (Consistent with measure-before-claiming.)

   **Measured 2026-07-17** (`crates/travsr-mcp/src/seed.rs::rerank_floor_sweep`, `bench/queries.json` N=27: 16 positive / 11 negative — a repo-self-dogfood calibration sample, not a statistically powered study; revisit with real usage telemetry). Measured with embeddings **excluded** from candidate recall (matches G2 — the reranker's own signal, independent of embed availability):
   - All 11 negatives (3 nonsense + 8 salad, including the reproduction query G1) scored ≤ **0.0021**.
   - All 6 literal + 3/8 conceptual + 1/2 cross positives that were actually recalled by the lexical/anchor candidate pool scored ≥ **0.9288**.
   - The remaining 5 conceptual/cross positives (C2–C6, X2) also scored near-zero — traced to a **recall gap, not reranker miscalibration**: "conceptual" queries have no literal symbol match by definition, so without embedding-based recall the truly relevant node never entered the reranked candidate set at all. This is consistent with §9 (embeddings are the recall source for vocabulary-mismatch conceptual queries; the reranker only judges relevance of whatever was retrieved). Re-running this sweep with embeddings wired into recall is a recommended follow-up validation before wide rollout, not a Phase 3 blocker.
   - Given the clean separation on the signal that matters (0.0021 max-negative vs. 0.9288 min-successfully-recalled-positive), chosen with a wide safety margin rather than hugging the ROC boundary: **`WEAK_FLOOR = 0.5`**, **`STRONG_FLOOR = 0.8`**.
3. **Code-domain fine-tune (later).** ms-marco is NL-trained; a CodeSearchNet-style head fine-tune is a follow-up optimization, not an MVP blocker.

## 15. Principal Architect — end-to-end review & rulings

Reviewed 2026-07-17. The ML-in-core boundary was raised to the CTO/Principal level and **amended**.

### Ruling 0 — ML-in-core boundary amended (approved)
> ML is permitted in the core repo **only** for *post-retrieval relevance ranking of candidates the graph already produced*. It may never determine an edge, node relationship, or structural dependency — **Invariant #3 stands untouched.** The bar a ranking model must clear: its score carries **genuine, abstention-capable weight** (the system can say "nothing is relevant"), never a confident salad. The governing principle is **output credibility, not runtime location.**

The cross-encoder qualifies (ranks graph-retrieved candidates; absolute score with a real abstain floor). Runtime location is therefore free to optimise for availability.

### Decision 1 — one `Reranker` trait; in-process is v1, sidecar is a future variant
The "in-process vs sidecar" fork is dissolved:
- New leaf crate **`travsr-rerank`** defines a `Reranker` trait. `TractReranker` (pure-Rust, CPU, **in-process**) is the **v1 default**, shipped in the main binary. A sidecar-backed impl is a **later variant** for GPU/cloud.
- **travsr-embed #8 is rescoped** from "v1 dependency" to "future GPU/cloud rerank variant." The confident-salad fix ships **without** the #6 ORT epic.
- In-process CPU `tract` is **deterministic** (fixed accumulation order) → preserves "same query, same result"; the future GPU path is not bit-identical (#6), so v1 is the more invariant-safe option.

### Gap register (resolved)

| # | Gap | Decision |
|---|---|---|
| **G1 (BLOCKER)** | Cross-encoder is NL-trained → would **false-abstain on exact-symbol queries** (`GetWarningsForPod` scores low), trading salad for missed precise lookups. | **Reranker arbitrates NL/conceptual queries ONLY.** When `build_seed_set` resolves a `SeedSource::Exact` anchor (user named a real symbol), confidence is driven by the **deterministic exact match**; the reranker is bypassed for that decision (may still reorder the tail). The deterministic path stays deterministic. |
| G2 | Embeddings-on must not change the confidence *decision*. | Embedding KNN is a candidate **source only** (raw cosine × kind_boost into RRF) → affects **recall, never confidence**. `Calibration`/`semantic_veto`/`confirm_anchor_floor`/`disjoint_rescue`/contamination-gate deleted from the confidence path. **Amended 2026-07-18** (post-implementation review F7): precisely, the confidence *gate* reads no embedding signal — for an identical candidate set, confidence is identical embed-on/off. Embeddings can still change *which* candidates reach the top-K reranked set (recall), and hence `max_rerank_score`, hence the decision — that is by design (recall affects what gets judged; the judge is embed-independent), but it means "identical confidence embed-on/off" holds per-candidate-set, not universally across recall changes. |
| G3 | `tract` must not leak toward `travsr-core`. | `travsr-rerank` depends only on `travsr-core` (`NodeId`) + `tract`; `travsr-mcp` depends on `travsr-rerank`. `travsr-core`/`travsr-retrieval` stay ML-free. No crate-rule violation. |
| G4 | 45 MB model distribution for a default-on component. | Model ships **alongside the platform binary in GitHub Releases**, pulled by the existing binary-fetch (npm wrapper / brew). Offline installs get it with the binary; no npm-tarball bloat. `TRAVSR_NO_RERANK=1` → lexical fallback for minimal installs. |
| G5 | In-process loses sidecar process isolation. | (a) inference on a dedicated **bounded blocking (HEAVY) pool**, never the async/MCP thread; (b) `catch_unwind` → a model panic degrades one query to the lexical gate, never crashes the daemon; (c) bounded K ≤ 40, seq ≤ 256; (d) **fail-open** on model-load failure. |
| G6 | Cold-start latency / MCP-init hang risk. | **Background-warm** at daemon start on a non-blocking thread (reuse the `embed_ready` lazy-init lesson). Pre-warm queries use the lexical gate + "warming" note; `rerank_budget_ms` circuit-breaker covers contention. |
| G7 | Floors are a *negative*-set property; bench measures positives only. | Extend `bench/` with a labelled **negative set** (off-topic + generic-vocab incl. the reproduction query). Floors = ROC point maximising negative-abstention s.t. positive-recall ≥ target. **Model version + floors pinned together** in the model manifest (atomic bump — no calibration drift). |
| G8 | Candidate text truncation + RBAC ordering. | Candidate text = **signature-first** + skeleton, truncated to 256 tok (truncation drops body, not signature; reuse `include_snippets` extractor). Rerank runs **after** `filter.allow` (already true — seeds RBAC-filtered before the hook), so restricted nodes are never scored. **Amended 2026-07-18** (see §16): the 256-token tokenizer truncation alone was not repo-agnostic — a char-level pre-cap (`travsr-rerank::MAX_CANDIDATE_CHARS = 480`) was added ahead of it after cross-repo E2E testing on kubernetes/kubernetes exposed a real cost blowup. |

### Net architecture (post-review)
`travsr-rerank` (leaf, `tract`, in-process, deterministic) → consumed by `travsr-mcp` seed selection → **arbitrates NL queries only; exact-symbol queries stay deterministic (G1)** → one absolute-floor confidence, embeddings-independent (G2) → fail-open, background-warmed, model+floors pinned (G5-G7). Ships in the main binary via the platform release (G4). Sidecar rerank (#8) is the future GPU/cloud variant behind the same `Reranker` trait (Decision 1).

### Amended acceptance criteria (additions to §12)
- [ ] `travsr-rerank` crate with `Reranker` trait + `TractReranker`; `travsr-core`/`travsr-retrieval` remain ML-free.
- [ ] Exact-symbol query (`SeedSource::Exact` present) is **not** governed by the rerank floor — deterministic exact-match confidence preserved (G1 regression test).
- [ ] Confidence gate reads no embedding signal; for an identical candidate set, confidence is identical embed-on/off (G2).
- [ ] Daemon survives a forced inference panic (fail-open to lexical gate); model-load failure does not break startup (G5).
- [ ] `bench/` negative set added; floors recorded with model version in the manifest (G7).

## 16. Cross-repo E2E validation (kubernetes/kubernetes, 2026-07-18)

Phases 1-3 were built and floor-calibrated (§13.2) entirely against travsr's own repo. Before sign-off, Phase 3 was re-run end-to-end against `kubernetes/kubernetes` (262k nodes) — a repo an order of magnitude larger, in a different language, with much denser functions — using a matching `salad` query set (generic CRUD verbs — `Delete`/`Get`/`Sync`/`Watch`/`Insert`/`Create`/`Update`/`List` — that coincidentally collide with real k8s symbols, e.g. `DeletePod`, `GetSecret`, `SyncPod`).

**Result:** no query reproduced the original bug (false `Strong` confidence on an off-topic query) — the worst case observed was `Weak`. No regression on genuine literal/conceptual hits; several (`L3`, `L4`, `X2`) that had been silently falling back to lexical-only actually **improved** once the fix below let rerank complete within budget.

**Real bug found and fixed — candidate-size cost blowup.** The circuit-breaker (§8, then 600ms) fired on nearly every k8s query, measured at 2.5-9s per rerank call (vs ~353ms for the same K=30 on travsr's own repo) — silently discarding the score and falling back to lexical every time, i.e. Phase 3 was a no-op on this repo in practice. Root cause, traced by reading the actual code path, not guessed: candidate text is `signature + snippet_for_node(..)` (up to 40 raw source lines, uncapped by language); 40 lines of dense Go is far more tokens than 40 lines of idiomatic Rust, routinely exceeding the tokenizer's 256-token truncation ceiling on k8s and rarely on travsr. Because tokenization used `PaddingStrategy::BatchLongest`, one over-length candidate in a batch padded every *other* candidate in that batch to the same length — a systemic effect once any candidate in a chunk crossed the ceiling, not an occasional outlier.

The fix bounds the input rather than tolerating the output cost: `travsr-rerank::MAX_CANDIDATE_CHARS = 480` (a fixed, non-env-tunable constant, enforced inside the crate so no future call site can reintroduce the bug) truncates each candidate to ~120 tokens before it ever reaches the tokenizer — independent of the source repo's language or function size. `ms-marco-MiniLM-L-6-v2` is a general MS MARCO passage-relevance model trained on ~50-80 word passages, never meant to consume raw function bodies; for RFC-021's coarse "is this even topically related" triage, the signature plus a handful of body lines carries the signal.

Post-fix, measured k8s cost dropped to 700ms-4.7s total `get_context` latency (rerank-only cost 700-950ms, down from 2.5-9s), and the circuit-breaker firing rate dropped from ~100% to 0/27 queries after also raising `rerank_budget_ms` default 600 → **1200** (real headroom over the new measured worst case — a backstop, not the primary defense; the primary defense is the input-size bound, which is what makes the budget number now hold across repos rather than needing per-repo retuning).

Two residual, informational findings — not regressions, not blockers:
- Three salad queries (`get the customer's shopping cart`, `watch a movie trailer`, `update my profile picture`) score in the `Weak` band on k8s rather than abstaining outright. Confirmed via direct measurement this is a genuine model judgment (not a budget-breaker artifact — identical result with the breaker disabled): k8s has real `AppArmorProfile`/`SeccompProfile`/watch-API vocabulary that legitimately overlaps with the salad query's words. This is the §13.2 floor-generalization caveat (floors measured on an 11-negative travsr-only sample) manifesting cross-repo, not a new defect.
- Rejected raising the circuit-breaker budget to cover the pre-fix k8s worst case (9-10s) as the primary fix: a wall-clock number tuned to one repo's observed cost is not an invariant (a denser repo blows past any fixed number too), and a multi-second synchronous MCP tool call is bad interactive UX regardless. Bounding candidate input size is the fix that actually generalizes; the wall-clock breaker is deliberately demoted to a last-resort backstop.

## 14. Out of scope

- Fine-tuning the reranker on code pairs (follow-up).
- MCP-sampling LLM judge (a higher-quality ceiling; separate RFC — this one is the local, always-available floor).
- Reranking the deterministic structural tools (`get_callers`, etc.) — they need no relevance ranking.
