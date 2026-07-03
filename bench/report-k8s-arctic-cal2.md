# Travsr tool benchmark — `arctic-embed-m-v1.5-256`

_2026-07-03T16:39:50.254Z_ · repo: k8s-arctic-cal2 · cold first get_context: **3123 ms**

## 1. Correctness (get_context, budget 2000, 3× median)

| metric | value |
|---|---|
| answerable queries | 16 |
| hit@1 | 0.313 |
| hit@3 | 0.438 |
| hit@5 | 0.5 |
| hit@10 | 0.563 |
| MRR | 0.399 |
| abstain accuracy (nonsense) | 0.333 (1/3) |

### Per-query

| id | cat | rank | conf | nodes | tok | ms | top result |
|---|---|---|---|---|---|---|---|
| L1 | literal | 1 | strong | 84 | 3017 | 266 | method:ReplicaSetController.syncReplicaSet |
| L2 | literal | MISS | strong | 50 | 1739 | 1581 | method:Controller.processNextWorkItem |
| L3 | literal | 1 | strong | 22 | 651 | 1997 | interface:SharedInformer |
| L4 | literal | 1 | strong | 106 | 3455 | 2007 | method:Scheduler.ScheduleOne |
| L5 | literal | 1 | strong | 20 | 762 | 275 | class:leaderElection |
| L6 | literal | 1 | strong | 46 | 1212 | 457 | class:Proxier |
| C1 | conceptual | 10 | strong | 53 | 1742 | 2709 | fn:Pod |
| C2 | conceptual | 5 | strong | 32 | 970 | 1666 | class:nodes |
| C3 | conceptual | MISS | strong | 41 | 1268 | 2496 | fn:Pod |
| C4 | conceptual | MISS | strong | 60 | 1800 | 2981 | interface:Manager |
| C5 | conceptual | MISS | strong | 19 | 576 | 2282 | fn:Node |
| C6 | conceptual | MISS | strong | 49 | 1456 | 1736 | fn:Event |
| C7 | conceptual | 2 | strong | 36 | 1082 | 2305 | fn:Pod |
| C8 | conceptual | 2 | strong | 45 | 1324 | 1201 | fn:Service |
| X1 | cross | MISS | strong | 38 | 1154 | 1331 | fn:Pod |
| X2 | cross | 11 | strong | 33 | 980 | 1365 | fn:Pod |
| N1 | nonsense | salad✗ | weak | 39 | 1331 | 1009 | fn:newGatewayTimeoutResponse |
| N2 | nonsense | salad✗ | weak | 27 | 1208 | 2177 | class:ComponentStatusApplyConfiguration |
| N3 | nonsense | abstain✓ | unknown | 0 | 157 | 2381 | — |

## 2. Performance — get_context budget sweep (3× )

| budget | median ms | p95 ms |
|---|---|---|
| 500 | 1818 | 1830 |
| 2000 | 1755 | 1870 |
| 4000 | 1677 | 1887 |
| 8000 | 2152 | 2177 |

## 3. Performance — get_snippets modes (3× median)

| target | mode | median ms | bytes | ~tokens |
|---|---|---|---|---|
| small_fn | auto | 86 | 368 | 92 |
| small_fn | full | 94 | 368 | 92 |
| small_fn | skeleton | 100 | 504 | 126 |
| large_fn | auto | 98 | 1786 | 447 |
| large_fn | full | 95 | 4360 | 1090 |
| large_fn | skeleton | 99 | 1786 | 447 |
| class | auto | 113 | 2730 | 683 |
| class | full | 106 | 1606 | 402 |
| class | skeleton | 113 | 2730 | 683 |

## 4. Usefulness (LLM-judge)

_Judge packets in `judge-packets.json`. Scored separately — see report addendum._
