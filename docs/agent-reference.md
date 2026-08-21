# Agent reference (read on demand)

Detail that used to live in `CLAUDE.md` but is not needed on every task.
`CLAUDE.md` is loaded into context on every turn; this file is not — read it
when the task actually touches one of these areas.

---

## MCP tools (shipped in v0.11.0)

```
Query:
  get_dependencies(file)                → imports / dependencies
  get_callers(symbol)                   → incoming edges, with path:line
  find_references(symbol)               → every use site, all languages
  find_pattern(pattern)                 → structural pattern search
  get_blast_radius(file)                → what breaks if this changes
  get_execution_path(source, sink)      → lowest-cost path + λ-corridor context
  get_context(query, token_budget)      → seed → PPR → k-core → rerank → knapsack
  get_snippets(symbol)                  → source with file + line context
  search_symbol(name)                   → symbol definitions
  get_repo_map()                        → structural overview
  get_graph_stats()                     → counts, schema version, last commit
  get_graph_json(query, direction, depth) → subgraph for renderers
  get_lang_status()                     → Phase B status per language
  repo_languages()                      → detected languages

Repo registry (global mode):
  repos_list · repos_remove · repos_prune

Query expansion:
  synonym_add · synonym_set · synonym_remove · synonym_remove_term
  synonym_list · synonym_reset

Diagnostics (hidden — not in tools/list, call by name):
  seed_trace(query)                     → full seed pipeline trace
  embed_knn_probe(query, k)             → raw KNN ranking, no fusion
```

Every response is sanitised and wrapped in a `<travsr-data>` envelope before it
reaches a model. Arguments are validated (path traversal, length) before any
store access.

---

## The algorithmic stack

```
Graph Construction (Zero-LLM):
  Tree-sitter     → instant structural AST (all languages)
  LSIF compiler   → deep semantic edges (CFG, DFG, call graphs)
  Kythe VNames    → globally unique node identity across all repos

Storage:
  SQLite + WAL    → the storage backend (MVP and production)
  RocksDB         → possible future hyperscale backend, not built
  (Kùzu was dropped in #457 — see docs/adrs/ADR-018-drop-kuzu-backend.md)

Retrieval (No Vector Search):
  BFS depth 3     → get_dependencies / get_callers
  Personalized PageRank (PPR)  → get_context, deep traversal
  Dijkstra + λ-corridor        → get_execution_path
                                 NOT Goemans-Williamson PCST — see issue #527.
                                 Real PCST is scoped for S16 and gated on a
                                 benchmark showing it actually beats this.
  k-core decomposition → buried middle recovery
  BM25 + FTS5 trigram  → lexical seed selection
  RRF fusion           → merges exact / lexical / semantic candidates
  0-1 Knapsack    → token budget optimization
  Cross-encoder (ONNX) → reranks candidates only; never determines edges

Interface:
  MCP stdio       → local daemon ↔ IDE/agent
  MCP SSE         → cloud server ↔ remote clients
```

---

## Full crate dependency graph

Actual graph as of v0.11.0. `core`, `error`, `config`, `ipc` and `rerank` have
zero travsr-* dependencies and are omitted from the right-hand side.

```
travsr-analysis         → core                        ← INVARIANT: core ONLY.
travsr-store            → core, error
travsr-plugin-protocol  → core, error
travsr-plugin-sdk       → core, plugin-protocol
travsr-indexer          → core, error, analysis
travsr-retrieval        → core, error, store
travsr-plugin-host      → core, error, config, analysis, indexer, plugin-protocol
                          ← INVARIANT: the ONLY crate in the indexer tier
                            allowed to depend on plugin-protocol.
travsr-mcp              → core, error, analysis, retrieval, rerank, store, plugin-host
travsr-daemon           → core, ipc, mcp, indexer, retrieval, store, plugin-host, analysis
travsr-cli              → core, config, ipc, indexer, daemon, mcp, store,
                          retrieval, plugin-sdk, plugin-host
```

Only the two INVARIANTs are load-bearing — they keep parsing pure and keep the
trust boundary in exactly one crate. Both are restated in those crates' own
module docs. The rest is descriptive, not a permission list.

---

## Retrieval quality: the open problem

Where the work actually is now — not a roadmap item:

```
literal queries      6/6 hit          solved
conceptual queries   7/8 MISS         the open problem
salad queries        4/8 leak         precision gap
abstention           3/3 on nonsense  working, but tuned too conservatively
                                      (it also refuses conceptual queries it should answer)
```

Measured on kubernetes/kubernetes: hit@1 0.208, MRR 0.25. On travsr itself:
hit@1 0.292. Numbers live in `bench/report-*.md`.

Original MVP success criteria (all met long ago, kept as a record of what
"done" originally meant):

- `npm install -g travsr && travsr init` works on macOS + Linux
- `travsr ask "what calls PaymentService?"` returns correct answer
- Claude Desktop can use travsr as an MCP source

---

## Infrastructure (OCI Always Free)

```
Region: ap-mumbai-1 (or your chosen home region)

Instance 1: travsr-mcp-server
  Shape: VM.Standard.A1.Flex
  OCPU: 2 | RAM: 12 GB | Block: 50 GB
  Runs: travsr-mcp Docker container + Nginx SSL

Instance 2: travsr-indexer
  Shape: VM.Standard.A1.Flex
  OCPU: 2 | RAM: 12 GB | Block: 100 GB
  Runs: travsr-indexer + GitLab webhook receiver

Object Storage: travsr-releases (20 GB)
OCIR: ap-mumbai-1.ocir.io/travsr/ (500 MB)
DNS: travsr.com → Instance 1 public IP
SSL: Let's Encrypt via Certbot
```

---

## Dogfooding — full tool-selection table

Pick the tool that matches the question:

| Question | Use |
| --- | --- |
| Who calls this? What breaks? | `travsr graph <symbol> --direction both` |
| Where is this defined? What is relevant to X? | `travsr ask "<query>"` |
| Every use site with `path:line` | `travsr references <symbol>` |
| Textual search scoped to the graph | `travsr pattern "<regex>"` |
| Is the index healthy / stale / ghosted? | `travsr status`, `travsr fsck` |
| Which repos are registered? | `travsr repos` |
| Phase B language tool state | `travsr lang status` |
| Semantic search behaving oddly? | `travsr embed status`, `travsr embed calibrate`, `travsr rerank status` |
| Retrieval missing a term pair | `travsr synonym list` / `add` |
| Where does a setting come from? | `travsr config get <key>` |

### When a Travsr tool gives a bad result

Every invocation is an observation. Watch for: missing callers, ghost nodes,
stale paths, wrong `path:line`; empty/abstained results where the answer
demonstrably exists; the right node buried under a test file; latency that makes
you want to reach for grep; help text or abstention messages that describe the
wrong thing.

When you hit one:

1. Capture the exact command, the query, and actual vs expected output.
2. Say so in the response — do not silently fall back to grep and move on.
3. If it is in scope for the current task, fix it. If not, propose an issue with
   the repro; do not fix it inline.

Do not disable/skip/work around a Travsr tool to finish faster. Do not report
"travsr found nothing" without showing the command. Do not fabricate performance
or accuracy numbers — measure with the actual command.

---

## Escalation path

```
Code question          → Tech Lead
Architecture question  → Solution Architect → Principal Architect
Strategic question     → CTO
Infrastructure         → DevOps (OCI)
Testing                → QA
Planning / timeline    → Project Manager
```
