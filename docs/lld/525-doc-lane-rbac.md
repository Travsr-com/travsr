# LLD 525-1: the docs retrieval lane bypasses `EdgeFilter`

Covers item 2 (doc-lane RBAC bypass) and item 5 (docs-default egress
documentation) of #525.

## Problem

`get_context_body` receives `filter: &dyn EdgeFilter` and threads it through
every code-lane stage. The docs lane runs beside that pipeline and never sees
it, so on the day Graph RBAC ships a scoped session gets scoped code results
and unscoped doc results.

## Root cause

The docs lane does not reach nodes by traversing edges. It enters the store
through a second index that graph traversal never covers:

- `crates/travsr-mcp/src/tools.rs:4944` `build_docs_section(store, query, token_budget)` has no filter parameter.
- `crates/travsr-mcp/src/seed.rs:887` `doc_lane_candidates` takes candidates straight from `store.embed_doc_knn_fn()` (the doc-space HNSW), which yields `NodeId`s with no source node and no edge.
- Both call sites hand it no scope: `tools.rs:5333` inside `get_context_body`, and `query.rs:318` (`docs_section`) inside `ask_query_with_filter`, which already holds a `filter`. The issue names only the first.

So the true defect is not a forgotten argument on one function. It is that
RBAC in this codebase is enforced as an *edge* gate, while the docs lane is a
seed-producing entry point that admits nodes without any edge being crossed.
`EdgeFilter` is still the right enforcement point, because the code lane
already solved the identical problem for its own KNN and FTS seeds by calling
the filter in self-edge form on admission
(`seed.rs:2053`, `:2280`, `:2366`, `:2504`); the docs lane is the one seed
source that was never wired to it. Anything else would be a second, divergent
access-control mechanism.

Two limits worth stating, because the issue's framing implies more than the
fix delivers:

- `RbacFilter` gates on `vname.corpus` only. The issue's motivating example,
  `exclude_paths: ["docs/internal/"]`, is not expressible in `EdgeFilter`
  today, for code results either. This change makes doc results obey exactly
  the scope code results already obey, no more.
- `ask_query_with_filter` also passes `&OpenFilter` to `embed_path_seeds`
  and `build_seed_set` (`query.rs:373`, `:421`) while holding a real filter.
  That is the same class of bypass in the code lane and is out of scope
  here; reported on #525 rather than fixed in a security PR whose diff should
  stay reviewable.

## Options considered

1. **Post-filter the rendered entries in `build_docs_section`.** Rejected.
   `rbac.rs:3` states the project rule directly: post-filtering leaks the
   existence of restricted nodes. Denied nodes would still be fetched, still
   be fed to the cross-encoder, and still consume `docs_max_results` slots and
   budget, so their presence stays observable in what survives.
2. **Filter inside `store.embed_doc_knn_fn()`.** Rejected. The hook is a
   store-level closure shared with the daemon and has no session context; the
   filter belongs to the caller, not the index.
3. **A separate doc-scope predicate.** Rejected: a second access-control
   mechanism to keep in sync with the first, for no gain.

## Chosen design

`doc_lane_candidates` takes `filter: &dyn EdgeFilter` and drops denied
candidates immediately after the KNN call, before any ranking, text lookup or
rendering. `build_docs_section` takes the filter and forwards it; both call
sites pass the scope they already hold.

Corpus resolution follows the `needs_corpus` contract established for PPR
(`ppr.rs:344`): one batched `get_nodes` only when the filter's verdict can
depend on the corpus, so the local `OpenFilter` path costs exactly what it did
before. A candidate missing from the store resolves to `None`, which every
filter must treat as a denial.

Item 5 ships here as documentation only: the `get_context` tool description
marks doc paths and headings as untrusted author-controlled text, and the
README and the #519 CHANGELOG entry both state that the default-on docs lane
emits file paths and heading text. The issue asks for a threat-model row too;
this repository has no threat-model document (the T8/T11 rows live in a plan
file that is not committed), so there is no row to add, and the disclosure
goes where a user actually reads it instead.

## Why this is optimal here

It puts the check at the earliest point where the lane knows which nodes it is
about to use, matches the admission-time idiom the code lane already uses,
adds no query cost to the single-repo path, and leaves one enforcement
mechanism rather than two.

## Test plan

- `build_docs_section_never_renders_a_denied_corpus` (`tools.rs`): two doc
  chunks in distinct corpora, an `RbacFilter` admitting one, the denied chunk
  ranked first. Its path and heading must be absent from every rendered entry
  and the permitted one present.
- `doc_lane_candidates_drops_candidates_the_filter_denies` (`seed.rs`): the
  denied id must not survive the candidate pool, so nothing downstream can
  score it.
- `doc_lane_candidates_denies_a_candidate_missing_from_the_store`: fail-closed
  on an unresolvable corpus.
- The existing docs-lane tests keep passing with `&OpenFilter`, which is the
  unchanged-local-behaviour check.

## Security analysis

- **Threat.** A session scoped to a subset of corpora receives doc chunks from
  corpora it may not see. Egress is `path § Humanized Heading Trail:lines`:
  no prose body, but paths and headings are exactly the readable part.
- **Enforcement point.** Admission, before rerank and before `get_nodes` for
  rendering. A denied node's `embed_text` is never read and never reaches the
  cross-encoder.
- **Fail-closed.** Unknown corpus is `None`; `RbacFilter` denies. A store
  error during corpus resolution yields an empty map, hence `None` for every
  candidate, hence denial of the whole pool for a corpus-sensitive filter.
- **Side channels.** Denied candidates are removed before the floor, the
  `docs_max_results` cap and the budget carve, so they cannot displace an
  allowed entry or shift the token count. Timing still varies with pool size,
  as it already does everywhere else in retrieval.
- **Not fixed here.** `RbacFilter` cannot express path-level scope; the code
  lane bypass in `ask_query_with_filter` remains.

## Risks

- Behaviour change is nil for local single-repo use: `OpenFilter` admits
  everything and, via `needs_corpus() == false`, skips the lookup entirely.
- Under a corpus-scoped filter, doc chunks whose `corpus` is empty (the local
  indexer's default) are denied. That matches what the code lane already does
  with the same nodes, so the two lanes stay consistent.
- One extra batched `get_nodes` per docs-enabled query under a corpus-scoped
  filter, bounded by `doc_rerank_overfetch` (default 20).
