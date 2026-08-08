# RFC-019: Direct-Cosine Oracle Augmentation and Seed Re-ranking

**Status:** Implemented
**Author:** Kriti Kushwaha
**Date:** 2026-08-05
**Crate(s) affected:** `travsr-mcp`, `travsr-daemon`, `travsr-store`, `travsr-plugin-host`
**Depends on:** RFC-018 (Embedding plugin architecture)
**Supersedes:** N/A
**Issue:** #536

---

## Summary

RFC-019 introduces direct-cosine oracle augmentation and seed re-ranking to improve retrieval quality for exact-symbol and low-coverage queries while preserving byte-for-byte behaviour when embedding support is unavailable.

The proposal augments the KNN oracle with directly measured query-to-candidate cosine scores for missed anchors, applies semantic validation using measured cosine instead of inferred membership, introduces structural contamination gating for exact-anchor queries, seeds anchor callees for improved dependency retrieval, and scales PPR teleportation weights using measured semantic similarity when the oracle is confident.

When embeddings are unavailable or disabled, the implementation remains a no-op and preserves the existing FTS-only behaviour.

---

## Motivation

RFC-019 addresses a limitation in the retrieval pipeline where the KNN oracle only contains cosine scores for nodes returned by the over-fetch stage. Exact anchors and specific nodes that were not surfaced by KNN were treated as if their relevance could be inferred only from membership rather than from a measured query-to-candidate cosine.

This behaviour created several issues:

1. Exact-anchor queries could be contaminated by structurally unrelated but semantically similar nodes that shared common identifier tokens.

2. Cache correctness depended on the embedding oracle, but this dependency was only referenced from code comments and not documented, making future maintenance error-prone.

3. Behavioural guarantees such as preserving byte-for-byte behaviour when embeddings are unavailable were undocumented despite being relied upon throughout the implementation.

4. Low-coverage queries required measured anchor confirmation to distinguish genuine semantic matches from coincidental lexical overlap.

RFC-019 documents these design decisions, defines the contracts that the implementation depends upon, and preserves compatibility with the existing retrieval pipeline when embedding support is disabled.

---

## Detailed Design

### D1 — Direct-cosine oracle augmentation

The existing KNN oracle only contains cosine scores for nodes surfaced during KNN over-fetch. RFC-019 extends the oracle by directly measuring query-to-candidate cosine scores for exact anchors and specific anchor nodes that were not returned by KNN.

The implementation maintains an augmented oracle that combines KNN results with directly measured cosine values. A separate `scored_ids` collection (`HashSet<NodeId>`) records every node submitted for scoring so downstream components can distinguish between:

- nodes that were measured successfully,
- nodes that were submitted but produced no score (absent ⇒ unknown — falls back to lexical evidence),
- nodes that were never submitted.

This three-way distinction is critical: a node absent from `scored_ids` is treated as "unknown" and retains its lexical justification, whereas a node present in `scored_ids` but below the floor is treated as "disagrees" and may be demoted.

When embedding support is unavailable (`score_fn == None`), the augmented oracle is identical to the original KNN oracle and the behaviour remains byte-for-byte identical to the FTS-only retrieval path.

### D2 — Tier-1 semantic validation

Semantic validation uses measured query-to-node cosine values instead of assuming relevance from KNN membership alone.

Exact anchors are intentionally preserved and are never removed during semantic validation. A deterministic exact symbol match must always reach the reranker even when the whole-query embedding is weak or abstains.

The `embed_confirmed_specific` flag evaluates whether at least one resolved specific anchor term is present in the anchor oracle with cosine `c >= confirm_anchor_floor()` (default `≈ 0.66`). This promotes low-coverage queries to strong confidence when the oracle agrees, separating answerable queries from coincidental lexical overlap.

When embeddings are unavailable, semantic validation becomes a no-op and existing retrieval behaviour is preserved.

### D3 — Seed re-ranking

Seed re-ranking is activated only when the query contains one or more exact anchors.

The algorithm computes the bounded structural neighbourhood of every exact anchor and applies a contamination gate that removes structurally unrelated semantic seeds whose cosine does not exceed the configured rescue threshold.

This prevents unrelated identifiers that share common tokens from competing with the user's intended symbol while preserving genuine semantic matches.

### D4 — Anchor-callee seeding

When an exact anchor is present, RFC-019 extends the initial seed set with the anchor's directly related callees.

Forward `RefCall` and `Depends` relationships are traversed to identify implementation nodes that may not be discoverable through lexical matching alone. This allows delegated implementations and private helper functions to participate in ranking even when they cannot be reached by exact-text search.

Existing seeds are preserved and duplicate nodes are ignored.

### D5 — Cosine-scaled teleportation

When the semantic oracle is confident, RFC-019 scales the personalization (teleportation) weight of non-exact seeds using their measured cosine score.

Exact-anchor seeds always retain priority because they represent the symbol explicitly named by the user.

Scaling is applied only when the oracle confidence exceeds the configured threshold. Nodes whose cosine is unknown retain their original weight, preserving structural justification and maintaining compatibility with existing ranking behaviour.

When semantic scoring is unavailable, teleportation weights remain unchanged and the ranking pipeline behaves identically to the existing implementation.

### D6 — Behavioural compatibility

RFC-019 intentionally preserves existing behaviour whenever semantic scoring is unavailable.

The following compatibility guarantees are maintained:

- FTS-only retrieval remains byte-for-byte identical.
- Exact-anchor queries continue to prioritize deterministic symbol matches.
- Semantic validation becomes a no-op when embeddings are unavailable.
- Existing ranking behaviour is preserved unless the oracle provides measured cosine values.

### D7 — Cache correctness

`ask` results depend on both `graph.db` (structural edges, FTS index) and `embed.db` (KNN index + RFC-019 cosine oracle). An embed reindex rewrites `embed.db` without any `graph.db` write, so graph state alone cannot detect that cached results are stale.

The cache key must therefore include `embed_data_version` — the SQLite `PRAGMA data_version` of `embed.db` as seen by the daemon's persistent read connection. When `embed.db` does not exist (FTS-only mode) or for tools that never read `embed.db` (e.g. `graph`, `status`), the daemon passes `None` for `embed_data_version` so that the embed sidecar's batched writes cannot thrash entries that do not depend on them.

Together with the existing `last_commit`, `phase_b_commit`, and `data_version` markers, any graph or embed mutation changes the cache key. Invalidation is therefore structural: stale entries simply stop matching and age out via LRU eviction — there is no explicit `invalidate()` call that a new mutation path could forget.

This contract is implemented in `travsr-daemon/src/query_cache.rs`.

---

## Key Identifiers

The following implementation identifiers correspond to the design sections above:

| Identifier | Location | Design section |
|---|---|---|
| `scored_ids` | `travsr-mcp/src/seed.rs` | D1 — tracks which nodes were submitted for direct-cosine scoring |
| `confirm_anchor_floor()` | `travsr-mcp/src/seed.rs` | D2 — returns the minimum cosine floor (default `0.66`, overridable via `TRAVSR_CONFIRM_ANCHOR_FLOOR`) |
| `embed_confirmed_specific` | `travsr-mcp/src/seed.rs` | D2 — flag promoting low-coverage queries when oracle agrees |
| `embed_data_version` | `travsr-daemon/src/query_cache.rs` | D7 — cache key component for embed freshness |

---

## Alternatives Considered

| Alternative | Why not |
|---|---|
| Continue using only the KNN oracle | Exact anchors and missed candidates would continue relying on inferred relevance instead of measured cosine values. |
| Remove structural contamination filtering | Token collisions between unrelated identifiers could incorrectly influence ranking for exact-symbol queries. |
| Apply seed re-ranking to every query | Pure natural-language queries do not have exact anchors and benefit more from semantic retrieval than structural filtering. |
| Ignore anchor-callee seeding | Delegated implementations and helper functions could remain undiscoverable despite being directly related to the queried symbol. |

---

## Drawbacks

- Additional semantic scoring introduces extra embedding work for candidates that are not returned by the original KNN oracle.
- The retrieval pipeline becomes more dependent on calibration thresholds for cosine-based decisions.
- Behaviour is more complex to understand than a purely lexical or purely KNN-based approach, requiring clear documentation of the retrieval contracts.

---

## Unresolved Questions

- Whether the cosine thresholds should remain fixed or become dynamically configurable based on the embedding model.
- Whether future retrieval pipelines should replace heuristic seed re-ranking with cross-encoder based ranking.
- Whether additional graph relationships should participate in anchor-callee seeding.

---

## Acceptance Criteria

- RFC-019 documents the behaviour implemented in the retrieval pipeline.
- Direct-cosine oracle augmentation preserves existing behaviour when embeddings are unavailable.
- Exact-anchor queries continue prioritising deterministic symbol matches.
- Seed re-ranking removes structurally unrelated semantic contamination while preserving genuine semantic matches.
- Existing tests continue to pass without behavioural regressions.
