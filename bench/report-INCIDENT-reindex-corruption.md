# INCIDENT — `travsr embed reindex` corrupted the bge-small HNSW mapping

_Discovered 2026-07-01 during tool benchmarking._

## Symptom
After running `travsr embed reindex` (which embedded 363 new nodes and rebuilt the
HNSW from 4045 embeddings), **every** get_context query — including exact symbol
names — returns unrelated nodes at score ~1.0:

```
query: "get_context_body"
→ fn:extract_python (skeleton.rs:740)        [score 1.00]
→ fn:collect_python_docstring (skeleton.rs)  [score 0.98]
```

Confidence collapses to `weak` across all literal queries that were `strong`/`exact`
before the reindex. Stable across fresh `travsr mcp` processes → the corruption is in
the **on-disk** `bge-small-en-v1.5.hnsw.usearch`, i.e. the rebuilt key→node-id
mapping is wrong.

## Likely cause
Two `travsr daemon start --foreground` processes were running (PIDs 32976, 57835)
plus two `travsr mcp` when the CLI `embed reindex` rebuilt the HNSW. Concurrent
writers to the shared HNSW / embed.db (embed.lock present but 0 bytes) is the prime
suspect. Open question: does an *isolated* reindex (no daemon) also corrupt? If yes,
it's a rebuild bug; if no, it's a concurrency/locking bug.

## Impact
- The live index the VS Code extension queries is currently degraded.
- Benchmark Run 2 (post-reindex) is invalid; Run 1 (pre-reindex) is the valid baseline.

## Repro / next steps
1. Stop ALL travsr daemon + mcp processes (single-owner the index).
2. `travsr embed reindex` in isolation → test exact query. Determines bug vs race.
3. If isolated reindex is clean → add file-lock enforcement so a second daemon/CLI
   cannot rebuild the HNSW concurrently. If still corrupt → fix the rebuild's
   key→node-id assignment (usearch key order vs stored id-map).
