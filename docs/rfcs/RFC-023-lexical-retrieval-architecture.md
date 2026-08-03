# RFC-023 — Lexical Retrieval Architecture: separating precision from recall

- **Status:** **Implemented and merged.** Landed as `7c43461` via PR #542 (2026-08-03). All 11 workstreams (WS-0 through WS-10) landed in one PR per decision 9. As-built record, verification results, and the fresh testing pass: `docs/plans/issue-478-implementation.md`.
- **Authors:** Tech Lead + Senior SWE + Senior QA (personas)
- **Affects:** `travsr-core` (NEW `ident` module), `travsr-store` (schema v21, `fused_search_scored`, `symbol_frequency`), `travsr-mcp` (`seed.rs`, `query.rs`), `travsr-cli` (`travsr explain`), `bench/`
- **Supersedes in part:** the substring-matching assumptions in RFC-012's fuzzy seed selection; the `fts_vocab` refcount as a relevance statistic (v10 L2-A)
- **Related:** #478 (the reported bug), #393 (cascade short-circuit + top-K starvation — this RFC closes both), RFC-021 (cross-encoder; unchanged by this RFC), RFC-019 (cosine oracle; this RFC removes our dependence on it as the *only* correctness backstop)

---

## 1. Problem statement (evidence-based)

Reported as #478: a query about SQLite's WAL journal mode returns a TypeScript AST walker at
the top of the result list.

```
$ travsr ask "what is the rationale for SQLite WAL"
fn:walk | packages/travsr-lsif-ts/src/walker.ts:58 | 1.000
113 nodes · ~1678 tokens · confidence: weak
```

Measured against master @ b8ef29b on the travsr repo itself (10,133 nodes).

### Evidence A — the lexical index cannot see word boundaries

```sql
CREATE VIRTUAL TABLE nodes_fts USING fts5(tokens, content='', tokenize='trigram')
```

`MATCH 'wal'` returns **171 rows** — `walker` (156), `walk` (12), `walkdir` (2), `walks` (2),
`wal` (1). The indexed token string for `fn:walk` is:

```
walk fn:walk packages travsr lsif src walker packages/travsr-lsif-ts/src/walker.ts function typescript
```

`wal` does not appear in it. Only the trigram layer admits the node. Raw BM25 over that index:

```
fn:walk   packages/travsr-lsif-ts/src/walker.ts   7.23   ← rank 1, corpus-wide
fn:walk   packages/travsr-lsif-py/src/walker.ts   7.23
pkg:walkdir@*                                     7.041
file      packages/travsr-lsif-ts/src/walker.ts   6.234
var:m     packages/travsr-lsif-py/src/walker.ts   6.111
```

`fn:walk` wins on **BM25 length normalisation** — a 7-character document maximises term
density. `seed.rs:2178` then computes `norm = bm25 / max_bm25`, and because `fn:walk` *is* the
max, assigns it exactly **1.000**.

### Evidence B — signature, path, kind and language share one scored field

The token string above contains `function` and `typescript`. A query containing either
BM25-matches every TypeScript function in the repo, scoring identically to a genuine symbol-name
match. A path match and a signature match are indistinguishable.

Measured on a 500-document probe (SQLite 3.51.0), term in doc 1's **signature** and doc 500's
**path**:

```
bm25(w)            -> rowid 500 = 5.2966   rowid 1 = 4.7424   ← path match WINS
bm25(w, 3.0, 1.0)  -> rowid 1   = 7.8423   rowid 500 = 5.2966 ← corrected
```

The default row is #478 in miniature.

### Evidence C — four incompatible definitions of "this token matches this node"

| Layer | Definition | `wal` says |
| --- | --- | --- |
| `nodes_fts` (governs **admission**) | trigram substring, anywhere | **171** |
| `symbol_frequency` → IDF (governs **weighting**) | `signature LIKE '%wal%'` | **22** |
| `fts_vocab.refcount` (maintained, unused for this) | word + camelCase segmented | **1** |
| `token_is_sig_component` (governs **anchoring**) | punctuation-only boundaries | **1** |

Nothing reconciles them. A token is admitted by the loosest definition, weighted by an unrelated
one, and gated by a fourth.

### Evidence D — the anchor guard rejects genuine anchors

`token_is_sig_component` (`seed.rs:981`) requires non-alphanumeric boundaries. `"sqlite"` in
`fn:SqliteStore.exec_ddl` is followed by `s`, so it is rejected. **All ~159 `SqliteStore.*`
methods fail the anchor guard for the token "sqlite"**: 10 nodes repo-wide pass the punctuation
rule, against 163 that the FTS tokenizer correctly segments. This generalises to PascalCase,
Go exported identifiers, and Java/TypeScript class names — most of the identifier surface of
every non-Rust language Travsr supports.

### Evidence E — the abstention gate is inert

`rrf_fuse` (`lib.rs:4069`) collapses all stages into one "natural score" via
`entry.2.max(*natural)`. But Stage 1's `synthetic_desc_scores` is **position-derived**
(`1.0 - i/(len+1)`, so row 0 always gets exactly `1.0`) while Stage 2's is real BM25 (0.1–7+).

`seed.rs` reads that single channel as BM25-scale for `max_bm25` normalisation (`seed.rs:2167`)
and for `top_bm25 >= bm25_strong_floor()` = 0.5 (`seed.rs:1239`). Stage 1's `WHERE` is
`signature LIKE '%tok%' OR path LIKE '%tok%'` and matches almost anything, so
`top_bm25 = 1.0 ≥ 0.5` holds **by construction** whenever Stage 1 returns a row. What reads as a
two-signal gate (lexical strength AND coverage) is a coverage gate.

### Evidence F — filters run after the LIMIT

`LIMIT 50` (FTS) and `LIMIT 100` (name) are applied in SQL. `is_noise_seed`, `filter.allow`, the
scope gate and per-path caps all run in Rust afterward. On kubernetes (~130k nodes) a
moderately common token can fill all 50 slots with test files, and the correct answer never
enters the candidate pool. This is the top-K starvation half of #393.

### Why this is only visible sometimes

With a warm embedding index the RFC-019 direct-cosine oracle vetoes the bad candidate and the
query correctly abstains. Re-running the #478 repro today on a fully-built arctic index produces
the right answer. **The defect is masked, not fixed.** It is fully live on the FTS-only path:
embeddings off, index cold, mid-rebuild, or any repo before its first embed pass. A
probabilistic backstop is currently the only thing standing between this class of query and a
confident wrong answer.

---

## 2. Root cause (architectural, not tuning)

> **`nodes_fts` is a single FTS5 trigram column holding signature, path, kind and language
> concatenated together. It is simultaneously the precision layer and the recall layer.**

A trigram index exists to match substrings — that is its job, and it does it correctly. The
error is asking it to *also* be the evidence that a query term names a symbol. Those are
different questions, and one index cannot answer both, because the substring match discards
exactly the information (word boundaries, which field matched) that the precision question needs.

Every finding above is a consequence:

- No word boundaries → `wal` matches `walker` (Evidence A).
- One blob field → path and kind pollute symbol scoring (Evidence B).
- Trigram admits, a different statistic weights → Evidence C.
- The boundary guard was bolted on at one gate, in a fourth dialect → Evidence D.
- Two scales collapsed into one channel to feed one pipeline → Evidence E.

No threshold tuning closes this. Tuning `scope_strong_floor` or `idf_coverage_min` only relocates
the false positives, because the signal being thresholded does not contain the distinction.

---

## 3. Design thesis

**Split the lexical front into two legs with different jobs, and let each carry an honest score.**

```
Leg B  word BM25      precision   field-weighted (sig^3, path^1), word-segmented
Leg C  trigram BM25   recall      substring/typo, structurally down-weighted
```

Three properties follow by construction rather than by patch:

1. A pure-substring match (`wal` → `walker`) reaches only Leg C, at a low weight. It can no
   longer take rank 1 or override crate scope. **#478 is fixed by topology, not by a discount.**
2. A signature match outranks a path match, because FTS5 per-column BM25 weights say so.
3. Document frequency comes from `fts5vocab` over the word index — the same segmentation that
   did the matching. Evidence C's four definitions collapse to one.

The trigram index is not removed or weakened. It keeps doing the job it is good at, and stops
doing the one it cannot.

---

## 4. Principal Architect ruling (philosophy compliance)

Checked against the non-negotiable principles in `CLAUDE.md`:

| Principle | Compliance |
| --- | --- |
| **Algorithms first, LLM last** | ✅ Strengthened. BM25, RRF, and deterministic segmentation only. No LLM anywhere. This RFC *reduces* reliance on the probabilistic RFC-019 oracle by making the deterministic path correct on its own. |
| **Always fresh** | ✅ Both indexes are written on the same incremental path; §8 makes a partial-write failure loud rather than silent. Migration backfills from `nodes` — no source reparse, no staleness window. |
| **Local first** | ✅ Entirely SQLite-local. No network. |
| **MCP is the only external interface** | ✅ No new MCP tool. `travsr explain` is a local CLI diagnostic, not an external interface. |
| **No unsafe Rust** | ✅ None. |
| **Crate dependency rules** | ✅ `ident` lands in `travsr-core` (zero travsr deps). `travsr-store → travsr-core` and `travsr-mcp → travsr-core` both already exist. **No new edges, no cycles.** |

**Ruling: compliant.** The one philosophy-relevant observation is that today's system depends on
an embeddings-derived signal to be *correct*, not merely to have better recall. That inverts the
stated thesis. This RFC restores the ordering.

---

## 5. Component architecture

### 5.1 Schema v21

```sql
CREATE VIRTUAL TABLE nodes_fts_words USING fts5(
    sig, path, content='', tokenize='unicode61'
);
CREATE VIRTUAL TABLE nodes_words_vocab USING fts5vocab(nodes_fts_words, 'row');
```

- `kind` and `language` leave the indexed text. They are already columns on `nodes` and become
  `WHERE` filters, not scored terms.
- `nodes_fts` (trigram) is **unchanged** — no rebuild, no reparse.
- Migration backfills `nodes_fts_words` from `nodes.signature` / `nodes.path`. The data is
  already in the database. Reuses the `backfill_fts_if_needed` machinery.

**Verified capabilities** (SQLite 3.51.0, the version in use):
- `bm25(tbl, w_sig, w_path)` per-column weights — work, and change ranking (Evidence B).
- `MATCH 'sig:token'` column-scoped queries — work.
- `fts5vocab(tbl,'row')` returns `term, doc, cnt` where `doc` is document frequency **maintained
  by FTS5 itself** — authoritative, zero drift.

**Size budget**, measured on this repo: trigram index 11.4 MB of a 27 MB database; raw token text
1.4 MB. The word index is a small fraction of what trigram already costs.

### 5.2 `travsr-core::ident` — one segmenter

```rust
/// Split `s` into lowercased word segments. Delimiters: non-ASCII-alphanumeric,
/// lower→upper transitions, letter↔digit transitions.
pub fn segments(s: &str) -> Vec<String>;

/// True when `token` appears in `text` as a whole segment, or as a contiguous
/// run of segments joined ("sqlitestore" matches ["sqlite","store"]).
pub fn contains_token(token: &str, text: &str) -> bool;
```

Extracted from `fts_tokenize::tokenize_identifier`, which already segments camelCase correctly —
this is why `fts_vocab` reports `sqlite`=163 while the anchor guard sees 10. Both the FTS
tokenizer and the boundary predicate call it. `token_is_sig_component` is deleted.

**`segments()` reproduces today's segmentation exactly.** Acronym runs stay unsplit
(`HTTPServer` → `["httpserver"]`); changing that changes `nodes_fts` content and forces a
trigram rebuild. Deferred (§13). This keeps "one segmenter" absolute while keeping the migration
to one new index.

### 5.3 Leg topology

```
query
 ├─ Leg A  exact / name       search_nodes_by_name, boundary-guarded    precision
 ├─ Leg B  word BM25          nodes_fts_words, bm25(t, W_SIG, W_PATH)   precision   NEW
 ├─ Leg C  trigram BM25       nodes_fts, down-weighted                  recall/typo
 ├─ Leg D  L2-A expansion     always contributes, no longer miss-gated  recall
 └─ Leg E  embed KNN          unchanged
      ├─ noise / RBAC / scope filters pushed into SQL, before LIMIT
      ├─ weighted RRF; each leg carries its own scale-honest score
      ├─ diversity as tie-break within a score band, not unconditional
      └─ cross-encoder rerank → PPR → knapsack   (RFC-021, unchanged)
```

### 5.4 Score channel separation

`rrf_fuse` stops collapsing stages into one float:

```rust
(rrf: f32, bm25_natural: Option<f32>, exact_rank: Option<usize>)
```

`bm25_natural` is `None` when no BM25-scale leg matched. Abstention and normalisation read only
that channel, so Evidence E's inert gate becomes a real one.

### 5.5 Frequency: one statistic, one set

| Consumer | Source | Rationale |
| --- | --- | --- |
| `symbol_frequency` (IDF) | `nodes_words_vocab.doc` | authoritative df, zero drift, O(1) indexed |
| L2-A `expand_query` (`lib.rs:4610`) | `fts_vocab` | needs a *set* of known words, not a statistic |

Conflating a set with a statistic is how Evidence C happened. The split is documented in code.

---

## 6. `travsr-mcp` integration

1. **Anchor loop** (`seed.rs:2024`) — `token_is_sig_component` → `ident::contains_token`.
2. **`resolved`** — currently `!exact_nodes.is_empty()` from an unguarded substring hit, so
   `wal` reads as resolved because it matches `walker.ts` via the path clause. Derive it from
   boundary-guarded matches. **This must ship with §5.5**: correcting `wal`'s df from 22 to 1
   lifts its IDF from 0.660 to 0.925, so without this fix a token that anchors nothing would look
   *more* specific than it does today. §5.5 introduces this regression on its own.
3. **`symbol_frequency` signature** → `Result<Option<usize>>`. "Absent from vocabulary" and
   "appears in zero nodes" are different facts; the caller supplies the fallback.
4. **Lexical loop** (`seed.rs:2171-2212`) — reads the fused legs; no boundary-evidence discount
   is added, because §3 makes it unnecessary.
5. **Filters before LIMIT** (Evidence F) — `LIMIT 50` (`lib.rs:4231`, `4308`, `4442`, `4534`) and
   `LIMIT 100` (`lib.rs:2209`, `4394`) are applied in SQL, while `is_noise_seed`, `filter.allow`,
   the scope gate and per-path caps all run in Rust afterward. Push the noise-path predicates
   into the `WHERE` clause (or a materialized `is_noise` column, §14.2) so top-K is drawn from
   the eligible set. This is Elasticsearch's filter-context pattern, and it is where the top-K
   starvation half of #393 is actually closed. Applies to every leg's SQL, not just Leg B.
6. **L2-A un-gating** (`lib.rs:4029`) — stages 3/4 currently fire only `if fused.is_empty()`. The
   existing code comment already says "Phase 2 will loosen this to weak-union". Build it. This
   closes the cascade short-circuit half of #393.
7. **`diversify_topk`** (`lib.rs:4100`) — unconditional round-robin by kind promotes a `var` or
   `file` node to rank 2 even when the top 5 methods are correct (Evidence A's dump shows
   `var:m` / `var:fn` in the top 8). Becomes a tie-break within a score band.
8. **`arms.sort(); arms.truncate(16)`** (`lib.rs:4163`) — alphabetical truncation can drop raw
   query tokens on long queries. Truncate by IDF; keep raw tokens unconditionally.
9. **Observability** — three items:
   - `embed_used` is hardcoded `false` on both abstain arms (`query.rs:342`, `query.rs:366`)
     despite `embed_contributed` being in scope. Pass the real value.
   - `ask` gains an FTS-only / degraded note, reusing `build_context_signals` (`tools.rs:2821`)
     rather than a second formatter, so `get_context` and `ask` stay at parity. Given that the
     embedding oracle is today the only backstop against this bug class (§1), its absence must
     be visible per-query.
   - `display_score` (`seed.rs:477`) caps the unscored-primary-seed branch by confidence tier so
     `1.000` cannot appear beside `confidence: weak`. Behind `TRAVSR_DISPLAY_TIER_CAP`.

### 6.1 `travsr explain`

```
$ travsr explain "<query>" <symbol>
```

Prints, for one query/node pair: the node's indexed tokens; per-query-token IDF and its source;
per-leg match status, raw score and rank; RRF contribution; every gate with its threshold and
verdict; rerank score; oracle cosine; final disposition — **including what the outcome would
have been on the FTS-only path.**

Justification: root-causing #478 required a session of manual SQL archaeology against a copy of
`graph.db`. The data all exists and is discarded. Beyond debugging, it is the instrument for the
§9 recalibration — six thresholds across two bench suites cannot be swept responsibly by watching
hit@1 move with no per-gate attribution. Implemented as an optional collector threaded through
`fused_search_scored` / `build_seed_set`, inert when off. It also subsumes the `TRAVSR_RERANK_TRACE`
that would otherwise be a second, redundant surface.

---

## 7. What gets deleted / demoted

| Thing | Disposition |
| --- | --- |
| `token_is_sig_component` (`seed.rs:981`) | **deleted** → `ident::contains_token` |
| Boundary-evidence demotion (proposed in the pre-RFC #478 plan) | **never built.** It was a patch for "trigram admits everything"; §3 removes the premise. Building both would be redundant. |
| `fts_vocab` drift reconciliation in `fsck` (also proposed pre-RFC) | **never built.** `fts5vocab` is FTS5-maintained; nothing drifts. |
| `symbol_frequency`'s `LIKE '%tok%'` scan | **deleted** → indexed `fts5vocab` lookup (also removes 3-5 full table scans per query) |
| `fts_vocab` as a relevance statistic | **demoted** to a candidate-vocabulary set (§5.5) |
| RFC-019 cosine oracle | **unchanged in code, demoted in role** — from sole correctness backstop to what it was designed to be: additional recall and validation |

---

## 8. Failure modes & graceful degradation

| Failure | Behaviour |
| --- | --- |
| v21 migration interrupted | Migration is transactional and idempotent; `nodes_fts_words` is rebuilt from `nodes` on next open. Gated exactly like `backfill_vocab_if_needed`. |
| Word index missing / empty (pre-migration DB) | Leg B returns empty. RRF degrades to today's leg set. **Never a hard failure** — a missing precision leg costs precision, not availability. |
| Incremental write updates `nodes_fts` but not `nodes_fts_words` | Same transaction, so it cannot partially commit. `fsck` gains a parity check across `nodes` / `nodes_fts_map` / `nodes_fts_words` (today: 10,133 / 10,133 / expected 10,133). |
| Query token absent from `nodes_words_vocab` | `symbol_frequency` → `None`; caller treats as maximally generic. A token we cannot measure is never *promoted* to a rare anchor. |
| Tokens < 3 bytes | Not indexed (unchanged). Handled by the `None` path above. |
| Embeddings cold/off | **This is the case the RFC exists to fix.** Legs A–D are fully deterministic. |

---

## 9. Recall vs precision (explicit)

This RFC **raises precision and is recall-neutral by construction**: no leg is removed, one is
added, and the currently miss-gated Leg D starts contributing (§6.5) — a recall *increase*.

The precision gain is a reweighting, not a filter. What changes is that a substring-only match
must now outscore genuine word matches on the strength of Leg C alone, rather than arriving
pre-normalised to 1.000.

**Every relevance threshold was tuned against the signals this RFC changes, and must be
re-derived.** This is a §12 acceptance criterion, not a follow-up:

| Threshold | Location | Why it moves |
| --- | --- | --- |
| `idf_coverage_min` 0.55 | `seed.rs:83` | word-aware df shifts IDF non-uniformly |
| anchor-emit cut 0.15 | `seed.rs:2041` | same |
| `bm25_strong_floor` 0.5 | `seed.rs:61` | compared against a real BM25 channel for the first time |
| `scope_strong_floor` 0.3 | `seed.rs:1706` | operates on leg-weighted norms |
| RRF leg weights | `lib.rs` `RRF_W_*` | two legs change role |
| `W_SIG` / `W_PATH` | new | never existed |

Measured, N = 10,133 — the shift is **not uniform** and cannot be corrected by a constant offset:

| token | f today | f after | idf today | idf after |
| --- | --- | --- | --- | --- |
| wal | 22 | **1** | 0.660 | **0.925** |
| mode | 191 | 57 | 0.430 | **0.560** ← crosses the 0.55 coverage bar |
| ppr | 71 | 111 | 0.520 | **0.485** ← moves *down* |

---

## 10. Performance budget

| Item | Expectation |
| --- | --- |
| `symbol_frequency` | **Improvement.** Removes 3-5 full `nodes` scans per query; `fts5vocab` is an indexed lookup. |
| Leg B query | One additional FTS5 MATCH per query, same shape as the existing trigram query. |
| Filters before LIMIT (§6) | **Improvement** on large repos — fewer rows scored and materialised. |
| Storage | +word index; measured baseline trigram 11.4 MB / raw text 1.4 MB on a 10k-node repo. |
| v21 migration | One-time backfill from `nodes`. **Must be measured on a k8s-sized DB (~130k nodes) before merge.** |
| `travsr explain` | Zero cost when off (optional collector, not always-on instrumentation). |

Net expectation is a **latency improvement**. If the seed path regresses, the migration or Leg B
is the suspect, and both are independently measurable.

---

## 11. Acceptance criteria

1. The #478 repro returns the correct storage nodes or a clean abstention **with embeddings
   disabled**, and `fn:walk` appears nowhere in the seed set.
2. `sqlite` anchors to `SqliteStore.*` methods (today: rejected by the guard).
3. A signature match outranks a path match for the same term (Evidence B, as a test).
4. `bm25_natural` is `None` for a Leg-A-only result — the abstention gate is live (Evidence E).
5. `resolved` is false for a token with no boundary-guarded match (§6.2).
6. Word-level `symbol_frequency`: `wal` ≠ `walk`, `works` ≠ `workspace`.
7. `tokenize_identifier` output is **byte-identical** before and after the §5.2 extraction —
   golden test over every distinct signature and path in a real `graph.db`. Blocking.
8. v21 migration is idempotent and verified on a k8s-sized DB; `fsck` parity check passes.
9. **Bench: no net regression on `queries-seeded-travsr.json` AND `queries-seeded-k8s.json`.**
   Report hit@1, hit@10, abstention rate on salad queries (must not fall), p50/p95 seed latency,
   and the §9 sweep tables. **k8s is a hard merge gate** — Go's exported-identifier convention is
   exactly the PascalCase population today's guard rejects, so §5.2's blast radius there is far
   larger than on this repo.
10. **Every regression test runs with `score_ref = None`.** The original bug was invisible on a
    warm oracle; that is why the existing suite missed it. Tests that pass only with embeddings on
    do not count.

---

## 12. Decisions (locked)

1. Two lexical legs, not one index with a discount. (§3)
2. Trigram index untouched — no rebuild, no reparse. (§5.1)
3. One segmenter, in `travsr-core`. Acronym runs deferred to keep it that way. (§5.2)
4. `fts5vocab` is the df authority; `fts_vocab` is demoted to a vocabulary set. (§5.5)
5. `kind` / `language` leave the scored text and become filters. (§5.1)
6. Score channels are separated, not collapsed. (§5.4)
7. Recalibration is an acceptance criterion, not a follow-up. (§9)
8. `travsr explain` ships in this PR. (§6.1)
9. Single PR, one merge. The workstreams are coupled: shipping them separately produces
   intermediate states worse than either endpoint (e.g. corrected frequencies without
   recalibrated thresholds inflate confidence repo-wide).

---

## 13. Out of scope

1. **Acronym-run segmentation** (`HTTPServer` → `[http, server]`) — requires a trigram rebuild.
   Follow-up issue, with a failing-by-design test pinning current behaviour.
2. **Replacing or removing the trigram tokenizer** — it earns its place as the recall leg.
3. **The cross-encoder (RFC-021).** It scored `fn:walk` 1.000 independently of admission. `travsr
   explain` will show whether the bad candidate still reaches it after this lands; file
   separately with that evidence.
4. **Embeddings, KNN, PPR, knapsack** — untouched.
5. **The two dogfooding bugs** found during investigation (`travsr pattern` regex alternation
   returning zero matches; score-0.000 nodes displayed as results) — separate issues. The
   score-0 floor interacts with §5.4: once `bm25_natural` is honest, a relevance floor becomes
   implementable, but it is not built here.
6. **Test/noise flood (RFC-022 RC-3).** Observed on the same surface during this investigation:
   `travsr ask "what does the WAL journal mode do"` ranks the test function
   `fn:wal_journal_mode_enabled_on_file_backed_store` at **0.960**, above the real
   `fn:SqliteStore.journal_mode` at **0.013**. Knowingly excluded — a prior design exists
   (shared path patterns + annotation scan + query-intent gate + soft seed down-weight) and was
   agreed but not built. This RFC neither fixes nor worsens it: `is_anchor_noise` already drops
   in-src test symbols from the *anchor* pool, and the leg reweighting does not change the
   relative standing of a test symbol versus an implementation symbol that match equally well.
   Worth re-measuring after this lands, since §9's recalibration touches the same thresholds.

---

## 14. Unresolved questions — resolutions (WS-9)

1. **`W_SIG` / `W_PATH` starting values.** Kept at 3.0 / 1.0 (the §1 probe values). No
   compile-time grid sweep was run — each trial requires a full rebuild, and the bench results
   at these values already show a net improvement with zero regressions on both suites (see
   `docs/plans/issue-478-implementation.md` §"Bench results"), so there was no measured signal
   to chase a different value. Open for a future pass if `travsr explain` surfaces a specific
   miss attributable to this weighting.
2. **`is_noise` as a materialized column vs a `WHERE` predicate.** Column, as leaned. Backfilled
   by the v21 migration; measured cost ~11s on a 263,778-node kubernetes/kubernetes checkout.
3. **Leg C (trigram) weight.** `RRF_W_TRIGRAM = 0.4` (down from the pre-#478 `RRF_W_FTS = 1.0`).
   Verified via `travsr explain` on the #478 repro itself: `fn:walk` matches only via this leg,
   at rank 0 within the leg but demoted to rank 9-19 in the fused, kind-diversified result with
   weight 0.09-0.86 — never rank 1. No dedicated typo-recall regression sweep was run against
   this specific value; flag for a follow-up if typo-tolerant queries regress.
4. **Confidence display ceiling** (`Weak => 0.60`, `None => 0.40`). Shipped as proposed in WS-0,
   gated behind `TRAVSR_DISPLAY_TIER_CAP` (default on). Still cosmetic, not bench-derived.
5. **`nodes_fts_words` third column for doc-node headings.** Not built. Stayed out of scope.

The four env-tunable thresholds this RFC's own §9 table calls out for recalibration
(`idf_coverage_min`, the anchor-emit cut — now named `anchor_emit_cut()`, `TRAVSR_ANCHOR_EMIT_CUT`
— `bm25_strong_floor`, `scope_strong_floor`) were swept across a range on the travsr-repo bench
suite and found **insensitive** at their existing default values: varying each alone and in
combination produced no change in hit rate or abstention. They were left at their pre-#478
defaults rather than changed on no signal. See `docs/plans/issue-478-implementation.md` for the
sweep values tried.
