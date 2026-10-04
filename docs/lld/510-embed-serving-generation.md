# 510 - Key the warm `ask` cache on the embed sidecar's serving generation

## Problem

The daemon's warm query cache keys `ask` entries on `embed.db`'s
`PRAGMA data_version` (`crates/travsr-daemon/src/query_cache.rs:35`, fed at
`crates/travsr-daemon/src/lib.rs:12753`). #510 reports that this both misses
real changes and fires on non-changes.

## Root cause

The cache key is built exclusively from state the daemon itself reads **at
rest**: two SQLite pragmas plus the two commit markers
(`crates/travsr-daemon/src/lib.rs:12734-12762`). The semantic half of an `ask`
answer is not at rest. It is produced by a separate process's **in-memory** HNSW
index, reached through a best-effort hook (`EmbedSupervisor::knn_hook`,
`crates/travsr-plugin-host/src/embed_supervisor.rs:245`) that talks to
`EmbedSidecar::knn` (`crates/travsr-plugin-host/src/embed_sidecar.rs:383`) over
IPC. Nothing about which index is being served enters the key.

The concrete gap: a sidecar respawn swaps the served index in place inside the
`Arc` the hooks hold (`embed_supervisor.rs::maybe_respawn`, `#736 item 6`). The
replacement loads whatever `.travsr/<model>.hnsw.usearch` is on disk at that
moment. If no `embed.db` write falls between the two `ask` calls, every
component of the key is identical across the swap, so a warm entry computed
against the old index keeps being served. The same is true of a model switch,
which also routes through `maybe_respawn`. This is the failure #464 set out to
eliminate, reintroduced one process boundary away.

### Where the issue's own diagnosis is incomplete

The issue asserts "the daemon never reads embed.db to answer `ask`" and
concludes that `embed.db` should be **replaced** in the key. That is wrong.
`travsr_store::score_candidates` (`crates/travsr-store/src/lib.rs:189-202`)
opens `embed.db` read-only and reads `node_embeddings` directly, in-process, on
every `ask`; it is wired in as the RFC-019 cosine oracle at
`crates/travsr-daemon/src/lib.rs:12963-12977`. `embed.db` is a first-class
dependency of an `ask` answer, not a correlated proxy. Dropping it from the key
would trade the reported staleness bug for a new one on the oracle path.

The issue's second complaint, over-invalidation during reindex, is also not the
defect it is presented as. `data_version` moves on *any* write to the file, so
it is coarse, but that coarseness is exactly what the design already accepts for
`graph.db` (`query_cache.rs:5-9`), and it errs toward recomputation, never
toward staleness. A reindex genuinely does change stored embeddings and
therefore genuinely does change cosine scores; invalidating is correct, if
blunt. Making it finer is a performance question, not the correctness bug in
this issue.

So the true root cause is a **missing** dependency in the key, not a wrong one.

### Known residual, deliberately out of scope

An `ask` answered while the sidecar is cold, shed by `HOOK_QUEUE_DEPTH`, or past
`KNN_CALL_TIMEOUT_MS` silently degrades to FTS-only seeds
(`embed_supervisor.rs:296-303`) and is then cached under a key that says nothing
about the degradation. Once the sidecar warms, the key has not moved, so the
degraded answer is served until something else invalidates it. Fixing that needs
the *result* to carry whether semantic seeds were actually used, which is a
different change through `EmbedKnnHook`; it belongs in its own issue rather than
folded in here.

## Options considered

**A. Sidecar-owned generation returned over the wire** (the issue's proposal).
The ideal end state, and rejected as not realizable here. The plugin binary
(`travsr-embed`) is out of tree; this repo owns only the `EmbedPlugin` trait and
the runner (`crates/travsr-plugin-sdk/src/embed_runner.rs:49`). A new
`KnnResponse` field would default to a constant for every sidecar in existence,
so it would buy nothing until the external crate ships an implementation, and it
would need a `plugin_version` feature gate to distinguish "old sidecar" from
"generation 0" (the `supports_doc_space` pattern, `embed_sidecar.rs:79-90`). It
also costs a `KnnHook` signature change: the hook is
`Fn(&str, u32) -> Result<Vec<(NodeId, f32)>, StoreError>`, shared by
`travsr-store`, `travsr-mcp` and every test double, and the protocol comment at
`crates/travsr-plugin-protocol/src/embed.rs:128-131` records that `space` was
put on the *request* specifically to avoid touching it.

**B. Fingerprint the `.usearch` files from the daemon.** No wire change, but it
measures the wrong thing. The sidecar serves an in-memory index; a rebuilt file
on disk changes no answer until a sidecar loads it, and a loaded index is not
invalidated by its file being rewritten. It would both over- and
under-invalidate, for the same reason `embed.db` alone does.

**C. Host-owned serving generation (chosen).** The host already knows the one
event that swaps the served index: it performs it. `EmbedSupervisor::try_start`
and `EmbedSupervisor::maybe_respawn` are the only two places a sidecar starts
serving the daemon's hooks.

## Chosen design

A process-global monotonic counter in `travsr-plugin-host`, advanced at exactly
those two points and read by the daemon when it builds an `ask` cache key.

- `crates/travsr-plugin-host/src/embed_supervisor.rs`: `SERVING_GENERATION`
  (`AtomicU64`), advanced by `note_sidecar_now_serving()` on a successful
  `try_start` and on a successful `maybe_respawn`; exposed as
  `embed_serving_generation()`.
- `crates/travsr-daemon/src/query_cache.rs`: `DataVersions` gains
  `embed_generation: Option<u64>`, carried into `CacheKey`.
- `crates/travsr-daemon/src/lib.rs`: populated alongside the existing `embed`
  component, both now gated on one named predicate, `reads_embeddings(tool)`,
  rather than two copies of the same literal, so `graph`/`status` entries stay
  unkeyed on embed state and cannot drift apart from each other.

`embed.db`'s token stays in the key. The two components are complements:
`embed` covers the stored vectors both the oracle and the sidecar's ANN read,
`embed_generation` covers the in-memory index neither of them can see.

The issue's open question about distinguishing sidecar states falls out of this
without extra machinery: generation `0` means no sidecar has ever served in this
process, so `embed: None, embed_generation: Some(0)` is "no embeddings
configured" while `embed: Some(t), embed_generation: Some(n > 0)` is a live
embed repo. A sidecar that is currently down keeps the last generation and gains
a new one when it comes back, which invalidates every FTS-only answer cached
during the outage.

## Why this is optimal here

It is exact for the event that matters (the served index was replaced), it needs
no protocol change, no `plugin_version` gate, and no `KnnHook` signature churn,
and it lands in the crate that already owns the sidecar lifecycle. The
process-global shape matches the existing `pause_embed` / `embed_paused` pair in
`crates/travsr-plugin-host/src/embed_catalog.rs:601-614`, which the daemon reads
the same way from `handle_control_message`, so no plumbing is added to a
function with eleven call sites.

## Wire/protocol compatibility analysis

No wire change. `EMBED_PROTOCOL_VERSION` stays `1`; `KnnRequest`, `KnnResponse`,
`EmbedHandshakeResponse` and the `EmbedPlugin` trait are untouched, so every
installed `travsr-embed` binary keeps working with no version gate. Nothing is
serialized: the cache is in memory and does not outlive the daemon.

`travsr-plugin-host/src/` is covered by the `plugin-hashes.lock` gate, so the
lock is regenerated with `.github/scripts/update-plugin-hashes.sh`. No
`plugin_version` bump is warranted: the gate exists to catch changes a plugin
*binary* must be rebuilt for, and no plugin-facing contract moved. There is
precedent for a regenerate-only update (commit `975a4d1`).

`travsr-daemon`'s `query_cache` types are crate-internal;
`QUERY_PROTOCOL_VERSION` on the CLI/daemon IPC boundary is unaffected because
cache keys never cross it.

## Test plan

- `travsr-plugin-host` / `embed_supervisor.rs`:
  `the_serving_generation_advances_only_when_a_sidecar_starts_serving`. Both
  halves in one test because the counter is process-global and two tests would
  race. The negative half guards the inactive `try_start` path against a
  spurious bump that would miss the cache on every daemon start of a machine
  with no plugin installed.
- `travsr-daemon` / `query_cache.rs`:
  `a_sidecar_restart_invalidates_warm_ask_entries`, the #510 scenario:
  identical `graph`, identical `embed`, advanced generation must miss, and the
  unchanged generation must still hit. This is the test that fails before the
  change.
- `travsr-daemon` / `lib.rs`: `only_ask_is_keyed_on_embed_state`, pinning the
  predicate both embed components are gated on.

Red-first evidence: with `embed_generation` carried in `DataVersions` but not
yet in `CacheKey`, `a_sidecar_restart_invalidates_warm_ask_entries` fails on
"a respawned sidecar serves a different HNSW index, so a warm entry built from
the old one must not hit", which is the stale hit #510 describes rather than a
compile error.

## Risks

- The counter is process-global. A single process hosting sidecars for several
  repos would invalidate all their caches on any one respawn. Conservative, and
  the daemon is per-repo today.
- Wrap-around at `u64` is not reachable.
- The counter does not distinguish "same index reloaded" from "different index
  loaded", so a plain crash-and-respawn invalidates warm entries that would
  still have been valid. One extra recompute per respawn, on a path that is
  already rare and already slow.

## Relation to #509 / PR #771

Complementary, not overlapping. #771 fixes *how* the `embed` component is
computed inside `travsr-store` (file identity mixed into the token so a
delete-and-recreate of `embed.db` is visible) and does not change the set of
components in the key. This change adds a component and does not change how
`embed` is derived. `SqliteStore::embed_data_version` is called unchanged.

The issue text says #509 "is subsumed if this direction is adopted". It is not:
that only followed from replacing `embed.db` in the key, which this design
rejects for the reason given under Root cause. #771 remains necessary and should
merge on its own.

Textual overlap is confined to `query_cache.rs`'s module doc and the `embed`
field doc, both of which #771 rewrites. This change appends after those blocks
rather than editing them, so the merge is additive.
