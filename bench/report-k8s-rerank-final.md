# Travsr tool benchmark — `arctic-embed-m-v1.5`

_2026-07-18T17:25:28.766Z_ · repo: k8s-rerank-final · cold first get_context: **6527 ms**

## 1. Correctness (get_context, budget 2000, 3× median)

| metric | value |
|---|---|
| answerable queries | 24 |
| hit@1 | 0.208 |
| hit@3 | 0.25 |
| hit@5 | 0.333 |
| hit@10 | 0.333 |
| MRR | 0.25 |
| abstain accuracy (nonsense) | 1 (3/3) |

### Per-query

| id | cat | rank | conf | nodes | tok | ms | top result |
|---|---|---|---|---|---|---|---|
| L1 | literal | 1 | strong | 88 | 3139 | 2192 | method:ReplicaSetController.syncReplicaSet |
| L2 | literal | 4 | strong | 93 | 4084 | 3126 | method:Controller.processNextWorkItem |
| L3 | literal | 1 | strong | 57 | 1826 | 2775 | interface:SharedInformer |
| L4 | literal | 1 | strong | 105 | 3426 | 2633 | method:Scheduler.ScheduleOne |
| L5 | literal | 1 | strong | 68 | 2602 | 2281 | go-pkg:staging/src/k8s.io/client-go/tools/leaderelection/leaderelection |
| L6 | literal | 2 | strong | 42 | 1139 | 1790 | go-pkg:pkg/util/iptables/iptables |
| C1 | conceptual | MISS | unknown | 0 | 181 | 2762 | — |
| C2 | conceptual | MISS | unknown | 0 | 146 | 1877 | — |
| C3 | conceptual | MISS | weak | 77 | 2640 | 3795 | fn:Pod |
| C4 | conceptual | MISS | unknown | 0 | 172 | 3085 | — |
| C5 | conceptual | MISS | unknown | 0 | 167 | 2385 | — |
| C6 | conceptual | MISS | unknown | 0 | 194 | 2809 | — |
| C7 | conceptual | 1 | weak | 101 | 3245 | 1530 | fn:Pod |
| C8 | conceptual | MISS | unknown | 0 | 155 | 1446 | — |
| X1 | cross | MISS | unknown | 0 | 165 | 1594 | — |
| X2 | cross | 4 | strong | 58 | 1905 | 1806 | class:Binding |
| N1 | nonsense | abstain✓ | unknown | 0 | 184 | 1282 | — |
| N2 | nonsense | abstain✓ | unknown | 0 | 163 | 2029 | — |
| N3 | nonsense | abstain✓ | unknown | 0 | 147 | 1781 | — |
| G1 | salad | MISS | unknown | 0 | 153 | 1759 | — |
| G2 | salad | MISS | weak | 119 | 4125 | 2492 | fn:verifyTotalEvents |
| G3 | salad | MISS | unknown | 0 | 131 | 1493 | — |
| G4 | salad | MISS | weak | 106 | 3519 | 1779 | fn:startEndpointWatcher |
| G5 | salad | MISS | unknown | 0 | 170 | 2136 | — |
| G6 | salad | MISS | unknown | 0 | 146 | 1710 | — |
| G7 | salad | MISS | weak | 72 | 2336 | 2134 | fn:AppArmorProfile |
| G8 | salad | MISS | unknown | 0 | 144 | 1991 | — |

## 2. Performance — get_context budget sweep (3× )

| budget | median ms | p95 ms |
|---|---|---|
| 500 | 1054 | 1246 |
| 2000 | 855 | 953 |
| 4000 | 863 | 954 |
| 8000 | 909 | 977 |

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
