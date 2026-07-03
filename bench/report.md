# Travsr tool benchmark — `bge-small-en-v1.5`

_2026-07-01T10:14:46.269Z_ · repo: travsr (self) · cold first get_context: **407 ms**

## 1. Correctness (get_context, budget 2000, 3× median)

| metric | value |
|---|---|
| answerable queries | 16 |
| hit@1 | 0.688 |
| hit@3 | 0.813 |
| hit@5 | 0.813 |
| hit@10 | 0.938 |
| MRR | 0.756 |
| abstain accuracy (nonsense) | 0 (0/3) |

### Per-query

| id | cat | rank | conf | nodes | tok | ms | top result |
|---|---|---|---|---|---|---|---|
| L1 | literal | 1 | exact | 34 | 948 | 19 | fn:get_context_body |
| L2 | literal | 1 | exact | 20 | 473 | 38 | impl:SnippetMode |
| L3 | literal | 1 | exact | 5 | 161 | 18 | fn:validate_mcp_list_arg |
| L4 | literal | 1 | exact | 5 | 167 | 20 | fn:parseSnippetsResult |
| L5 | literal | 1 | strong | 6 | 200 | 35 | fn:snippet_line_cap |
| L6 | literal | 1 | strong | 17 | 606 | 18 | class:ContextExplorerPanel |
| C1 | conceptual | 1 | strong | 74 | 1836 | 57 | fn:token_budget_keeps_bfs_prefix_and_reports_cut |
| C2 | conceptual | 8 | strong | 55 | 1340 | 37 | fn:inline_approval_prompt |
| C3 | conceptual | 7 | strong | 12 | 351 | 42 | fn:language_from_extension_covers_all_variants |
| C4 | conceptual | 1 | strong | 55 | 1467 | 39 | fn:ppr |
| C5 | conceptual | 3 | strong | 64 | 1620 | 58 | fn:normalize_nl_query |
| C6 | conceptual | 2 | exact | 82 | 2113 | 57 | fn:print_summary |
| C7 | conceptual | 1 | strong | 59 | 1576 | 42 | fn:knapsack |
| C8 | conceptual | MISS | strong | 101 | 2552 | 51 | fn:try_new_rejects_too_long |
| X1 | cross | 1 | strong | 11 | 353 | 46 | fn:get_context |
| X2 | cross | 1 | strong | 14 | 376 | 45 | fn:snippet_line_cap |
| N1 | nonsense | salad✗ | weak | 28 | 760 | 35 | fn:extract_bearer |
| N2 | nonsense | salad✗ | weak | 64 | 1670 | 50 | fn:pcst_is_deterministic_on_tied_cost_topology |
| N3 | nonsense | salad✗ | weak | 20 | 588 | 39 | fn:Daemon.run |

## 2. Performance — get_context budget sweep (3× )

| budget | median ms | p95 ms |
|---|---|---|
| 500 | 42 | 44 |
| 2000 | 41 | 42 |
| 4000 | 42 | 42 |
| 8000 | 41 | 42 |

## 3. Performance — get_snippets modes (3× median)

| target | mode | median ms | bytes | ~tokens |
|---|---|---|---|---|
| small_fn | auto | 2 | 1400 | 350 |
| small_fn | full | 2 | 1400 | 350 |
| small_fn | skeleton | 28 | 1324 | 331 |
| large_fn | auto | 28 | 2079 | 520 |
| large_fn | full | 2 | 23856 | 5964 |
| large_fn | skeleton | 29 | 2079 | 520 |
| class | auto | 4 | 643 | 161 |
| class | full | 2 | 9262 | 2316 |
| class | skeleton | 4 | 643 | 161 |

## 4. Usefulness (LLM-judge)

_Judge packets in `judge-packets.json`. Scored separately — see report addendum._
