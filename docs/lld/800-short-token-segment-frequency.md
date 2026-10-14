# LLD 800: short-token frequency measures segment membership

Issue: #800. Parent: #778. Narrowed by: #791.

## Problem

`SqliteStore::symbol_frequency` answers "how many nodes does this token match?",
the IDF denominator that decides whether an anchor grounds or abstains
(`travsr-mcp/src/seed.rs:2220`). For a token with at least one 3-char segment it
reads `nodes_words_vocab`, which measures **segment membership**. For a token
whose every segment is shorter than 3 bytes it fell back to
`exact_leaf_name_count`, which measured **leaf equality**.

Two identically shaped corpora therefore measured on different scales:

| corpus | token | measured |
| --- | --- | --- |
| 300 `method:Widget{i}.user_key` + `class:Key` | `key` (3ch) | 301 |
| 300 `method:Widget{i}.user_id` + `class:Id` | `id` (2ch) | 1 |

Both tokens admit all 300 compound members to `anchor_pool`
(`ident::contains_token` is true for both), so the short token grounds `Weak`
where the long one abstains, decided purely by token length.

## Root cause

Not "the fallback picked the wrong SQL predicate". The fallback exists at all
because `symbol_frequency` reuses a **search** index as its **document-frequency
oracle**, and that index is deliberately blind to short segments.

`Self::node_fts_words` (`crates/travsr-store/src/lib.rs:4925-4937`) drops every
segment shorter than 3 bytes before writing `nodes_fts_words`:

```rust
let sig = travsr_core::ident::segments(&node.vname.signature)
    .into_iter()
    .filter(|t| t.len() >= 3)
```

That filter is correct for its own job: `nodes_fts_words` is Leg B of the fused
lexical search (`fts_query_words_scored`, `lib.rs:6048`), and `"fn"`, `"id"`,
`"ok"` as BM25 terms are near-universal noise. `nodes_words_vocab`
(`migrations/v21_lexical_split.sql:25`) is an `fts5vocab` view over that same
table, so the frequency measure inherits a filter chosen for retrieval
precision. Every short token is unmeasurable there, so #791 (`c690cae`) bolted a
second scale onto the same function rather than fixing the coupling.

Leaf equality was then a reasonable choice for #778's motivating case
(`class:UI`, where the token *is* the whole name) and a wrong one for every
compound member name, which is the case #800 reports. The issue's surface
diagnosis ("three notions of match are in play") is accurate as a symptom, but
its **proposed fix is unsafe**: indexing 2-char segments in `nodes_fts_words`
would put `id`/`ok`/`fn` back into the Leg B BM25 term space, which is exactly
what the write-time filter is documented (`lib.rs:4920-4924`) to prevent, and
what the trigram leg's own `len >= 3` filter has always prevented. The frequency
measure's problem must not be paid for out of the search leg's precision.

## Options considered

1. **Index 2-char segments in `nodes_fts_words`** (the issue's proposal).
   Rejected: regresses Leg B. `fts_query_words_scored` matches unqualified
   across columns, and short terms would then match near-universally at BM25
   rank; the filter removed exists precisely to stop that. Also a schema bump
   plus a full `nodes_fts_words` backfill for every existing database.

2. **Add a third `short` column to `nodes_fts_words` holding the dropped
   segments.** The term spaces are disjoint (nothing >= 3 bytes lands in it) and
   Leg B pre-segments queries with the same `len >= 3` filter, so no false
   matches. Rejected anyway: a third column changes FTS5 document lengths and so
   perturbs `bm25(nodes_fts_words, W_SIG, W_PATH)` ranking corpus-wide for a
   measurement that is never searched, and it still costs a migration and a full
   backfill. Large blast radius for a cold path.

3. **Keep the index untouched; make the fallback measure the predicate that
   actually admits anchors** (chosen).

## Chosen design

Replace `exact_leaf_name_count` with `token_segment_node_count`: a bounded scan
of `nodes.signature` counting rows for which
`travsr_core::ident::contains_token(token, signature)` holds.

`contains_token` is the same predicate `build_seed_set` uses to build
`anchor_pool` (`travsr-mcp/src/seed.rs:2251`) and the same segmenter
(`ident::raw_segments`) that populates the vocab. So the short-token count now
answers "how many nodes would this token admit", which is what the IDF
denominator is for. The number of scales drops from three to one predicate
(`ident::segments`) expressed two ways: an O(1) vocab lookup where the index
carries the term, and a direct evaluation where it deliberately does not.

Everything else is preserved verbatim from #791: signature-only (a path-only
match is not an anchor), `kind != 'doc-chunk'`, the 4096 cap, and saturation to
`total_node_count()` at the cap so a truncated count floors IDF to generic
instead of reading as specific. `LEAF_NAME_COUNT_CAP` is renamed
`SEGMENT_MATCH_COUNT_CAP` to match what it now bounds.

## Why this is optimal here

- Zero schema change, zero backfill, no reindex: the fallback is already an
  unindexed scan of `nodes` (`LIKE '%:' || tok` cannot use
  `V19NodesSignatureIdx`), so this is the same cost class on the same cold path.
- Reuses `travsr_core::ident`, the module whose whole stated purpose
  (`ident.rs:1-10`) is being the single source of truth for "what words is this
  identifier made of", instead of adding a fourth notion of matching.
- The one index that exists keeps the semantics it was tuned for. Option 1 and 2
  both spend Leg B ranking quality on a measurement that is never searched.

## Test plan

`crates/travsr-store/tests/lexical_split.rs`:

- `symbol_frequency_short_token_counts_compound_members_like_vocab_does`: the
  issue's two corpora side by side, asserting `freq("id") == freq("key") ==
  Some(41)`. Fails before the change with `Some(1)` vs `Some(41)`.
- `symbol_frequency_short_token_counts_segments_not_substrings`: `parseId` and
  `parse_id` both count, `identifier` does not. Fails before with `None`, since
  neither compound is a leaf named `id`.
- Existing tests pin the behaviour that must not move:
  `symbol_frequency_short_token_grounds_on_exact_symbol` (unique `class:UI`
  stays `Some(1)`, `QZ` stays `None`), `..._counts_qualified_members_as_generic`
  (`Some(41)`), `..._saturates_at_cap` (`Some(4097)`), `..._none_for_short_token`,
  and the `wal`/`walk`, `works`/`workspace` vocab-path tests.
- `cargo test -p travsr-store`, `cargo test -p travsr-mcp` (seed-path consumers),
  `cargo clippy --all-targets -- -D warnings`, `check-em-dash.sh`.

## Risks

- **Scan cost.** Per candidate row the check is now Rust segmentation rather
  than a SQL `LIKE`, so the constant factor rises on a full-corpus scan. It is
  bounded the same way as before (stop at 4096 matches) and reached only when
  every segment of a query token is shorter than 3 bytes. The leaf-name index
  deferred on the #791 perf thread would not have helped this predicate anyway;
  if this path ever becomes hot, the fix is a short-segment posting list, not a
  narrower predicate.
- **Counts rise for short tokens.** By design: a short token that is a segment
  of many compound names is now generic and stops grounding. That is the
  abstain-side move, consistent with what the 3-char path already does; it
  cannot promote a token to a rare anchor that was not one before, because the
  new count is a superset of the old one for every token except a leaf match
  that is not a segment match, which cannot exist (a leaf is always a segment
  run of its own signature).
- **Path words.** The vocab path counts a term appearing in `path` as well as
  `signature`; this fallback stays signature-only, matching the anchor gate. The
  residual asymmetry is a slight under-count for short tokens relative to the
  vocab path, in the conservative direction, and closing it would mean
  contradicting `anchor_pool`'s signature-only rule.
