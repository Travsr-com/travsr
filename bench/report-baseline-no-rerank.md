# Travsr tool benchmark — `arctic-embed-m-v1.5`

_2026-07-17T16:46:52.276Z_ · repo: travsr · cold first get_context: **1702 ms**

## 1. Correctness (get_context, budget 2000, 3× median)

| metric | value |
|---|---|
| answerable queries | 24 |
| hit@1 | 0.333 |
| hit@3 | 0.375 |
| hit@5 | 0.375 |
| hit@10 | 0.375 |
| MRR | 0.353 |
| abstain accuracy (nonsense) | 0.333 (1/3) |

### Per-query

| id | cat | rank | conf | nodes | tok | ms | top result |
|---|---|---|---|---|---|---|---|
| L1 | literal | 1 | exact | 84 | 2625 | 51 | fn:get_context_body |
| L2 | literal | 3 | exact | 18 | 604 | 89 | fn:validate_capacity_bounds_and_auto |
| L3 | literal | 1 | exact | 44 | 1389 | 74 | fn:validate_mcp_arg_with_limit |
| L4 | literal | 1 | exact | 6 | 228 | 40 | fn:parseSnippetsResult |
| L5 | literal | 1 | exact | 49 | 1578 | 94 | fn:snippet_line_cap |
| L6 | literal | 1 | strong | 9 | 376 | 42 | method:ContextExplorerPanel.dispose |
| C1 | conceptual | 11 | exact | 115 | 3500 | 108 | fn:get_context_body |
| C2 | conceptual | MISS | unknown | 0 | 147 | 84 | — |
| C3 | conceptual | MISS | strong | 82 | 2660 | 119 | fn:get_lang_status |
| C4 | conceptual | 1 | strong | 1 | 54 | 89 | fn:seed |
| C5 | conceptual | MISS | strong | 26 | 843 | 94 | fn:every_tool_has_input_schema_with_type_object |
| C6 | conceptual | 19 | exact | 66 | 1933 | 100 | fn:print_summary |
| C7 | conceptual | 1 | strong | 66 | 2089 | 93 | fn:knapsack |
| C8 | conceptual | MISS | exact | 45 | 1503 | 102 | fn:run_ra_lsif_timeout_kills_long_running_process |
| X1 | cross | 1 | strong | 13 | 524 | 103 | method:ContextExplorerPanel.scheduleQuery |
| X2 | cross | MISS | exact | 55 | 1675 | 97 | fn:make_node_with_kind_and_path |
| N1 | nonsense | abstain✓ | unknown | 0 | 120 | 53 | — |
| N2 | nonsense | salad✗ | weak | 25 | 846 | 90 | import:crypto |
| N3 | nonsense | salad✗ | weak | 60 | 2146 | 60 | fn:run_parallel_reindex |
| G1 | salad | MISS | strong | 75 | 2381 | 92 | fn:Guard.drop |
| G2 | salad | MISS | weak | 82 | 2688 | 97 | fn:get_lang_status |
| G3 | salad | MISS | strong | 121 | 3923 | 103 | fn:maybe_spawn_embed_phase_b_complete_detection |
| G4 | salad | MISS | unknown | 0 | 136 | 84 | — |
| G5 | salad | MISS | weak | 1 | 121 | 82 | fn:load_config |
| G6 | salad | MISS | unknown | 0 | 125 | 82 | — |
| G7 | salad | MISS | weak | 104 | 3190 | 90 | fn:Resolver.build |
| G8 | salad | MISS | unknown | 0 | 124 | 82 | — |

## 2. Performance — get_context budget sweep (3× )

| budget | median ms | p95 ms |
|---|---|---|
| 500 | 92 | 92 |
| 2000 | 90 | 90 |
| 4000 | 91 | 91 |
| 8000 | 88 | 89 |

## 3. Performance — get_snippets modes (3× median)

| target | mode | median ms | bytes | ~tokens |
|---|---|---|---|---|
| small_fn | auto | 0 | 1400 | 350 |
| small_fn | full | 0 | 1400 | 350 |
| small_fn | skeleton | 29 | 1324 | 331 |
| large_fn | auto | 30 | 2035 | 509 |
| large_fn | full | 1 | 26990 | 6748 |
| large_fn | skeleton | 29 | 2035 | 509 |
| class | auto | 2 | 951 | 238 |
| class | full | 0 | 9077 | 2269 |
| class | skeleton | 2 | 951 | 238 |

## 4. Usefulness (LLM-judge)

_Judge packets in `judge-packets.json`. Scored separately — see report addendum._
