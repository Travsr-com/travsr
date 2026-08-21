# Running Travsr Locally

> Build karne se lekar Travsr ko apne editor me MCP server banane tak ka poora setup.
> Ek deep-dive walkthrough ke liye `docs/dogfooding.md` dekho — wo team ka daily workflow hai.

---

## Prerequisites

| Tool | Version | Note |
|---|---|---|
| Rust | **1.97+** | Workspace MSRV `1.75` hai, par committed `Cargo.lock` ke transitive deps naye rustc maangte hain |
| Node.js | 18 LTS+ | Sirf `bench/run.mjs` harness ke liye |
| Git | 2.x | `travsr init` ko git repo chahiye |
| bubblewrap | any | **Linux only** — Phase B sandbox. macOS `sandbox-exec` khud use karta hai |
| graphviz | any | Optional — `travsr graph --format dot` ko SVG banane ke liye |

```bash
rustc --version
node --version
git --version
```

### Agar MSRV error aaye

```
error: rustc 1.86.0 is not supported by the following packages:
  kstring@2.0.3   requires rustc 1.96.0
  time@0.3.47     requires rustc 1.88.0
```

Rust purana hai. Update karo:

```bash
rustup update stable
```

`rust-toolchain.toml` me `channel = "stable"` likha hai, toh rustup jo bhi stable installed hai wahi use karta hai — auto-update nahi hota. CI `dtolnay/rust-toolchain@stable` use karti hai, isliye project hamesha **latest stable** pe build hone ki umeed rakhta hai, `1.75` pe nahi.

---

## 1. Build

```bash
cd /path/to/travsr

# Sirf CLI, release mode — indexing ke liye yahi use karo
cargo build --release -p travsr-cli
# → target/release/travsr

# Ya poora workspace (saare crates + tests)
cargo build --workspace
```

Pehli baar 5–15 minute lagenge (300+ transitive crates, aur release profile me
`lto = "thin"` + `codegen-units = 1` hai).

> **Gotcha:** `cargo build ... | tail` mat karo. Pipeline ka exit code `tail` ka
> hota hai, cargo ka nahi — fail hua build "success" dikhega. `set -o pipefail`
> use karo ya seedha chalao.

Debug build (`cargo build -p travsr-cli`) tez compile hota hai, par indexing
kaafi slow hoti hai — tree-sitter parsing debug me bahut mehngi padti hai.

---

## 2. Repo index karo

```bash
./target/release/travsr init
```

Ye teen kaam karta hai:

1. Saari source files walk karta hai — `.gitignore` + `.travsrignore` respect karke
2. Graph banata hai → `.travsr/graph.db` (SQLite + WAL, machine se bahar nahi jaata)
3. `post-commit` hook install karta hai aur repo ko `~/.travsr/registry.json` me register karta hai

| Flag | Kaam |
|---|---|
| `--jobs N` | Parallel parse workers (default: CPU cores) |
| `--semantic` | Phase B (call edges) synchronously chalao, background me nahi |
| `--json` | Machine-readable output (summary stdout pe, progress stderr pe) |
| `--quiet` | Progress aur tips suppress karo |
| `--allow-unsandboxed-lsif` | rust-analyzer ko bina sandbox chalao. **Sirf trusted repos pe** |

Graph `init` ke turant baad queryable hai — pehle commit ka wait nahi karna padta.

---

## 3. Verify

```bash
./target/release/travsr status    # nodes, edges, schema version, last-indexed SHA
./target/release/travsr repo      # detected repo root + corpus
./target/release/travsr repos     # saare globally registered repos
./target/release/travsr fsck      # graph integrity check + repair
```

---

## 4. Query — terminal se

```bash
# Natural-language query (PPR + knapsack)
./target/release/travsr ask "how does seed selection work"

# Dependency graph, ASCII tree
./target/release/travsr graph crates/travsr-mcp/src/seed.rs

# Kaun call karta hai
./target/release/travsr graph get_context_body --direction callers --depth 2

# Saare use sites
./target/release/travsr references ppr_weighted

# Structural pattern search
./target/release/travsr pattern "fn *_query"

# Machine-readable
./target/release/travsr graph seed.rs --format json

# Poora repo graph → SVG (graphviz chahiye)
./target/release/travsr graph --all --format dot | dot -Tsvg -o repo.svg && open repo.svg
```

`travsr graph` flags:

| Flag | Default | Values |
|---|---|---|
| `--direction` | `deps` | `deps` · `callers` · `both` |
| `--depth` | `3` | traversal depth |
| `--format` | `tree` | `tree` · `dot` · `json` |
| `--all` | — | poora graph (query ke saath mutually exclusive) |

---

## 5. MCP server ke roop me connect karo

Yahi asli use case hai — apne AI tool ko graph tak pahunch dena.

```bash
# Manually test — newline-delimited JSON-RPC 2.0 over stdio
./target/release/travsr mcp --stdio
```

### Claude Code

```bash
claude mcp add travsr -- /absolute/path/to/travsr/target/release/travsr mcp --stdio --global
```

### Claude Desktop

`~/Library/Application Support/Claude/claude_desktop_config.json` (macOS) ya
`%APPDATA%\Claude\claude_desktop_config.json` (Windows):

```json
{
  "mcpServers": {
    "travsr": {
      "command": "/absolute/path/to/travsr/target/release/travsr",
      "args": ["mcp", "--stdio", "--global"]
    }
  }
}
```

### Cursor

`~/.cursor/mcp.json` (global) ya `.cursor/mcp.json` (per-project) — same shape.

### Modes

| Mode | Command | Kab |
|---|---|---|
| Global | `mcp --stdio --global` | **Recommended.** `~/.travsr/registry.json` padhta hai — har init kiya repo available |
| Single repo | `mcp --stdio --db /path/.travsr/graph.db` | Ek specific repo target karna ho |
| cwd-based | `mcp --stdio` | Repo ke andar se chalao, git root se db discover karta hai |

Global mode me results `[repo-name]` se prefix hote hain, aur `file`/`symbol`
lene wale tools ek optional `repo` parameter bhi lete hain.

---

## 6. Development loop

```bash
cargo test --workspace                                  # ~1,299 tests
cargo test -p travsr-mcp                                # ek crate
cargo test -p travsr-mcp seed                           # naam se filter

cargo clippy --workspace --all-targets -- -D warnings   # CI yahi chalati hai
cargo fmt --all
```

### Retrieval benchmark

Kuch bhi retrieval-related badalne se **pehle** baseline lo:

```bash
cargo build --release          # harness ko release binary chahiye
node bench/run.mjs
# → bench/results.json, bench/report.md, bench/judge-packets.json
```

Kisi doosre repo pe:

```bash
BENCH_REPO=/path/to/kubernetes \
BENCH_QUERIES=bench/queries-k8s.json \
BENCH_TARGETS=bench/targets-k8s.json \
BENCH_LABEL=k8s-mytest \
node bench/run.mjs
```

`bench/report.md` ko safe jagah copy karo — wo tumhara *before* hai. Retrieval
touch karne wale PR me before/after table expected hai (repo ki saari
`bench/report-*.md` files yahi hain).

### Debugging

```bash
RUST_LOG=travsr_mcp=debug,travsr_retrieval=debug ./target/release/travsr ask "query"
```

Har seed/rerank threshold env se override hota hai — recompile ki zaroorat nahi:

```bash
TRAVSR_SEMANTIC_PROMOTE_STRONG=0.70 ./target/release/travsr ask "query"
TRAVSR_CONFIRM_ANCHOR_FLOOR=0.60    ./target/release/travsr ask "query"
TRAVSR_SEMANTIC_VETO_FLOOR=0.50     ./target/release/travsr ask "query"
TRAVSR_KNN_BUDGET_MS=2000           ./target/release/travsr ask "query"
```

Poori list `crates/travsr-mcp/src/seed.rs` ke constants section me hai — har ek
ke comment me measured number likha hai.

Hidden diagnostic MCP tools (`tools/list` me nahi dikhte, naam se call karo):

| Tool | Kaam |
|---|---|
| `seed_trace` | Pura seed pipeline trace — kahan FTS/rerank/abstain gate lagta hai |
| `embed_knn_probe` | Raw embed-KNN ranking, koi fusion nahi |

---

## 7. Semantic embeddings (optional)

Default me graph traversal + FTS5 chalta hai. Vector recall add karne ke liye:

```bash
./target/release/travsr embed list       # available models
./target/release/travsr embed init       # sidecar setup
./target/release/travsr embed reindex    # embeddings build — time lagta hai
./target/release/travsr embed status
./target/release/travsr embed switch <model>
```

Iske bina bhi sab kaam karta hai — `get_context` lexical-only mode me chalega
aur header me `embeddings: off` dikhayega.

---

## 8. Phase B language indexers (optional)

Phase A (Tree-sitter structure) har supported language pe by default chalta hai.
Phase B real cross-file **call edges** deta hai:

```bash
./target/release/travsr lang list                # status per language
./target/release/travsr lang detect              # scan + auto-install
./target/release/travsr lang install <language>
./target/release/travsr lang approve <language>  # network access chahiye ho toh
```

Rust, TypeScript, Python aur Dart me **native Phase B** hai — koi external tool
download nahi hota. Baaki languages sandboxed sidecar tools use karti hain,
jinhe ADR-017 ke tehat explicit per-corpus trust grant chahiye.

---

## Daemon (optional)

```bash
./target/release/travsr daemon start
./target/release/travsr daemon status
./target/release/travsr daemon stop
```

Daemon file watcher chalata hai, git hook dispatch handle karta hai, background
me Phase B schedule karta hai, aur query results cache karta hai. Logs
`.travsr/daemon.log` me jaate hain (daily rotation).

Iske bina bhi sab chalta hai — CLI daemon na mile toh seedha DB se padh leta hai.

---

## Troubleshooting

| Symptom | Fix |
|---|---|
| `rustc X is not supported by the following packages` | `rustup update stable` |
| Build "succeed" karta hai par binary nahi banti | `\| tail` hata do — pipe exit code chhupa raha hai |
| `travsr init` fail — git repo nahi | `git init` pehle chalao |
| Rust semantic edges missing | Sandbox unavailable → rust-analyzer skip ho gaya. Trusted repo ho toh `--allow-unsandboxed-lsif` |
| `get_context` hamesha abstain karta hai | Expected behaviour agar query graph se match nahi karti. `seed_trace` se confirm karo |
| Bench harness binary nahi dhoondh pata | `cargo build --release` chahiye — harness `target/release/travsr` expect karta hai |
| Graph stale lag raha hai | `travsr status` se `last_commit` check karo; `travsr fsck` chalao |

---

## Reference

| Doc | Kya |
|---|---|
| `docs/dogfooding.md` | Team ka daily workflow, Travsr pe Travsr |
| `CONTRIBUTING.md` | Branch naming, PR requirements, coding standards |
| `docs/rfcs/` | 23 RFCs — har major design decision |
| `docs/adrs/` | 14 ADRs — architecture decisions |
| `CHANGELOG.md` | Release history |






core · error · config · ipc          foundation (2,587 LOC)
analysis · indexer · plugin-host     write path (33,495)
plugin-protocol · plugin-sdk         plugin contract (747)
store                                persistence (9,748)
retrieval · rerank                   read path algorithms (4,152)
mcp                                  read path brain (17,758)
daemon · cli                         control plane (13,483)









Travsr: Show Status                      12. Travsr: Graph Stats
 2. Travsr: Show Blast Radius                13. Travsr: Registered Repos
 3. Travsr: Show Callers                     14. Travsr: Re-index Now
 4. Travsr: Show Welcome                     15. Travsr: Languages
 5. Travsr: Refresh Graph                    16. Travsr: Open File Graph
 6. Travsr: Download Binary                  17. Travsr: Refresh Repo Files
 7. Travsr: Show Graph                       18. Travsr: Search Files
 8. Travsr: Ask Symbol                       19. Travsr: Open Context Explorer
 9. Travsr: Manage Synonyms                  20. Travsr: Register MCP Server in Agent
10. Travsr: Show Dependencies                21. Travsr: Check Blast Radius Before Edit
11. Travsr: Show Execution Path              22. Travsr: Copy Graph Context for Chat