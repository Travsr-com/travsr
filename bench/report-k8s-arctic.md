# Travsr tool benchmark — `arctic-embed-m-v1.5-256`

_2026-07-03T13:23:36.041Z_ · repo: k8s-arctic · cold first get_context: **2697 ms**

## 1. Correctness (get_context, budget 2000, 3× median)

| metric | value |
|---|---|
| answerable queries | 16 |
| hit@1 | 0.313 |
| hit@3 | 0.375 |
| hit@5 | 0.438 |
| hit@10 | 0.5 |
| MRR | 0.358 |
| abstain accuracy (nonsense) | 0.667 (2/3) |

### Per-query

| id | cat | rank | conf | nodes | tok | ms | top result |
|---|---|---|---|---|---|---|---|
| L1 | literal | 1 | strong | 82 | 2963 | 265 | method:ReplicaSetController.syncReplicaSet |
| L2 | literal | 4 | strong | 94 | 4091 | 720 | method:Controller.processNextWorkItem |
| L3 | literal | 1 | weak | 16 | 493 | 661 | interface:SharedInformer |
| L4 | literal | MISS | unknown | 0 | 127 | 623 | — |
| L5 | literal | 1 | strong | 71 | 2801 | 293 | go-pkg:staging/src/k8s.io/client-go/tools/leaderelection/leaderelection |
| L6 | literal | 1 | strong | 44 | 1165 | 452 | class:Proxier |
| C1 | conceptual | MISS | unknown | 0 | 173 | 1363 | — |
| C2 | conceptual | 7 | weak | 67 | 1923 | 832 | class:nodes |
| C3 | conceptual | MISS | weak | 26 | 797 | 1473 | fn:Pod |
| C4 | conceptual | MISS | unknown | 0 | 190 | 1735 | — |
| C5 | conceptual | MISS | unknown | 0 | 156 | 1380 | — |
| C6 | conceptual | MISS | weak | 29 | 861 | 1532 | fn:Event |
| C7 | conceptual | 3 | weak | 37 | 1065 | 1624 | fn:Pod |
| C8 | conceptual | 1 | weak | 37 | 1104 | 1378 | fn:Endpoints |
| X1 | cross | MISS | unknown | 0 | 173 | 1470 | — |
| X2 | cross | MISS | unknown | 0 | 144 | 1277 | — |
| N1 | nonsense | abstain✓ | unknown | 0 | 167 | 997 | — |
| N2 | nonsense | salad✗ | weak | 83 | 3056 | 1498 | class:ComponentStatusApplyConfiguration |
| N3 | nonsense | abstain✓ | unknown | 0 | 160 | 1364 | — |

## 2. Performance — get_context budget sweep (3× )

| budget | median ms | p95 ms |
|---|---|---|
| 500 | 859 | 931 |
| 2000 | 755 | 757 |
| 4000 | 920 | 934 |
| 8000 | 805 | 808 |

## 3. Performance — get_snippets modes (3× median)

| target | mode | median ms | bytes | ~tokens |
|---|---|---|---|---|
| small_fn | auto | 88 | 368 | 92 |
| small_fn | full | 87 | 368 | 92 |
| small_fn | skeleton | 92 | 504 | 126 |
| large_fn | auto | 90 | 1786 | 447 |
| large_fn | full | 86 | 4360 | 1090 |
| large_fn | skeleton | 90 | 1786 | 447 |
| class | auto | 106 | 2730 | 683 |
| class | full | 96 | 1606 | 402 |
| class | skeleton | 112 | 2730 | 683 |

## 4. Usefulness (LLM-judge)

_Judge packets in `judge-packets-k8s-arctic.json`. Scored separately — see analysis below._

## 5. Analysis — raw recall vs grounded recall (the real story)

The correctness table above counts only **grounded** nodes (lines tagged `[via …]`).
When get_context decides a query "isn't grounded", it prints
`[note: no grounded match for this query in this repo]` and demotes the top
candidates to a **"speculative guesses"** list with no `[via]` tag — so the harness
parser scores them as 0-node MISS **even when the correct symbol is right there**.

Direct MCP re-probe of all 16 answerable queries, checking whether the expected
symbol appears **anywhere** in the output (grounded or demoted-guess):

| query | grounded? | expected symbol present anywhere? | verdict |
|---|---|---|---|
| L1,L2,L3,L5,L6 | Y | Y | correct + confident |
| **L4** (`Scheduler ScheduleOne`) | **n** | **Y** | **over-abstention** — `method:Scheduler.ScheduleOne` is literally guess #2, demoted |
| C2,C7,C8 | Y | Y | correct + confident |
| **C4** (leader election) | **n** | **Y** | **over-abstention** — right file demoted to guess |
| C3,C6 | Y | n | confident on the **wrong** node (salad) |
| C1,C5,X1,X2 | n | n | genuine retrieval miss (concept/cross) |

- **raw recall (expected present anywhere) = 0.625 (10/16)**
- **grounded rate = 0.625 (10/16)** — but a *different* 10: L4+C4 are demoted, C3+C6 are grounded-but-wrong.

### Two distinct failure modes on k8s (131,873 nodes)

1. **Over-abstention / demotion** (L4, C4). The correct node is retrieved but the
   RFC-019 `confirm_anchor_floor` (0.66) rejects it, so get_context downgrades the
   whole answer to "speculative." This fires even on a **literal symbol query**
   (`Scheduler ScheduleOne`). Root cause: arctic-embed-**256** (Matryoshka-truncated)
   produces lower absolute cosines than the bge-small/full-dim corpus the floor was
   tuned on → the fixed floor is too strict at 256-dim × 131k nodes. This is the
   R1 relevance-floor tension swinging the *other* way (over-abstain instead of salad).
2. **Conceptual / cross recall genuinely weak** (C1, C3, C5, C6, X1, X2). KNN doesn't
   surface concept→symbol on a large, diverse corpus; cross-file queries (X1, X2) miss
   entirely. Matches the known "conceptual recall is the weak spot" hypothesis — far
   more pronounced at 131k nodes than at travsr's 4k.

### Caveat on the prior "arctic 0.938 hit@1" number
That was **model-pure ORT top-1 KNN** on 4,114 travsr symbol nodes (16 queries). This
bench is the **full get_context pipeline** (KNN → seeds → PPR → knapsack → grounding
gate) on a **32× larger** corpus with **256-dim** vectors. The gap (0.938 → 0.31
grounded / 0.63 raw) is the pipeline + truncation + scale tax, **not** arctic KNN
quality per se — literals still retrieve 6/6 raw.

### Perf verdict (unambiguous win)
Warm get_context 0.75–0.92s across budgets, cold 2.7s, get_snippets 86–112ms on a
131k-node / 147 MB-HNSW repo. No perf regression from the bigger corpus.

### Actionable takeaways
- **Floor is model-relative, not absolute.** `confirm_anchor_floor` must scale with the
  active model's cosine distribution (or be calibrated per-model/per-dim), else arctic-256
  over-abstains on large repos. Fixing this alone recovers L4 + C4 → grounded ≥ 0.75 on literals.
- **256-dim truncation is suspect at scale.** Re-run with full **arctic-768** to separate
  "truncation cost" from "floor tuning" before shipping 256 as default on big repos.
- Cross-file (X) recall needs PPR expansion over call edges, not KNN alone.
