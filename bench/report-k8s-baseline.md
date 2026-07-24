# Travsr tool benchmark — `arctic-embed-m-v1.5`

_2026-07-18T15:13:56.596Z_ · repo: k8s-baseline · cold first get_context: **7034 ms**

## 1. Correctness (get_context, budget 2000, 3× median)

| metric | value |
|---|---|
| answerable queries | 24 |
| hit@1 | 0.125 |
| hit@3 | 0.167 |
| hit@5 | 0.208 |
| hit@10 | 0.208 |
| MRR | 0.156 |
| abstain accuracy (nonsense) | 0.667 (2/3) |

### Per-query

| id | cat | rank | conf | nodes | tok | ms | top result |
|---|---|---|---|---|---|---|---|
| L1 | literal | 1 | strong | 84 | 3015 | 1773 | method:ReplicaSetController.syncReplicaSet |
| L2 | literal | 4 | strong | 93 | 4109 | 2065 | method:Controller.processNextWorkItem |
| L3 | literal | MISS | unknown | 0 | 146 | 1755 | — |
| L4 | literal | MISS | unknown | 0 | 129 | 1689 | — |
| L5 | literal | 1 | strong | 68 | 2591 | 1680 | go-pkg:staging/src/k8s.io/client-go/tools/leaderelection/leaderelection |
| L6 | literal | 2 | strong | 42 | 1138 | 877 | go-pkg:pkg/util/iptables/iptables |
| C1 | conceptual | MISS | unknown | 0 | 185 | 1594 | — |
| C2 | conceptual | MISS | unknown | 0 | 142 | 943 | — |
| C3 | conceptual | MISS | weak | 70 | 2457 | 2072 | fn:Pod |
| C4 | conceptual | MISS | unknown | 0 | 194 | 2758 | — |
| C5 | conceptual | MISS | unknown | 0 | 188 | 2035 | — |
| C6 | conceptual | MISS | weak | 28 | 832 | 4032 | fn:Event |
| C7 | conceptual | 1 | weak | 89 | 2965 | 2821 | interface:State |
| C8 | conceptual | MISS | unknown | 0 | 156 | 2065 | — |
| X1 | cross | MISS | unknown | 0 | 165 | 1907 | — |
| X2 | cross | MISS | unknown | 0 | 155 | 1188 | — |
| N1 | nonsense | abstain✓ | unknown | 0 | 175 | 995 | — |
| N2 | nonsense | salad✗ | weak | 86 | 2978 | 1694 | method:SimpleReactor.React |
| N3 | nonsense | abstain✓ | unknown | 0 | 146 | 1340 | — |
| G1 | salad | MISS | unknown | 0 | 152 | 1786 | — |
| G2 | salad | MISS | weak | 119 | 4121 | 3261 | fn:verifyTotalEvents |
| G3 | salad | MISS | unknown | 0 | 161 | 1420 | — |
| G4 | salad | MISS | weak | 106 | 3519 | 1821 | fn:startEndpointWatcher |
| G5 | salad | MISS | unknown | 0 | 166 | 2091 | — |
| G6 | salad | MISS | unknown | 0 | 150 | 2182 | — |
| G7 | salad | MISS | weak | 75 | 2418 | 4378 | fn:AppArmorProfile |
| G8 | salad | MISS | unknown | 0 | 137 | 2368 | — |

## 2. Performance — get_context budget sweep (3× )

| budget | median ms | p95 ms |
|---|---|---|
| 500 | 1481 | 1634 |
| 2000 | 832 | 958 |
| 4000 | 1131 | 1265 |
| 8000 | 805 | 955 |

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
