# Sprint 1 — Tree-sitter Indexer + SQLite Graph Schema

- **Dates:** 2026-05-18 (Mon) to 2026-05-29 (Fri)
- **Phase:** 1 (MVP)
- **Sprint goal:** Stand up the core data model, a Tree-sitter TypeScript pipeline that produces nodes/edges, a SQLite + WAL graph store with migrations, and a `travsr init` that writes an empty graph database.

---

## Stories

| ID | Crate | Story | Acceptance |
|---|---|---|---|
| S1-1 | `travsr-core` | Confirm and lock the `VName`, `Node`, `Edge` types per Kythe 5-field schema (`corpus`, `root`, `path`, `language`, `signature`). | Types compile, `Serialize`/`Deserialize` derived, `vname_id()` stable hash documented, unit tests for equality and round-trip. |
| S1-2 | `travsr-indexer` | Tree-sitter TypeScript wiring: load `tree-sitter-typescript`, parse a file, walk the AST, emit `Node` per file/class/function/variable and `Edge` for parent/child + import. | Given a fixture `.ts` file, indexer emits the expected node count and import edges; covered by snapshot tests. |
| S1-3 | `travsr-store` | SQLite + WAL schema: `nodes`, `edges`, `files`, `meta` tables; migration runner (versioned, idempotent); `put_node`, `put_edge`, `get_node`, `iter_edges_from` API on a `Store` trait. | Migrations run twice without error; `cargo test` covers put/get and edge iteration; WAL mode verified via `PRAGMA journal_mode`. |
| S1-4 | `travsr-cli` | `travsr init`: detects the current git repo root, creates `.travsr/`, runs store migrations, writes `.travsr/graph.db`, prints a one-line summary. Refuses to run outside a git repo. | `travsr init` exits 0 in a git repo, exits non-zero outside one; second run is idempotent. |

---

## Definition of Done

- [ ] `cargo test --workspace` is green on Linux and macOS
- [ ] `cargo clippy --workspace -- -D warnings` is clean
- [ ] `travsr init` creates `.travsr/graph.db` and is idempotent
- [ ] README has an Install section referencing `cargo install --path crates/travsr-cli` (npm wrapper lands Sprint 3)
- [ ] No `unsafe` blocks introduced (per `CLAUDE.md`)
- [ ] No `unwrap()` in library code; CLI may unwrap with a clear message
- [ ] Crate dependency rules from `CLAUDE.md` respected (verified by `cargo tree`)

---

## Risks

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Tree-sitter TypeScript grammar version drift (TS vs TSX) breaks parsing | Medium | Medium | Pin grammar crate version; ship both `typescript` and `tsx` language objects; fixture covers both. |
| SQLite WAL behavior differs across macOS and Linux file systems | Low | Medium | CI matrix runs both; explicit `PRAGMA` assertions; document `.travsr/` must be on a local FS. |
| `VName` signature design churn blocks downstream crates | Medium | High | Lock the struct on day 2 via a short ADR; only the `signature` derivation may change after that. |

---

## Demo Plan (Fri 2026-05-29)

1. In a freshly cloned fixture repo: `travsr init` shows the one-line summary and a `.travsr/graph.db` appears.
2. Open the DB with `sqlite3` and show the `nodes` and `edges` tables populated by a manual indexer run against `fixtures/ts-small/`.
3. Run `cargo test --workspace` live; show green.
4. Show `cargo tree -p travsr-store` to prove no forbidden cross-crate edges.

---

## Out of Scope (Deferred)

- Git hook (Sprint 2)
- BFS retrieval (Sprint 2)
- MCP server (Sprint 3)
- npm wrapper (Sprint 3)
- LSIF, Kuzu, PPR (Phase 2)
