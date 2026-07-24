# Travsr tool benchmark — `arctic-embed-m-v1.5`

_2026-07-18T14:53:43.337Z_ · repo: travsr · cold first get_context: **2058 ms**

## 1. Correctness (get_context, budget 2000, 3× median)

| metric | value |
|---|---|
| answerable queries | 24 |
| hit@1 | 0.292 |
| hit@3 | 0.333 |
| hit@5 | 0.333 |
| hit@10 | 0.333 |
| MRR | 0.312 |
| abstain accuracy (nonsense) | 0.667 (2/3) |

### Per-query

| id | cat | rank | conf | nodes | tok | ms | top result |
|---|---|---|---|---|---|---|---|
| L1 | literal | 1 | exact | 84 | 2625 | 367 | fn:get_context_body |
| L2 | literal | 3 | exact | 18 | 604 | 241 | fn:validate_capacity_bounds_and_auto |
| L3 | literal | 1 | exact | 44 | 1389 | 570 | fn:validate_mcp_arg_with_limit |
| L4 | literal | 1 | exact | 6 | 228 | 508 | fn:parseSnippetsResult |
| L5 | literal | 1 | exact | 58 | 1879 | 574 | fn:snippet_line_cap |
| L6 | literal | 1 | strong | 8 | 305 | 389 | method:ContextExplorerPanel.dispose |
| C1 | conceptual | 11 | exact | 116 | 3552 | 346 | fn:get_context_body |
| C2 | conceptual | MISS | unknown | 0 | 147 | 602 | — |
| C3 | conceptual | MISS | unknown | 0 | 166 | 319 | — |
| C4 | conceptual | MISS | unknown | 0 | 98 | 116 | — |
| C5 | conceptual | MISS | strong | 26 | 832 | 444 | fn:every_tool_has_input_schema_with_type_object |
| C6 | conceptual | 19 | exact | 66 | 1926 | 355 | fn:print_summary |
| C7 | conceptual | 1 | strong | 66 | 2105 | 349 | fn:knapsack |
| C8 | conceptual | MISS | exact | 45 | 1503 | 474 | fn:run_ra_lsif_timeout_kills_long_running_process |
| X1 | cross | 1 | strong | 13 | 532 | 228 | method:ContextExplorerPanel.scheduleQuery |
| X2 | cross | MISS | exact | 54 | 1647 | 344 | fn:make_node_with_kind_and_path |
| N1 | nonsense | abstain✓ | unknown | 0 | 122 | 180 | — |
| N2 | nonsense | salad✗ | weak | 25 | 846 | 172 | import:crypto |
| N3 | nonsense | abstain✓ | unknown | 0 | 135 | 176 | — |
| G1 | salad | MISS | unknown | 0 | 136 | 211 | — |
| G2 | salad | MISS | unknown | 0 | 180 | 237 | — |
| G3 | salad | MISS | unknown | 0 | 149 | 550 | — |
| G4 | salad | MISS | unknown | 0 | 136 | 720 | — |
| G5 | salad | MISS | weak | 1 | 121 | 181 | fn:load_config |
| G6 | salad | MISS | unknown | 0 | 128 | 335 | — |
| G7 | salad | MISS | weak | 104 | 3190 | 1450 | fn:Resolver.build |
| G8 | salad | MISS | unknown | 0 | 123 | 542 | — |

## 2. Performance — get_context budget sweep (3× )

| budget | median ms | p95 ms |
|---|---|---|
| 500 | 481 | 505 |
| 2000 | 1053 | 1142 |
| 4000 | 531 | 573 |
| 8000 | 576 | 678 |

## 3. Performance — get_snippets modes (3× median)

| target | mode | median ms | bytes | ~tokens |
|---|---|---|---|---|
| small_fn | auto | 1 | 1400 | 350 |
| small_fn | full | 1 | 1400 | 350 |
| small_fn | skeleton | 29 | 1324 | 331 |
| large_fn | auto | 29 | 2035 | 509 |
| large_fn | full | 1 | 26990 | 6748 |
| large_fn | skeleton | 30 | 2035 | 509 |
| class | auto | 2 | 951 | 238 |
| class | full | 0 | 9077 | 2269 |
| class | skeleton | 2 | 951 | 238 |

## 4. Usefulness (LLM-judge)

_Judge packets in `judge-packets.json`. Scored separately — see report addendum._
