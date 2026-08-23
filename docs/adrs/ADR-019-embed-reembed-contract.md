# ADR-019: Embed Re-embed Contract (`--reembed` + version floor)

**Date:** 2026-08-23
**Status:** Accepted
**Phase:** N/A (cross-repo contract change)
**Author:** Solution Architect
**Related:** RFC-025 (sidecar version floor / honesty tests), travsr-embed #6 (engine provenance), EMBED_UX_AUDIT.md findings F3 + F8

---

## Context

A `travsr embed` index stores one vector per node in `embed.db`, tagged with the
engine that produced it (`meta.embed_backend`). GPU/ORT fp32 and tract fp32
matmul are not bit-identical, so when the resolved engine changes between runs
(for example travsr-embed v1.5.0 reversed the default macOS engine to tract),
an incremental reindex leaves `embed.db` with vectors from two engines. The
sidecar detected this and printed:

> WARNING: embedding backend changed (...). Existing vectors were produced by a
> different engine; run `travsr embed reindex --rebuild` for consistent
> embeddings.

Two defects made that remedy dishonest (EMBED_UX_AUDIT.md F3):

1. **`travsr embed reindex --rebuild` did not exist** — the flag errored with
   "unexpected argument".
2. **No re-embed-all path existed anywhere.** `reindex` skips nodes that already
   have a vector (`NOT EXISTS`); `switch` only rewrites config; `calibrate` is
   explicitly "without re-embedding"; `gc` reclaims. A user could not get a
   consistent index without manually deleting `embed.db`.

The warning also fired **mid-reindex** (F8), so it read as "reindex while
reindexing".

## Decision

**Add a first-class full-rebuild path, spanning the sidecar and the CLI, gated by
the RFC-025 version floor. The three pieces move together and the sidecar
release ships first.**

### 1. Sidecar: `--reindex <db> --reembed` (travsr-embed v1.6.0)

`--reembed` clears every stored vector for the active model **and** the recorded
engine, then the ordinary reindex re-embeds every node with the current engine
and rebuilds the HNSW index. Because the clear happens once, up front, the
existing chunked-streaming reindex loop (which drains via `NOT EXISTS`) is
reused unchanged and terminates normally.

**Why clear-then-reindex rather than "bypass `NOT EXISTS`".** The reindex loop
depends on committed chunks dropping out of the pending set to make progress;
removing the filter would loop forever. Deleting the model's rows up front makes
every node pending again with no other change to the loop.

**Crash-safety.** The old vectors are gone before any new one is written, so at
every moment the rows present are all from the current engine (never a torn or
mixed file). A killed run simply leaves fewer rows; a later ordinary `--reindex`
fills the rest with the same engine. Forgetting the recorded engine up front
means a mid-run crash leaves provenance honest (single engine), and a completed
run records the current engine with no mixed-engine warning.

### 2. Sidecar: honest, end-of-run notice (F8)

The mixed-engine notice is emitted **once at the end of a run** instead of
mid-progress, and points at the command that now exists. It is plain-language:
no engine names, no internal terms.

### 3. CLI: `travsr embed reindex --rebuild` + version floor (RFC-025)

`--rebuild` invokes the sidecar in `--reembed` mode through the existing
banner/progress/cancel plumbing. Because `--reembed` is a **required** sidecar
behavior the moment the CLI depends on it, the same change bumps
`EMBED_MIN_VERSION` to **v1.6.0** and every embed `version_fallback` to v1.6.0
(RFC-025 decision 3). A pre-v1.6.0 sidecar then hits the actionable floor
refusal at the reindex entry point instead of failing on an unknown argument.

## Release ordering (load-bearing)

The RFC-025 honesty test `declared_floor <= latest_released` (network) refuses a
floor above what users can install. Therefore:

1. Publish **travsr-embed v1.6.0** (carrying `--reembed`) first.
2. Once published, `latest_released >= min_version`, so honesty test (b) passes.
3. Merge the main CLI change (`--rebuild` + the v1.6.0 floor bump) after.

Until step 1 lands, honesty test (b) is red **by design** on a networked run
(offline it skips); the non-network honesty test (a),
`version_fallback >= min_version`, stays green because both are v1.6.0.

## Consequences

- The engine-change remedy is now reachable and consistent end-to-end.
- The daemon's automatic reindex paths are untouched: `--reembed` is CLI-only
  (every daemon spawn passes `reembed = false`).
- A future sidecar-behavior dependency repeats this pattern: add the behavior,
  release the sidecar, floor to it in the same commit that first relies on it.
