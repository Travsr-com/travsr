# Travsr tool benchmark — `arctic-embed-m-v1.5`

_2026-07-18T15:19:41.101Z_ · repo: k8s-rerank · cold first get_context: **5584 ms**

## 1. Correctness (get_context, budget 2000, 3× median)

| metric | value |
|---|---|
| answerable queries | 24 |
| hit@1 | 0.167 |
| hit@3 | 0.208 |
| hit@5 | 0.292 |
| hit@10 | 0.292 |
| MRR | 0.208 |
| abstain accuracy (nonsense) | 1 (3/3) |

### Per-query

| id | cat | rank | conf | nodes | tok | ms | top result |
|---|---|---|---|---|---|---|---|
| L1 | literal | 1 | strong | 88 | 3139 | 2309 | method:ReplicaSetController.syncReplicaSet |
| L2 | literal | 4 | strong | 93 | 4109 | 2811 | method:Controller.processNextWorkItem |
| L3 | literal | MISS | unknown | 0 | 146 | 2842 | — |
| L4 | literal | 1 | strong | 105 | 3426 | 2415 | method:Scheduler.ScheduleOne |
| L5 | literal | 1 | strong | 68 | 2591 | 2614 | go-pkg:staging/src/k8s.io/client-go/tools/leaderelection/leaderelection |
| L6 | literal | 2 | strong | 42 | 1139 | 1671 | go-pkg:pkg/util/iptables/iptables |
| C1 | conceptual | MISS | unknown | 0 | 181 | 1687 | — |
| C2 | conceptual | MISS | unknown | 0 | 146 | 791 | — |
| C3 | conceptual | MISS | weak | 77 | 2640 | 2703 | fn:Pod |
| C4 | conceptual | MISS | unknown | 0 | 172 | 1942 | — |
| C5 | conceptual | MISS | unknown | 0 | 167 | 1505 | — |
| C6 | conceptual | MISS | unknown | 0 | 194 | 1954 | — |
| C7 | conceptual | 1 | weak | 101 | 3245 | 2555 | fn:Pod |
| C8 | conceptual | MISS | unknown | 0 | 155 | 2860 | — |
| X1 | cross | MISS | unknown | 0 | 165 | 2944 | — |
| X2 | cross | 4 | strong | 58 | 1905 | 4123 | class:Binding |
| N1 | nonsense | abstain✓ | unknown | 0 | 184 | 1374 | — |
| N2 | nonsense | abstain✓ | unknown | 0 | 163 | 2842 | — |
| N3 | nonsense | abstain✓ | unknown | 0 | 147 | 3384 | — |
| G1 | salad | MISS | unknown | 0 | 153 | 3821 | — |
| G2 | salad | MISS | weak | 119 | 4121 | 4551 | fn:verifyTotalEvents |
| G3 | salad | MISS | unknown | 0 | 131 | 2863 | — |
| G4 | salad | MISS | weak | 106 | 3519 | 3194 | fn:startEndpointWatcher |
| G5 | salad | MISS | unknown | 0 | 166 | 3390 | — |
| G6 | salad | MISS | unknown | 0 | 150 | 8414 | — |
| G7 | salad | MISS | weak | 75 | 2418 | 9045 | fn:AppArmorProfile |
| G8 | salad | MISS | unknown | 0 | 137 | 4329 | — |

## 2. Performance — get_context budget sweep (3× )

| budget | median ms | p95 ms |
|---|---|---|
| 500 | 2376 | 2936 |
| 2000 | 3372 | 3661 |
| 4000 | 2507 | 2705 |
| 8000 | 2569 | 2572 |

## 3. Performance — get_snippets modes (3× median)

| target | mode | median ms | bytes | ~tokens |
|---|---|---|---|---|
| small_fn | auto | 0 | 58 | 15 |
| small_fn | full | 0 | 58 | 15 |
| small_fn | skeleton | 0 | 58 | 15 |
| large_fn | auto | 0 | 58 | 15 |
| large_fn | full | 0 | 58 | 15 |
| large_fn | skeleton | 0 | 58 | 15 |
| class | auto | 0 | 58 | 15 |
| class | full | 0 | 58 | 15 |
| class | skeleton | 0 | 58 | 15 |

## 4. Usefulness (LLM-judge)

_Judge packets in `judge-packets.json`. Scored separately — see report addendum._
