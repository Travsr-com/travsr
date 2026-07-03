# Travsr tool benchmark — `bge-small-en-v1.5` (VALID baseline)

_Run 1, 2026-07-01. Index @ commit deddccc, 4034 embeddings (pre-reindex, consistent).
Repo: travsr (self, 8412 nodes). Cold first get_context: **722 ms**._

> Provenance: this is Run 1, before `travsr embed reindex` corrupted the on-disk
> HNSW (see report-INCIDENT.md). Run 2 numbers are discarded. Caveat: L2 targets
> `SnippetMode`, a symbol added this session that was not embedded at Run 1 time,
> so its miss reflects staleness, not retrieval quality.

## 1. Correctness (get_context, budget 2000, 3× median)

| metric | value |
|---|---|
| answerable queries | 16 |
| hit@1 | 0.50 |
| hit@3 | 0.69 |
| hit@5 | 0.75 |
| hit@10 | 0.81 |
| MRR | 0.61 |
| abstain accuracy (nonsense) | 0.67 (2/3) |

### Per-query

| id | cat | rank | conf | nodes | tok | ms | top result |
|---|---|---|---|---|---|---|---|
| L1 | literal | 1 | exact | 59 | 1557 | 20 | fn:get_context_body |
| L2 | literal | MISS* | strong | 82 | 2108 | 42 | fn:rust_enum_node (*unembedded new symbol) |
| L3 | literal | 2 | strong | 6 | 185 | 18 | fn:AttrList.as_mut_ptr |
| L4 | literal | 1 | strong | 5 | 162 | 18 | fn:looksLikeSnippetHeader |
| L5 | literal | 1 | strong | 5 | 162 | 34 | fn:snippet_line_cap |
| L6 | literal | 1 | strong | 42 | 1133 | 21 | class:ContextExplorerPanel |
| C1 | conceptual | 1 | strong | 35 | 886 | 46 | fn:apply_token_budget |
| C2 | conceptual | MISS | none | 0 | 141 | 35 | — (abstained; safe failure) |
| C3 | conceptual | 6 | weak | 49 | 1333 | 43 | fn:AstSkeleton.embed_rich |
| C4 | conceptual | 1 | strong | 56 | 1493 | 38 | fn:ppr |
| C5 | conceptual | 3 | strong | 61 | 1577 | 57 | fn:normalize_nl_query |
| C6 | conceptual | 2 | exact | 69 | 1834 | 62 | fn:GoCallback |
| C7 | conceptual | 1 | strong | 56 | 1459 | 42 | fn:knapsack |
| C8 | conceptual | MISS | weak | 82 | 2159 | 51 | fn:dot_lang_resolves_to (salad) |
| X1 | cross | 1 | strong | 24 | 665 | 47 | fn:get_context |
| X2 | cross | 5 | weak | 84 | 2147 | 53 | fn:map_kind_from_wire |
| N1 | nonsense | abstain✓ | none | 0 | 138 | 34 | — |
| N2 | nonsense | salad✗ | weak | 52 | 1447 | 42 | fn:knapsack |
| N3 | nonsense | abstain✓ | none | 0 | 140 | 35 | — |

## 2. Performance — get_context budget sweep (warm, 3×)

| budget | median ms | p95 ms |
|---|---|---|
| 500 | 32 | 32 |
| 2000 | 32 | 32 |
| 4000 | 32 | 33 |
| 8000 | 33 | 35 |

Latency is budget-independent (knapsack/format cost negligible vs seed+PPR).

## 3. Performance — get_snippets modes (3× median)

| target | mode | median ms | bytes | ~tokens |
|---|---|---|---|---|
| small_fn (kind_boost) | auto | 2 | 1400 | 350 |
| small_fn | full | 2 | 1400 | 350 |
| small_fn | skeleton | 28 | 1324 | 331 |
| large_fn (get_context_body) | auto | 28 | 2079 | 520 |
| large_fn | full | 2 | 23856 | 5964 |
| large_fn | skeleton | 28 | 2079 | 520 |
| class (ContextExplorerPanel) | auto | 4 | 643 | 161 |
| class | full | 2 | 9262 | 2316 |
| class | skeleton | 4 | 643 | 161 |

Notes: skeleton costs ~28 ms (Tree-sitter re-parse); raw/full is a cheap file read
(~2 ms). `auto` on a large fn pays the skeleton cost (it overflows the line cap).
`full` returns the whole body uncapped (large_fn: 24 KB / ~6k tokens) — confirming
the mode + the removed 4 KB output cap work end-to-end.

## Key qualitative findings (bge-small)
- **Literal recall is strong** (5/6 hit@3; the 6th is the unembedded new symbol).
- **Conceptual recall is the weak spot** — C2 ("prompt injection defence") abstains
  and C8 ("reject long/traversal args") returns salad, though both target real
  indexed code (sanitize.rs / validate_mcp_arg). This is exactly where bge-base/large
  richer embeddings are hypothesized to help (to be measured, not assumed).
- **Abstention is imperfect**: N2 returned weak-confidence salad instead of abstaining
  (a known R1-class "confident salad on no-match" gap).
- **Perf is not the bottleneck**: warm get_context 20–65 ms, snippets 2–28 ms.
