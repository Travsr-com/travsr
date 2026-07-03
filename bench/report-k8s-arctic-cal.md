# Travsr tool benchmark — `arctic-embed-m-v1.5-256`

_2026-07-03T16:13:50.767Z_ · repo: k8s-arctic-cal · cold first get_context: **4421 ms**

## 1. Correctness (get_context, budget 2000, 3× median)

| metric | value |
|---|---|
| answerable queries | 16 |
| hit@1 | 0.375 |
| hit@3 | 0.438 |
| hit@5 | 0.5 |
| hit@10 | 0.625 |
| MRR | 0.433 |
| abstain accuracy (nonsense) | 0 (0/3) |

### Per-query

| id | cat | rank | conf | nodes | tok | ms | top result |
|---|---|---|---|---|---|---|---|
| L1 | literal | 1 | strong | 84 | 3017 | 467 | method:ReplicaSetController.syncReplicaSet |
| L2 | literal | 4 | strong | 99 | 4100 | 1948 | method:Controller.processNextWorkItem |
| L3 | literal | 1 | strong | 22 | 651 | 3007 | interface:SharedInformer |
| L4 | literal | 1 | strong | 106 | 3455 | 2779 | method:Scheduler.ScheduleOne |
| L5 | literal | 1 | strong | 45 | 1746 | 369 | go-pkg:pkg/controlplane/controller/leaderelection/leaderelection |
| L6 | literal | 1 | strong | 70 | 1866 | 455 | class:Proxier |
| C1 | conceptual | MISS | strong | 41 | 1241 | 2759 | fn:Pod |
| C2 | conceptual | 7 | strong | 67 | 1899 | 1224 | class:nodes |
| C3 | conceptual | MISS | strong | 41 | 1268 | 1676 | fn:Pod |
| C4 | conceptual | 6 | strong | 94 | 3016 | 2098 | interface:Manager |
| C5 | conceptual | MISS | strong | 19 | 576 | 1258 | fn:Node |
| C6 | conceptual | MISS | strong | 29 | 837 | 1701 | fn:Event |
| C7 | conceptual | 3 | strong | 37 | 1041 | 1407 | fn:Pod |
| C8 | conceptual | 1 | strong | 41 | 1200 | 1219 | fn:Endpoints |
| X1 | cross | MISS | strong | 40 | 1205 | 1428 | fn:Pod |
| X2 | cross | 33 | strong | 42 | 1229 | 1461 | fn:Pod |
| N1 | nonsense | salad✗ | weak | 39 | 1331 | 1006 | fn:newGatewayTimeoutResponse |
| N2 | nonsense | salad✗ | strong | 27 | 1184 | 1532 | class:ComponentStatusApplyConfiguration |
| N3 | nonsense | salad✗ | strong | 52 | 1612 | 1416 | fn:filter |

## 2. Performance — get_context budget sweep (3× )

| budget | median ms | p95 ms |
|---|---|---|
| 500 | 790 | 830 |
| 2000 | 773 | 774 |
| 4000 | 776 | 776 |
| 8000 | 776 | 776 |

## 3. Performance — get_snippets modes (3× median)

| target | mode | median ms | bytes | ~tokens |
|---|---|---|---|---|
| small_fn | auto | 90 | 368 | 92 |
| small_fn | full | 91 | 368 | 92 |
| small_fn | skeleton | 98 | 504 | 126 |
| large_fn | auto | 95 | 1786 | 447 |
| large_fn | full | 92 | 4360 | 1090 |
| large_fn | skeleton | 95 | 1786 | 447 |
| class | auto | 110 | 2730 | 683 |
| class | full | 103 | 1606 | 402 |
| class | skeleton | 110 | 2730 | 683 |

## 4. Usefulness (LLM-judge)

_Judge packets in `judge-packets.json`. Scored separately — see report addendum._
