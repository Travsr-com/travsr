# Sprint 2 — Git Hook + SHA256 Delta + BFS Retrieval

- **Dates:** 2026-06-01 (Mon) to 2026-06-12 (Fri)
- **Phase:** 1 (MVP)
- **Sprint goal:** Make the graph always-fresh on git commit (per `CLAUDE.md` principle 2) and ship BFS depth-3 retrieval with a token-budget cutoff so we can answer the first useful question: "who calls this symbol?".

---

## Stories

| ID | Crate | Story | Acceptance |
|---|---|---|---|
| S2-1 | `travsr-daemon` | Install a `post-commit` git hook on `travsr init` (idempotent, preserves existing hooks via chain script). Add a file-watcher fallback (`notify` crate) for non-git edits. Hook invokes the indexer with the changed file list from `git diff --name-only HEAD~1 HEAD`. | Hook file present after `init`, runs on commit, no-ops cleanly if `.travsr` is missing. Watcher debounces 250ms. |
| S2-2 | `travsr-indexer` | SHA256 per-file hashing stored in `files` table. On change: delete nodes/edges scoped to the file's VName path, re-parse, re-insert (MVP delta path per `CLAUDE.md` and proposal section 4 Layer 3). | Editing a file flips its hash, the old node ids are purged, new ids appear; covered by an integration test on a 10-file fixture. |
| S2-3 | `travsr-retrieval` | `bfs(seed: VName, depth: u8, token_budget: usize)` against `SqliteStore`. Walks call/import edges, stops on depth or budget, returns ordered `Vec<Node>`. Token cost approximated from node source span length. | Returns correct callers on `fixtures/ts-callers/`; depth cap and budget cap each independently enforced by tests. |
| S2-4 | `travsr-cli` | `travsr status`: prints graph stats (node count, edge count, last commit indexed, freshness). `travsr ask "who calls <Symbol>"` (toy): resolves symbol via simple name match, calls BFS, prints results as a table. | `status` runs in under 50ms on the fixture; `ask` returns the expected callers for the fixture symbol. |

---

## Definition of Done

- [ ] Editing any file in a 10-file fixture and running `git commit` updates the graph in under 1s (measured by hook wall time)
- [ ] BFS returns the documented correct callers on `fixtures/ts-callers/`
- [ ] `travsr status` shows freshness derived from the last indexed commit SHA
- [ ] `cargo test --workspace` and `cargo clippy -- -D warnings` are green
- [ ] Hook script is shell-portable (bash, dash); macOS and Linux tested in CI
- [ ] Existing user hooks are preserved (chain pattern)

---

## Risks

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Hook chaining clobbers a pre-existing `.git/hooks/post-commit` | Medium | High | Detect existing hook; rename to `.travsr.bak`; emit a clear message; cover with a test that pre-seeds a hook. |
| Delete-and-reinsert causes a visible read gap during commit | Medium | Medium | Wrap the swap in a single SQLite transaction; document the brief lock in the proposal's MVP simplification. |
| BFS without ranking returns noisy results on dense graphs | High | Low (MVP) | Acceptable for MVP; PPR lands in Phase 2. Document the limitation in `travsr ask --help`. |

---

## Demo Plan (Fri 2026-06-12)

1. Start from the Sprint 1 fixture, `travsr init`. Show `.git/hooks/post-commit` was installed.
2. Edit `src/payments/charge.ts`, `git commit -m 'demo'`. Show hook output (under 1s).
3. `travsr status` shows the new commit SHA and updated node count.
4. `travsr ask "who calls charge"` returns the expected callers in a table.
5. Show the integration test that asserts the same thing in CI.

---

## Out of Scope (Deferred)

- MCP server and JSON-RPC handling (Sprint 3)
- npm wrapper and Claude Desktop integration (Sprint 3)
- Stacked-database visibility masks (Phase 3)
- PPR ranking (Phase 2)
