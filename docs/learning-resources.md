# Travsr — learning resources

Everything needed to contribute deeply, ordered by what actually unblocks work on
this codebase. Not exhaustive — curated.

> **On the YouTube links:** channel URLs and search terms rather than specific
> video URLs, because individual video links go stale and I would rather send you
> to a channel that definitely exists than a video ID that might not.

---

## Priority — if you only do three things

```
1.  Rustlings                 learn by hand, not by reading
2.  scip.proto                one sitting; your current scope (#549/#552) lives here
3.  RRF paper                 4 pages; seed.rs:535 is this exact algorithm
```

Everything else when a specific task demands it.

---

## Rust

The gap between "can review a PR" and "can write one" in this codebase.

| Resource | Link |
|---|---|
| The Rust Book | https://doc.rust-lang.org/book/ |
| Rust by Example | https://doc.rust-lang.org/rust-by-example/ |
| Rustlings (exercises) | https://github.com/rust-lang/rustlings |
| Too Many Linked Lists | https://rust-unofficial.github.io/too-many-lists/ |
| Async book | https://rust-lang.github.io/async-book/ |
| Clippy lints | https://rust-lang.github.io/rust-clippy/master/ |
| API guidelines | https://rust-lang.github.io/api-guidelines/ |

**Don't read the whole Book.** Chapters 4 (ownership), 10 (traits/generics),
13 (closures/iterators), 15 (smart pointers), 17 (trait objects). Ownership is
the only genuinely unfamiliar idea; the rest resembles other languages.

### YouTube — fastest first

| Channel | What to watch | Length |
|---|---|---|
| [Fireship](https://www.youtube.com/@Fireship) | search *"Rust in 100 Seconds"* | 2 min |
| [No Boilerplate](https://www.youtube.com/@NoBoilerplate) | *"Rust for the impatient"* | ~7 min |
| [Let's Get Rusty](https://www.youtube.com/@letsgetrusty) | "Rust Book" playlist — **best ratio** | ~15 min/ch |
| [freeCodeCamp](https://www.youtube.com/@freecodecamp) | search *"Rust Programming Course"* | ~5 h |
| [Jon Gjengset](https://www.youtube.com/@jonhoo) | "Crust of Rust" playlist | 2–4 h each |

Jon Gjengset is the best intermediate Rust content anywhere, but it is not a
starting point. Save it until Rustlings stops hurting.

**Fastest path:** Fireship (2 min) → Let's Get Rusty ch 4 and 10 (30 min) →
Rustlings (hands on).

### Where it's used here

```rust
&dyn EdgeFilter            // trait objects        — pcst.rs, rbac.rs
impl Fn(&str) -> Vec<_>    // closures as params   — store.rs embed hooks
Option<Vec<T>>             // ownership + Option    — everywhere
Arc<Mutex<SqliteStore>>    // shared state          — daemon
#[cfg(test)]               // conditional compile   — ~40% of the tree
```

---

## SCIP / LSIF

Highest return on your time right now — #549 and #552 are yours and they live
here.

| Resource | Link |
|---|---|
| **SCIP proto schema** | https://github.com/sourcegraph/scip/blob/main/scip.proto |
| SCIP announcement | https://sourcegraph.com/blog/announcing-scip |
| SCIP repo | https://github.com/sourcegraph/scip |
| LSIF spec | https://microsoft.github.io/language-server-protocol/specifications/lsif/0.6.0/specification/ |
| LSP spec | https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/ |

**Read `scip.proto` itself, not a summary.** 300 lines, well commented, and it is
the ground truth. `SymbolRole`, `Occurrence` and `SymbolInformation` are the
three messages that matter for #549.

**YouTube:** nothing good exists for SCIP. For background, search *"Language
Server Protocol explained"* — LSP is the concept SCIP descends from.

---

## Information Retrieval

The largest product problem — conceptual recall sits at hit@1 0.208 — is an IR
problem, not a Rust one.

| Resource | Link |
|---|---|
| IR Book (Manning, free) | https://nlp.stanford.edu/IR-book/ |
| **RRF paper (4 pages)** | https://plg.uwaterloo.ca/~gvcormac/cormacksigir09-rrf.pdf |
| BM25 | https://en.wikipedia.org/wiki/Okapi_BM25 |
| hit@k / MRR | https://en.wikipedia.org/wiki/Mean_reciprocal_rank |
| Ranked-retrieval evaluation | https://nlp.stanford.edu/IR-book/html/htmledition/evaluation-of-ranked-retrieval-results-1.html |

The IR Book is a reference, not a read-through. Chapters 6 (scoring), 8
(evaluation), 11 (probabilistic / BM25).

### YouTube — thin category, be honest about it

| Channel | What |
|---|---|
| search *"BM25 explained"* | several good ~10 min explainers |
| search *"Stanford CS276"* | Manning's own course; dated but sound |
| [James Briggs](https://www.youtube.com/@jamesbriggs) | semantic search, embeddings, reranking — most practical |

The RRF paper is four pages and faster than any video on it.

### Where it's used here

```
bm25.rs                 Okapi BM25, k1=1.2 b=0.75
seed.rs:475             idf_weight
seed.rs:535             rrf_fuse
seed.rs:864             classify_confidence
bench/report-*.md       hit@k, MRR, abstention accuracy
```

---

## Graph algorithms

| Resource | Link |
|---|---|
| PageRank | https://en.wikipedia.org/wiki/PageRank |
| Personalized PageRank | https://en.wikipedia.org/wiki/Topic-sensitive_PageRank |
| k-core decomposition | https://en.wikipedia.org/wiki/Degeneracy_(graph_theory) |
| Prize-Collecting Steiner Tree | https://en.wikipedia.org/wiki/Steiner_tree_problem |
| petgraph docs | https://docs.rs/petgraph/ |

### YouTube — the richest category here

| Channel | What |
|---|---|
| [WilliamFiset](https://www.youtube.com/@WilliamFiset-videos) | "Graph Theory" playlist — **best graph content on YouTube** |
| [Abdul Bari](https://www.youtube.com/@abdul_bari) | Dijkstra, DP, knapsack — clear and exam-style |
| [Reducible](https://www.youtube.com/@Reducible) | visual and beautiful; good on PageRank |
| [3Blue1Brown](https://www.youtube.com/@3blue1brown) | the linear-algebra intuition behind PageRank |

### Where it's used here

```
ppr.rs:136              ppr_weighted      α=0.85, ε=1e-6, ≤50 iterations
kcore.rs:44             compute_kcore     Batagelj & Zaversnik 2003
knapsack.rs:53          knapsack          2-D DP + greedy fallback
pcst.rs:48              pcst_path         Dijkstra + λ-corridor (NOT Steiner — see #527)
```

The in-code comments are genuinely good. `ppr.rs:1-30` contains most of the
theory you need.

---

## Parsing — Tree-sitter

| Resource | Link |
|---|---|
| Tree-sitter docs | https://tree-sitter.github.io/tree-sitter/ |
| Queries | https://tree-sitter.github.io/tree-sitter/using-parsers#pattern-matching-with-queries |
| **Playground** | https://tree-sitter.github.io/tree-sitter/playground |

Only the "Pattern Matching with Queries" section matters; the rest is for grammar
authors. The **playground is the fastest way in** — paste code, watch the AST.

**YouTube:** search *"tree-sitter queries neovim"* — the Neovim community
produced the most practical query explanations.

---

## MCP

| Resource | Link |
|---|---|
| Spec | https://modelcontextprotocol.io/ |
| JSON-RPC 2.0 | https://www.jsonrpc.org/specification |
| Example servers | https://github.com/modelcontextprotocol/servers |

Small and new — two hours covers it. **YouTube:** search *"Model Context
Protocol explained"*, or [Anthropic](https://www.youtube.com/@anthropic-ai).

---

## SQLite

The entire store rests on this.

| Resource | Link |
|---|---|
| FTS5 | https://www.sqlite.org/fts5.html |
| WAL mode | https://www.sqlite.org/wal.html |
| **Query planner** | https://www.sqlite.org/optoverview.html |
| rusqlite | https://docs.rs/rusqlite/ |

`optoverview.html` is worth the time — it explains why the OR-join was avoided in
#551.

**YouTube:** search *"SQLite internals Richard Hipp"* (the creator's own talk,
excellent), or *"SQLite FTS5 tutorial"*.

---

## Embeddings / rerank

| Resource | Link |
|---|---|
| Sentence Transformers | https://www.sbert.net/ |
| Cross-encoder vs bi-encoder | https://www.sbert.net/examples/applications/cross-encoder/ |
| tract (ONNX in Rust) | https://github.com/sonos/tract |

**YouTube:** [James Briggs](https://www.youtube.com/@jamesbriggs) is the most
practical on reranking and semantic search. [3Blue1Brown](https://www.youtube.com/@3blue1brown)
*"Attention in transformers"* for the intuition behind embeddings.

---

## Sandboxing

| Resource | Link |
|---|---|
| bubblewrap | https://github.com/containers/bubblewrap |
| macOS App Sandbox | https://developer.apple.com/library/archive/documentation/Darwin/Conceptual/AppSandboxDesignGuide/ |
| Windows Job Objects | https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects |
| Linux namespaces | https://man7.org/linux/man-pages/man7/namespaces.7.html |

**YouTube:** search *"Linux namespaces and cgroups explained"* or *"how
containers work under the hood"* — the best route to understanding bwrap.

---

## A weekend plan

```
Sat AM   Fireship Rust (2m) → Let's Get Rusty ch 4, 10 (30m) → Rustlings (2h)
Sat PM   scip.proto (1h) → RRF paper (30m)
Sun AM   WilliamFiset: PageRank + BFS (1h)
Sun PM   MCP spec (1h) → SQLite optoverview (1h)
```

Then open any file in `travsr-retrieval` — it will read differently.

---

## The part that actually matters

None of this substitutes for writing code. Two hours stuck on a real compile
error teaches more than twenty hours of reading.

Pick a small unassigned issue, write it yourself, and ask for help when stuck —
rather than asking for the patch. The difference in what sticks is large.

Good starting points:

```
#456   Cargo workspace version still 0.7.0     one line
#453   search_symbol substring noise           small, retrieval
#454   repos_list exists flag                  small, CLI
```

`travsr-retrieval` is the safest crate to experiment in — 3,439 lines, six
files, self-contained, well tested. Two weeks of one file at a time and the
whole crate makes sense.
