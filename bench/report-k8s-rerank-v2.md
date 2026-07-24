# Travsr tool benchmark — `arctic-embed-m-v1.5`

_2026-07-18T17:03:33.037Z_ · repo: k8s-rerank-v2 · cold first get_context: **7318 ms**

## 1. Correctness (get_context, budget 2000, 3× median)

| metric | value |
|---|---|
| answerable queries | 24 |
| hit@1 | 0.208 |
| hit@3 | 0.25 |
| hit@5 | 0.292 |
| hit@10 | 0.292 |
| MRR | 0.24 |
| abstain accuracy (nonsense) | 1 (3/3) |

### Per-query

| id | cat | rank | conf | nodes | tok | ms | top result |
|---|---|---|---|---|---|---|---|
| L1 | literal | 1 | strong | 88 | 3139 | 2435 | method:ReplicaSetController.syncReplicaSet |
| L2 | literal | 4 | strong | 93 | 4084 | 4099 | method:Controller.processNextWorkItem |
| L3 | literal | 1 | strong | 57 | 1826 | 3647 | interface:SharedInformer |
| L4 | literal | 1 | strong | 105 | 3426 | 2770 | method:Scheduler.ScheduleOne |
| L5 | literal | 1 | strong | 68 | 2602 | 1386 | go-pkg:staging/src/k8s.io/client-go/tools/leaderelection/leaderelection |
| L6 | literal | 2 | strong | 42 | 1139 | 1142 | go-pkg:pkg/util/iptables/iptables |
| C1 | conceptual | MISS | unknown | 0 | 181 | 4082 | — |
| C2 | conceptual | MISS | unknown | 0 | 146 | 1020 | — |
| C3 | conceptual | MISS | weak | 77 | 2640 | 3250 | fn:Pod |
| C4 | conceptual | MISS | unknown | 0 | 172 | 2821 | — |
| C5 | conceptual | MISS | unknown | 0 | 167 | 1651 | — |
| C6 | conceptual | MISS | unknown | 0 | 194 | 2281 | — |
| C7 | conceptual | 1 | weak | 101 | 3245 | 3371 | fn:Pod |
| C8 | conceptual | MISS | unknown | 0 | 155 | 1949 | — |
| X1 | cross | MISS | unknown | 0 | 165 | 1762 | — |
| X2 | cross | MISS | unknown | 0 | 149 | 1588 | — |
| N1 | nonsense | abstain✓ | unknown | 0 | 184 | 1272 | — |
| N2 | nonsense | abstain✓ | unknown | 0 | 163 | 1861 | — |
| N3 | nonsense | abstain✓ | unknown | 0 | 147 | 1887 | — |
| G1 | salad | MISS | unknown | 0 | 153 | 1856 | — |
| G2 | salad | MISS | weak | 119 | 4121 | 3994 | fn:verifyTotalEvents |
| G3 | salad | MISS | unknown | 0 | 131 | 1597 | — |
| G4 | salad | MISS | weak | 106 | 3519 | 1878 | fn:startEndpointWatcher |
| G5 | salad | MISS | unknown | 0 | 170 | 1817 | — |
| G6 | salad | MISS | unknown | 0 | 146 | 1828 | — |
| G7 | salad | MISS | weak | 72 | 2336 | 4145 | fn:AppArmorProfile |
| G8 | salad | MISS | unknown | 0 | 137 | 3072 | — |

## 2. Performance — get_context budget sweep (3× )

| budget | median ms | p95 ms |
|---|---|---|
| 500 | 1028 | 1148 |
| 2000 | 887 | 930 |
| 4000 | 894 | 1087 |
| 8000 | 828 | 832 |

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
