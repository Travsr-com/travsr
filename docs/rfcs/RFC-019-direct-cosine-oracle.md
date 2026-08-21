# RFC-019: Direct-Cosine Oracle and Seed Re-Rank

**Status:** DRAFT SKELETON — reconstructed from code citations, NOT yet authored
**Author:** _(TBD)_
**Date:** _(TBD)_
**Crates affected:** `travsr-mcp`, `travsr-store`, `travsr-daemon`, `travsr-plugin-host`
**Depends on:** RFC-018 (embedding plugin architecture)
**Issue:** #536

---

> ### ⚠️ How to use this file
>
> This is **scaffolding, not a finished RFC.** No RFC-019 document was ever found in
> `docs/`, but the code carries 42 citations across 9 files, and the comments at those
> sites contain most of the reasoning already. This file groups them into sections and
> quotes the source so an author can assemble rather than invent.
>
> **Before writing anything, do the search in #535 Step 0.** If the real document exists
> on a branch or in an unmerged PR, land that instead and delete this file.
>
> Each section below carries its `travsr-*/src/…:line` citations. Read the surrounding
> five lines at each — the rationale is usually in the comment body, not the cited line.
>
> Delete this box before submitting.

---

## Summary

_(TBD — one paragraph.)_

Roughly: embeddings moved out of `graph.db` into a sibling `embed.db` owned by the embed
sidecar, and a **direct-cosine oracle** was added beside the existing KNN hook. Where KNN
answers *"what is near this query?"*, the oracle answers *"how near is **this specific
node** to this query?"* — which lets seed selection MEASURE a named anchor instead of
inferring from its absence in a KNN result set.

---

## Motivation

_(TBD.)_

The behavioural defect this fixes is stated plainly at `travsr-mcp/src/seed.rs:1133`:

> RFC-019: absence is no longer disagreement. With the direct-cosine oracle we MEASURE
> every specific anchor; an anchor's node that is absent from the oracle *after* we tried
> to score it (`scored_ids`) is genuinely unscoreable (no stored vector / degraded) →
> "unknown", not "the model rejects it" → we fall back to lexical evidence (agrees). Only
> an anchor that is present and below floor counts as disagreement.

Before this, an anchor missing from the KNN neighbour set was read as the model rejecting
it. That conflates *"the model scored this low"* with *"we never scored it"* — two very
different signals for the abstention gate.

**Cite the measurement that motivated it.** `seed.rs:240` references a "0.751 collision"
and `seed.rs:2452` references `GetWarningsForPod` evidence; both look like they came from
a specific investigation worth naming here.

---

## Detailed Design

### D1 — Storage split: `node_embeddings` moves to `embed.db`

**Citations:** `travsr-store/src/lib.rs:380, 633, 1183, 1234` ·
`travsr-plugin-host/src/embed_sidecar.rs:121, 259`

Migration **v17** (`store/lib.rs:380`):

> `graph.db` drops `node_embeddings` (now owned by the embed sidecar in `embed.db`) and
> gains `node_tombstones` + `capture_node_delete` trigger so the sidecar can prune stale
> embeddings between reindex passes without a full-table scan.

KNN queries target `embed.db`, not `graph.db` (`embed_sidecar.rs:259`).

_(TBD: why a sibling file rather than a table or an attached schema — the lock-separation
argument. State it explicitly; it is the load-bearing reason.)_

### D2 — The direct-cosine oracle

**Citations:** `travsr-store/src/lib.rs:24, 99, 141, 625, 909, 916, 5696` ·
`travsr-daemon/src/lib.rs:5302` · `travsr-mcp/src/lib.rs:130, 156, 220` ·
`travsr-plugin-host/src/embed_supervisor.rs:20, 226` · `travsr-mcp/src/tools.rs:3480` ·
`travsr-mcp/src/query.rs:312`

The contract is already written at `store/lib.rs:141`:

> RFC-019 Option A: score `ids` against a pre-embedded `query_vec` by reading the stored
> candidate vectors from `embed.db` and computing the true cosine. Opens `embed_db_path`
> read-only (the sidecar owns writes) and reads the blobs for `model_id`. **Ids with no
> stored row or an undecodable blob are omitted from the result (contract: omission =
> "unknown").** Chunked at 500 ids…

Two hooks are armed in parallel — the query-embedding hook (`mcp/lib.rs:130, 156`) and the
oracle itself (`mcp/lib.rs:220`), injected beside the KNN hook by the daemon
(`daemon/lib.rs:5302`).

> **"Option A" implies an Option B was considered.** Find it and record it under
> Alternatives. That is the single most valuable thing to recover here.

### D3 — Absence ≠ disagreement

**Citations:** `travsr-mcp/src/seed.rs:1133, 1173, 3627`

See the Motivation quote above. The compatibility guarantee is at `seed.rs:3627`:

> absent⇒disagree semantics → Weak, **byte-for-byte identical to pre-RFC-019**

_(TBD: state the guarantee as a contract — under which conditions is behaviour required to
be unchanged, and what test pins it.)_

### D4 — Seed re-rank: contamination gate + anchor-callee seeding

**Citations:** `travsr-mcp/src/seed.rs:1824, 2368, 2407, 2412`

`seed.rs:2407, 2412` note that the "strict RFC-019 structural gate still governs" and stays
"byte-for-byte" — implying the re-rank is additive over a gate that predates it.

_(TBD: define the contamination gate, and what "anchor-callee seeding" selects.)_

### D5 — Cosine-scaled teleportation

**Citations:** `travsr-mcp/src/seed.rs:2452`

> when the oracle is confident, scale each non-exact seed's PPR weight by its true cosine
> so teleportation mass tracks semantic proximity. A 0.46-cosine seed can no longer hand
> itself a PPR restart floor above a strong structural dependency of the 1.00 exact anchor
> (see the `GetWarningsForPod` evidence). Absent-from-oracle → cosine unknown → weight left
> unchanged (structural seeds like anchor callees keep their justification).

This one touches PPR personalisation, so it interacts with **ADR-003**. Say so explicitly.

### D6 — Role provenance

**Citations:** `travsr-mcp/src/tools.rs:2849, 3963, 7771, 7799`

Caller/dependency roles now name which seed assigned them. User-visible output — worth a
short section.

### D7 — Cache invalidation _(correctness-critical)_

**Citations:** `travsr-daemon/src/query_cache.rs:11` · `travsr-daemon/src/lib.rs:5143` ·
`travsr-store/src/lib.rs:1094`

`store/lib.rs:1094`:

> `ask` results depend on `embed.db` (KNN + RFC-019 cosine oracle), which the embed sidecar
> rewrites **without ever touching `graph.db`** — so `graph.db`'s `data_version` alone
> cannot invalidate cached `ask` answers after a `travsr embed reindex`. The daemon's query
> cache keys on this value alongside the graph one (#464 follow-up).

`query_cache.rs:11`:

> The daemon passes `None` both while no `embed.db` exists and for tools that never read it
> (`graph`, `status`), so the embed sidecar's batched writes cannot thrash entries that
> don't depend on them. Together, any graph or embed mutation changes the key. That makes
> invalidation **structural**: stale entries simply stop matching and age out via LRU —
> there is no explicit `invalidate()` to forget to call.

**This is the section most worth getting right.** Anyone modifying the query cache needs
this reasoning; without it, the two-key scheme looks like redundancy and is an easy
"simplification" that silently serves stale answers.

### D8 — Thresholds

**Citations:** `travsr-mcp/src/seed.rs:201, 240`

`confirm_anchor_floor` (`seed.rs:201`) — the low-coverage `embed_confirmed_specific` rescue,
with worked examples already in the comment (`"manifests" ≈0.71` vs `"semantic" ≈0.615`).

`RECALL_FLOOR_IDENTITY = 0.75` (`seed.rs:240`) — the un-calibrated / bge fallback, citing
"RFC-019's 0.751 collision".

_(TBD: record where 0.751 came from. A named collision implies a specific measurement.)_

---

## Alternatives Considered

_(TBD — and do not skip this.)_

Known leads:

- **"Option A"** at `store/lib.rs:141` — there was an Option B. What was it, why rejected?
- Keeping `node_embeddings` in `graph.db` (pre-v17). The lock-contention argument.
- Treating absence as disagreement — the pre-RFC-019 behaviour, now the explicit fallback.

---

## Drawbacks

_(TBD.)_

- Two database files to keep consistent; cache must key on both.
- The oracle reads `embed.db` per query — cost and failure mode when absent or degraded.
- `omission = "unknown"` is silent by construction; a systematically empty oracle degrades
  quietly rather than erroring.

---

## Unresolved Questions

_(TBD.)_

---

## Acceptance Criteria

_(TBD.)_

Note the existing guarantee to preserve: un-calibrated / bge behaviour must remain
**byte-for-byte identical** (`seed.rs:240, 3627`). Whatever test pins that should be named
here.

---

## Appendix — full citation list

Regenerate with:

```bash
grep -rn "RFC-019" crates/ --include="*.rs"
```

42 citations across 9 files: `travsr-mcp/{seed,tools,query,lib}.rs`,
`travsr-store/lib.rs`, `travsr-daemon/{lib,query_cache}.rs`,
`travsr-plugin-host/{embed_sidecar,embed_supervisor}.rs`.
