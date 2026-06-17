# RFC-017: `travsr-analysis` — Unified Code Analysis Crate

**Status:** Draft  
**Author:** Tech Lead / Abhishek  
**Date:** 2026-06-17  
**Crates affected:** `travsr-indexer`, `travsr-plugin-host`, `travsr-mcp` (new: `travsr-analysis`)  
**Depends on:** RFC-003 (multi-lang indexer), RFC-011 (plugin architecture), RFC-016 (diagnostic system)  
**Issue:** #355

---

## Summary

All Tree-sitter grammar bindings, AST walkers, language parsers, and code-level
analysis utilities currently live in two separate crates — `travsr-indexer` and
`travsr-plugin-host` — with snippet extraction scattered into `travsr-mcp`. This
RFC introduces a new `travsr-analysis` crate that consolidates all of this into a
single canonical home, makes `travsr-indexer` a thin pipeline, and unblocks a set of
read-path code intelligence features (AST skeletons, sub-node extraction, code
summarisation hooks) that would otherwise require duplicating Tree-sitter machinery
into `travsr-mcp`.

---

## Motivation

### The duplication problem today

Tree-sitter grammar bindings and AST walking code are split across three locations:

| Location | Tree-sitter usage | Languages |
|---|---|---|
| `travsr-indexer/src/{rust,typescript,python,go}.rs` | Full parse + node extraction | Rust, TS, Python, Go |
| `travsr-indexer/src/phase_b_{rust,typescript,python}.rs` | Phase B call-site edges via Tree-sitter | Rust, TS, Python |
| `travsr-plugin-host/src/plugins/{generic,java}.rs` + `registry.rs` | Generic plugin + Java | Java + 10 plugin languages |

Every one of these files duplicates the same pattern: load `tree_sitter::Language`,
build a `Parser`, compile a `Query`, iterate captures. If `travsr-mcp` needs AST
access at query time (for skeleton generation, sub-node extraction, or intent
signals), it must either:

- **A)** Duplicate the Tree-sitter machinery again — third copy, drifts independently
- **B)** Pull in `travsr-indexer` as a dependency — breaks the dependency boundary  
  (`travsr-mcp → travsr-indexer → travsr-store` creates a cycle via retrieval)
- **C)** Create `travsr-analysis` — one canonical home, all callers depend on it

Option C is the only sound choice.

### The read-path gap

`travsr-mcp`'s `snippet_for_node` is pure file I/O (read file, slice lines, strip
docblocks). For the design decisions emerging from the `include_snippets`
implementation — AST skeleton generation, full-body-or-nothing budget decisions, and
eventual intent signals from structural code patterns — `travsr-mcp` needs access to
a parsed AST at query time. That capability can only come from Tree-sitter, and
Tree-sitter must not be duplicated again.

### The `travsr-indexer` mass problem

After Phase B additions (`phase_b_rust.rs`, `phase_b_typescript.rs`,
`phase_b_python.rs`), `travsr-indexer`'s four core language parsers total
**~3760 LOC** of Tree-sitter walking code. The `Indexer` struct itself — which is
the actual pipeline — is ~200 LOC. The crate is 95% parser implementation
and 5% pipeline. This inversion makes it hard to reason about what the indexer
actually does versus what the language parsers do.

---

## Current state (verified 2026-06-17)

### Files containing Tree-sitter code

**`travsr-indexer`** (all Tree-sitter deps: rust, ts, python, go)
```
src/rust.rs             924 LOC   — Rust Phase A parser
src/typescript.rs       399 LOC   — TypeScript/TSX Phase A parser
src/python.rs           681 LOC   — Python Phase A parser
src/go.rs              1002 LOC   — Go Phase A parser
src/phase_b_rust.rs       ?       — Rust Phase B call-site edges
src/phase_b_typescript.rs ?       — TS Phase B call-site edges
src/phase_b_python.rs     ?       — Python Phase B call-site edges
src/emit.rs               ?       — Edge construction helpers (used by parsers above)
src/lib.rs                        — ParseOutput struct, Indexer struct, language dispatch
```

**`travsr-plugin-host`** (all Tree-sitter deps: java, kotlin-ng, ruby, c-sharp, php, scala, cpp, c, swift, dart, objc)
```
src/registry.rs           —  creates Language objects for 11 plugin languages
src/plugins/generic.rs    —  generic Tree-sitter plugin (handles 10 plugin languages)
src/plugins/java.rs       —  Java-specific Tree-sitter plugin
```

**`travsr-mcp`** — no Tree-sitter; snippet extraction is pure file I/O
```
src/tools.rs: snippet_for_node, snippet_line_cap, skip_leading_comments, is_comment_line
```

### Current dependency graph (relevant portion)

```
travsr-core
  ↑
travsr-indexer  ←── owns all Tree-sitter for Rust/TS/Python/Go
  ↑
travsr-plugin-host ←── owns all Tree-sitter for plugin languages
  ↑
travsr-daemon
  ↑
travsr-cli

travsr-mcp ←── no Tree-sitter access; blind at query time
```

---

## Proposed: `travsr-analysis` crate

### Mandate

`travsr-analysis` is the **canonical home for everything derived from parsing source
code**. It owns:

1. All 16 Tree-sitter grammar bindings (previously split between indexer and plugin-host)
2. Language detection by file extension
3. Phase A structural AST walkers (node + edge extraction per language)
4. Phase B call-site edge walkers
5. `ParseOutput` struct (the parse pipeline's output type)
6. `emit.rs` edge construction helpers
7. Snippet extraction helpers (`snippet_for_node`, `skip_leading_comments`, `snippet_line_cap`)
8. **AST skeleton generation** (new — for structured code summaries without truncation)
9. Sub-node extraction (future — extract specific branches, arms, methods from a body)
10. Code intent signal extraction (future — structural signals for retrieval routing)
11. Code summarisation hooks (future — where ONNX or sampling-LLM plugs in)

The principle: **if it requires a parsed AST, it belongs here**. Pure file I/O or
graph algorithms do not.

### Dependency rule

```
travsr-analysis  →  depends on travsr-core ONLY
```

No circular dependencies. This is the same constraint `travsr-core` has. Both are
leaves in the dependency tree.

### Updated dependency graph

```
travsr-error         → zero deps
travsr-core          → zero deps
travsr-analysis      → travsr-core only          ← NEW
travsr-ipc           → zero deps
travsr-plugin-protocol → zero deps
travsr-plugin-sdk    → travsr-plugin-protocol
travsr-indexer       → travsr-analysis + travsr-core      (loses Tree-sitter, gains analysis)
travsr-store         → travsr-core only
travsr-retrieval     → travsr-core + travsr-store
travsr-plugin-host   → travsr-analysis + travsr-core + travsr-plugin-protocol + travsr-error + travsr-indexer
travsr-mcp           → travsr-analysis + travsr-retrieval  (gains analysis for read-path AST)
travsr-daemon        → travsr-mcp + travsr-indexer
travsr-cli           → travsr-daemon
```

---

## Crate structure

```
crates/travsr-analysis/
├── Cargo.toml
└── src/
    ├── lib.rs           — pub re-exports, Language enum, ParseOutput, detect_language()
    ├── emit.rs          — edge construction helpers  (moved from travsr-indexer)
    ├── rust.rs          — Rust Phase A + Phase B parser  (moved from travsr-indexer)
    ├── typescript.rs    — TypeScript/TSX parser  (moved from travsr-indexer)
    ├── python.rs        — Python Phase A parser  (moved from travsr-indexer)
    ├── go.rs            — Go Phase A parser  (moved from travsr-indexer)
    ├── phase_b_rust.rs  — Rust Phase B call-site edges  (moved from travsr-indexer)
    ├── phase_b_typescript.rs  (moved from travsr-indexer)
    ├── phase_b_python.rs      (moved from travsr-indexer)
    ├── generic_plugin.rs — generic Tree-sitter plugin for plugin-host languages
    │                       (moved from travsr-plugin-host/plugins/generic.rs)
    ├── java.rs          — Java-specific parser  (moved from travsr-plugin-host)
    ├── snippet.rs       — snippet_for_node, skip_leading_comments, snippet_line_cap,
    │                       is_comment_line  (moved from travsr-mcp/tools.rs)
    ├── skeleton.rs      — AST skeleton generation  (NEW)
    └── language.rs      — Language enum + from_extension() + grammar registry
                           (merged from travsr-indexer::lib.rs + travsr-plugin-host::registry.rs)
```

### Key public API

```rust
// Language detection
pub enum Language { Rust, TypeScript, Python, Go, Java, Kotlin, Ruby, CSharp, Php,
                    Scala, Cpp, C, Swift, Dart, ObjC }
pub fn detect_language(path: &Path) -> Option<Language>

// Parse pipeline output (moved from travsr-indexer)
pub struct ParseOutput { pub nodes: Vec<Node>, pub edges: Vec<Edge>,
                         pub ffi_markers: Vec<FfiMarker> }
impl ParseOutput { pub fn merge_deduped(&mut self, other: ParseOutput) }

// Parse a file into graph records
pub fn parse_file(corpus: &str, abs_path: &Path, vname_path: &str)
    -> anyhow::Result<ParseOutput>

// Snippet extraction (moved from travsr-mcp)
pub fn snippet_for_node(node: &Node, repo_root: &Path) -> Option<String>
pub fn snippet_line_cap(kind: &str) -> usize
pub fn skip_leading_comments<'a>(lines: &'a [&'a str]) -> Vec<&'a str>

// AST skeleton (new — for structured summaries)
pub fn skeleton_for_node(node: &Node, repo_root: &Path) -> Option<AstSkeleton>

pub struct AstSkeleton {
    pub language: Language,
    pub signature: String,    // fn name + params + return type
    pub members: Vec<String>, // field names for struct/class, variant names for enum
    pub call_sites: Vec<String>, // direct callees visible in body (no resolution)
    pub token_estimate: usize,
}
```

---

## Migration plan

The migration is purely mechanical — no algorithmic changes. All existing tests move
with the code; the test suite stays byte-identical in terms of what it exercises.

### Phase 1 — Create the crate shell (non-breaking)

1. Add `crates/travsr-analysis/` to workspace `Cargo.toml` members.
2. Create `Cargo.toml` with all 16 Tree-sitter grammar deps (from both indexer and
   plugin-host), `travsr-core` dep, and standard workspace fields.
3. Create `src/lib.rs` that re-exports the `Language` enum (copied, not yet the
   canonical one) and `ParseOutput` (identical to indexer's).
4. **No existing code changes.** Both crates compile unchanged; the new crate is
   empty except stubs. CI gate: all tests pass, no new warnings.

### Phase 2 — Migrate language parsers from `travsr-indexer`

Move each file in order, updating `pub(crate)` → `pub` where needed:

```
travsr-indexer/src/emit.rs          → travsr-analysis/src/emit.rs
travsr-indexer/src/rust.rs          → travsr-analysis/src/rust.rs
travsr-indexer/src/typescript.rs    → travsr-analysis/src/typescript.rs
travsr-indexer/src/python.rs        → travsr-analysis/src/python.rs
travsr-indexer/src/go.rs            → travsr-analysis/src/go.rs
travsr-indexer/src/phase_b_rust.rs  → travsr-analysis/src/phase_b_rust.rs
travsr-indexer/src/phase_b_typescript.rs → travsr-analysis/src/phase_b_typescript.rs
travsr-indexer/src/phase_b_python.rs     → travsr-analysis/src/phase_b_python.rs
```

For each file moved:
- Replace `use crate::emit` with `use travsr_analysis::emit` at the call site in `travsr-indexer`.
- `travsr-indexer/Cargo.toml` gains `travsr-analysis = { path = "../travsr-analysis" }`.
- Tree-sitter grammar deps remain in indexer's `Cargo.toml` until Phase 3 (they become
  transitive via analysis; at Phase 3 we remove them from indexer).
- All existing tests in the moved file move with it — zero test deletion.
- CI gate per file: `cargo test -p travsr-indexer && cargo test -p travsr-analysis` green.

`travsr-indexer/src/lib.rs` after Phase 2:
- `ParseOutput` → `pub use travsr_analysis::ParseOutput` (re-export, not the owner)
- `Indexer` struct and `parse_file_with_vname` remain (they're the pipeline, not analysis)
- `Language::from_extension` re-exported from `travsr_analysis::Language`
- Tree-sitter grammar deps removed from `travsr-indexer/Cargo.toml` (transitive)

### Phase 3 — Migrate plugin-host grammar registry

Move the Tree-sitter language grammar registration out of `travsr-plugin-host`:

```
travsr-plugin-host/src/registry.rs (grammar init section)  → travsr-analysis/src/language.rs
travsr-plugin-host/src/plugins/generic.rs                  → travsr-analysis/src/generic_plugin.rs
travsr-plugin-host/src/plugins/java.rs                     → travsr-analysis/src/java.rs
```

`travsr-plugin-host/Cargo.toml`:
- Remove all 11 Tree-sitter grammar deps (now transitive via `travsr-analysis`).
- Add `travsr-analysis = { path = "../travsr-analysis" }`.
- Remove `travsr-indexer` dep if it was only pulled for `ParseOutput` (check during migration).

CI gate: `cargo test -p travsr-plugin-host && cargo test -p travsr-analysis` green.

### Phase 4 — Migrate snippet helpers from `travsr-mcp`

Move from `travsr-mcp/src/tools.rs` to `travsr-analysis/src/snippet.rs`:
- `snippet_for_node`
- `snippet_line_cap`
- `skip_leading_comments`
- `is_comment_line`
- `SNIPPET_SEP`
- `SNIPPET_DEFAULT_BUDGET`

`travsr-mcp/src/tools.rs`: replace with `use travsr_analysis::snippet::*`.

`travsr-mcp/Cargo.toml`: add `travsr-analysis = { path = "../travsr-analysis" }`.

All existing snippet tests move to `travsr-analysis/src/snippet.rs` as `#[cfg(test)]`.
`travsr-mcp` retains a thin integration test that exercises `snippet_for_node` via the
public API to confirm the re-export works.

CI gate: `cargo test -p travsr-mcp && cargo test -p travsr-analysis` green.

### Phase 5 — AST skeleton (new capability)

Implement `skeleton_for_node` in `travsr-analysis/src/skeleton.rs`. This is the only
phase that adds new functionality.

For each language, the skeleton walker:
1. Opens the source file (same SEC path guard as `snippet_for_node`).
2. Parses **only the declaration's byte range** via Tree-sitter (not the full file).
3. Walks the local AST to extract:
   - Signature (function params + return type, struct fields, enum variants)
   - Direct callees (identifier nodes in call position — unresolved, structural only)
   - Token estimate (character count / 4, clipped to 512 minimum)
4. Returns `AstSkeleton` or `None` on file-not-found / unsupported language.

**No line cap.** Skeleton generation never truncates — it summarises. This resolves
the "full body or nothing" constraint: for functions too large for a snippet, return
a skeleton instead. `travsr-mcp`'s `include_snippets` path will:
- Try `snippet_for_node` first (full body within budget).
- Fall back to `skeleton_for_node` for nodes that don't fit.
- Return header-only for unsupported languages.

CI gate: `cargo test -p travsr-analysis` green, including skeleton tests against real
fixture files for Rust, TypeScript, Python, Go.

---

## What `travsr-indexer` looks like after migration

```
crates/travsr-indexer/
├── Cargo.toml                ← deps: travsr-analysis, travsr-core, travsr-error,
│                                      anyhow, tracing, sha2, walkdir, ignore,
│                                      scip, protobuf, toml (no tree-sitter direct)
└── src/
    ├── lib.rs                ← Indexer struct, parse_file_with_vname pipeline,
    │                            re-exports ParseOutput + Language from travsr-analysis
    ├── hash.rs               ← SHA256 file hashing (stays)
    ├── lsif.rs               ← LSIF parsing (stays)
    ├── runner.rs             ← orchestration (stays)
    ├── sandbox.rs            ← SCIP sandbox (stays)
    ├── scip_unifier.rs       ← SCIP edge unification (stays)
    ├── ffi.rs                ← FFI marker types (stays)
    ├── ffi_resolver.rs       ← FFI resolution pass (stays)
    ├── ra_runner.rs          ← rust-analyzer runner (stays)
    └── phase_b_dart.rs       ← Dart Phase B (no Tree-sitter; stays)
```

LOC reduction: ~3760 LOC of parser code moves out; ~1000 LOC of pipeline code stays.
The indexer becomes a pipeline, not a parser library.

---

## What `travsr-mcp` gains

```
travsr-mcp/src/tools.rs after migration:
  snippet_for_node(n, root)     →  travsr_analysis::snippet_for_node(n, root)
  snippet_line_cap(kind)        →  travsr_analysis::snippet_line_cap(kind)
  skip_leading_comments(lines)  →  travsr_analysis::skip_leading_comments(lines)

  [NEW — enabled by travsr-analysis]
  skeleton_for_node(n, root)    →  travsr_analysis::skeleton_for_node(n, root)
```

This unblocks:
- **AST skeleton fallback** in `include_snippets` — large functions get a structured
  summary instead of a line-capped truncation
- **RFC-016 Tier 1 diagnostic signals** — structural lint queries over local AST at
  query time (no full re-parse; analysis crate caches nothing, each call is pure)
- **Future intent classification** — structural patterns as signals for retrieval
  routing (RFC-012 A2 L2-D hook site)

---

## Invariants

1. **`travsr-analysis` depends only on `travsr-core`** — enforced via `cargo deny`
   `[graph]` section; any new travsr-* dep is a build error.
2. **No Tree-sitter direct dep in `travsr-mcp`** — `travsr-analysis` is the only
   path to Tree-sitter from the read path. Enforced by the dependency rule above.
3. **No algorithmic changes during migration** — Phases 1–4 are pure file moves +
   visibility changes. Zero functional difference. Each phase's CI gate must show
   byte-identical test output (all existing tests pass, no new failures).
4. **Existing tests move with their code** — no test deletion. `#[cfg(test)]` blocks
   are part of the module they test and move atomically.
5. **`ParseOutput` has one owner** — `travsr-analysis`. `travsr-indexer` re-exports
   it; all existing callsites resolve unchanged.
6. **`unsafe` is banned in `travsr-analysis`** — same rule as all crates. Tree-sitter
   C bindings are behind the `tree-sitter` crate's safe Rust wrapper.

---

## What is explicitly out of scope

- **LSIF/SCIP parsing** — stays in `travsr-indexer`; it's a pipeline input format,
  not an AST analysis capability.
- **FFI resolution** — stays in `travsr-indexer`; requires cross-file analysis state
  that belongs to the indexer pipeline, not a stateless analysis utility.
- **rust-analyzer / pyright integration** — stays in `travsr-indexer`; these are
  external tool runners, not Tree-sitter analysis.
- **Graph storage** — `ParseOutput` contains raw nodes/edges; how they're stored
  remains `travsr-store`'s problem.
- **Embeddings / ONNX inference** — the RFC-012 A2 L2-B ONNX path belongs in
  `travsr-store` (behind the `embeddings` feature flag); `travsr-analysis` only
  provides the structural signal hooks that feed it.
- **Plugin protocol / transport** — stays in `travsr-plugin-host`; `travsr-analysis`
  only provides the parsing capability the plugins use, not the IPC mechanism.

---

## Risks and mitigations

| Risk | Likelihood | Mitigation |
|---|---|---|
| Phase 2 import churn breaks non-test code that re-exports via `crate::` | Low | Move one file at a time; CI gate after each file |
| `travsr-plugin-host` currently depends on `travsr-indexer` for `ParseOutput` — removing that dep breaks it | Medium | Keep `travsr-indexer` in plugin-host dep list through Phase 3; remove only after `ParseOutput` is stable in `travsr-analysis` and re-exported correctly |
| Two copies of `Language` enum during migration (indexer + analysis) cause type mismatch | Medium | In Phase 1, the analysis `Language` is additive; in Phase 2 the indexer re-exports from analysis and removes its own definition atomically (one PR, one CI gate) |
| Skeleton generation re-parses files on every query → latency regression | Low | Skeleton is only called on nodes that passed PPR + knapsack and had no fitting full snippet. Typically 1–5 nodes per query. Tree-sitter parse of a 40-line function body is sub-millisecond. No cache needed at MVP. |
| `travsr-analysis` Cargo.toml compiles 16 grammars → slower cold builds | Medium | Grammars were already compiled; they move from two crates to one. Total compilation time is unchanged; only the crate that triggers them changes. Warm builds are unaffected. |

---

## Escalation

- **Architecture sign-off required** before Phase 1 begins (Solution Architect → Principal Architect).
- **Tech Lead review** on Phase 5 skeleton API before any MCP contract changes land.
- Phase 1–4 are purely mechanical; each phase is its own PR. Phase 5 is a separate PR
  requiring a code review focused on the SEC path guard (same guard as `snippet_for_node`).

---

## Sequencing summary

```
Phase 1: Create crate shell                   [1 PR, ~30 min]
Phase 2: Migrate indexer parsers              [1 PR per language file, ~4 PRs]
Phase 3: Migrate plugin-host grammar registry [1 PR]
Phase 4: Migrate snippet helpers from MCP     [1 PR]
Phase 5: Implement skeleton_for_node          [1 PR, requires Phase 4 complete]
```

Total estimated LOC moved: ~4500 (net new: ~300 for `language.rs` + `skeleton.rs`).
No new external dependencies. No schema changes. No migration required.
