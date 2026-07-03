# Travsr tool benchmark — `bge-small-en-v1.5` (FINAL, post-fix)

_2026-07-01 · repo: travsr (self, 8412 nodes, 4045 embeddings) · cold get_context 407 ms._

This is the valid semantic run, after fixing the HNSW-build regression (see
report-INCIDENT-reindex-corruption.md). The `bench/` harness is model-agnostic —
re-run under bge-base/large via `embed switch <m> && embed reindex && node bench/run.mjs`.

## 0. The fix (why this run is valid)
`travsr-embed`'s `build_from_db` (src/index.rs) required each node to have an incoming
`ref/call` edge to enter the HNSW. That dropped **970 of 4045** embedded nodes from the
index — anything with no *recorded* caller (entry points, trait/impl methods, freshly
edited symbols pre-Phase-B). KNN could never return them, so `get_context` seeded on
unrelated nodes and returned garbage even for exact-name queries. Fix: index all
embeddable nodes (kind filter only); blast-radius preference belongs in downstream
ranking, not index membership. HNSW went 970 → 4045 nodes.

## 1. Correctness — before (stale/sparse index) vs after (fixed)

| metric | before (sparse HNSW) | after (fixed) |
|---|---|---|
| hit@1 | 0.50 | **0.69** |
| hit@3 | 0.69 | **0.81** |
| hit@10 | 0.81 | **0.94** |
| MRR | 0.61 | **0.76** |
| abstain accuracy (nonsense) | 0.67 | **0.00** ⚠ |

Literal recall is now **6/6 hit@1** (all exact/strong). Distinctive conceptual queries
(ppr, knapsack, skeleton, get_context flow) hit@1. Cross-file queries hit@1.

### ⚠ New finding: abstention regressed to 0/3
With full recall, KNN always returns *something*, so nonsense queries
("quantum blockchain payment gateway") now return weak-confidence nodes instead of
abstaining (they returned 0 nodes when the index was sparse). Abstention was
**accidentally relying on index sparsity**, not a real relevance floor. This is the
R1 "confident salad on no-match" gap — now the top priority for get_context seed logic:
abstain when the best fused seed score is below a floor, regardless of KNN recall.

## 2. Usefulness — LLM-judge (1–5: would this context let a repo-blind AI answer?)

Judge: Claude, rubric below, over `bench/judge-packets.json` (no API key here; packets
are ready for an independent API judge). Mean answerable score **4.2 / 5**.

| id | query | score | note |
|---|---|---|---|
| L1 | get_context_body | 5 | target #1 + get_context_with_filter/raw siblings |
| L2 | SnippetMode … | 4 | target present, interleaved with unrelated enums |
| L3 | validate_mcp_list_arg | 5 | target + validate_mcp_arg sibling + callers |
| L4 | parseSnippetsResult | 5 | target + parseContextResult sibling |
| L5 | snippet_line_cap … | 5 | target + its tests |
| L6 | ContextExplorerPanel | 5 | class + methods |
| C1 | snippets truncated to budget | 4 | budget-truncate logic + tests present |
| C2 | prompt injection defence | **2** | polysemy: matched "prompt" (approval prompts); sanitize.rs at rank 8 |
| C3 | extension add to AI context | **2** | matched add/context/extension literally to wrong nodes; pin at rank 7 |
| C4 | ppr seed selection | 5 | ppr / build_seed_set — dead on |
| C5 | avoid re-embedding cold start | 3 | embed/query neighbourhood, not the specific gate |
| C6 | AST skeleton for large fn | 4 | skeleton_for_node / AstSkeleton |
| C7 | knapsack token budget | 5 | knapsack / apply_token_budget |
| C8 | reject long/traversal args | 3 | found path-traversal *tests*, not validate_mcp_arg impl |
| X1 | get_context panel→snippets flow | 5 | spans crate + extension |
| X2 | kind-aware line limits | 5 | snippet_line_cap / snippet_for_node |

Rubric: 5 target #1 + strong siblings · 4 target top-3 · 3 topical but imprecise ·
2 misleading/buried · 1 wrong.

### Usefulness takeaways (the bge-base/large hypothesis, now concrete)
- **Distinctive vocabulary → excellent** (ppr, knapsack, skeleton: 5/5).
- **Polysemous / generic terms → poor** (C2 "prompt", C3 "add/context/extension"): bge-small
  matches the surface token, not the concept. **This is exactly what bge-base (63.6 MTEB)
  / large (64.2) should improve** — the concrete, measurable hypothesis to test next.
- **Tests outrank implementations** (C1, C8 surfaced `*_rejects_*` test fns over the impl).
  A test-deprioritisation signal in seed ranking would raise C8-type scores.

## 3. Performance (3× median)

get_context: cold **407 ms**; warm **18–58 ms**, budget-independent (500→8000 all ~32 ms).

get_snippets modes (unchanged by the embed fix):
| target | auto | full | skeleton |
|---|---|---|---|
| small fn | 2 ms / 350 tok | 2 ms / 350 tok | 28 ms / 331 tok |
| large fn | 28 ms / 520 tok | 2 ms / **5964 tok (uncapped)** | 28 ms / 520 tok |
| class | 4 ms / 161 tok | 2 ms / 2316 tok | 4 ms / 161 tok |

skeleton pays a ~28 ms Tree-sitter reparse; raw/full is a ~2 ms file read; `full` returns
the whole body uncapped (confirms the mode + removed 4 KB output cap).

## Next
1. Add a relevance floor to get_context seed selection (fix the 0/3 abstain regression).
2. Run bench under bge-base + bge-large to test the polysemy/conceptual hypothesis.
3. Deprioritise test functions in seed ranking.
