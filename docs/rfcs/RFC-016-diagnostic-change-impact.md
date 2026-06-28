# RFC-016: Diagnostic Change-Impact System — `get_change_impact` MCP Tool

**Status:** Draft  
**Author:** Tech Lead / Abhishek  
**Date:** 2026-06-15  
**Crates affected:** `travsr-core`, `travsr-store`, `travsr-mcp`, `travsr-daemon` (new: `travsr-diagnostic`)  
**Depends on:** RFC-003 (multi-lang indexer), RFC-014 (Phase B symbol unification)  
**Issue:** #345 (fuzz gap — related context), TBD (diagnostic feature tracking issue)

---

## Summary

Travsr can already answer *"what is structurally connected to my change?"* via
`get_blast_radius`. This RFC adds `get_change_impact` — a new MCP tool that answers
*"what is broken after my change?"* by layering a two-tier diagnostic system on top
of the existing blast-radius graph traversal.

**Tier 1 (Structural Lint):** Always runs. Uses the Tree-sitter AST and graph edges
already present after Phase A. Detects dangling references and argument-count
mismatches for all 16 indexed languages, with zero external tool dependencies.

**Tier 2 (Type-Checker Overlay):** Best-effort, per-language. Invokes language-native
type checkers (tsc, pyright, clang JSON, BSP for JVM) on blast-radius files only.
Each provider is independent — a missing or broken provider degrades gracefully to
Tier 1 output.

Results are written to `graph.db` as a new `diagnostics` table after each Phase B
completion and served via the `get_change_impact` MCP tool with zero compute at
query time.

---

## Motivation

### The gap today

After `PaymentService.charge(amount: number)` is refactored to
`PaymentService.charge(amount: number, currency: string)`, an AI agent using
Travsr today must:

1. Call `get_blast_radius` → receive 14 affected files
2. Issue 14 `Read` tool calls to read each file
3. Mentally diff each call site against the new signature — probabilistic LLM
   reasoning that can hallucinate

With `get_change_impact`, step 2 and 3 collapse into a single MCP call that returns
the exact file, line, and error message for each broken call site. **This is the core
token-reduction promise of Travsr applied to correctness, not just context.**

### Why Phase B diagnostic output is insufficient alone

Phase B tools (scip-typescript, scip-go, pyright, scip-clang) produce diagnostic
output as a side effect of running, but Travsr currently discards it — only graph
edges are ingested. More critically, diagnostic availability is inconsistent:

| Language | Phase B tool | Diagnostic output |
|---|---|---|
| TypeScript | scip-typescript / tsc | Structured JSON — reliable |
| Python | pyright | Structured JSON — reliable |
| Go | scip-go | `go build` errors — reliable |
| C++ | scip-clang | Requires `compile_commands.json` — conditional |
| Java | scip-java | Requires Gradle/Maven setup — conditional |
| Kotlin | external | Build-tool dependent — conditional |
| Scala | Bloop/Metals | Requires `bloop export` — conditional |
| Ruby | tree-sitter only | No type checker in Phase B |
| PHP | tree-sitter only | No type checker in Phase B |
| Dart | dart analyze | HOME env issue (tracked) — fragile |
| Swift | sourcekit-lsp | macOS only, cold-start slow |
| C# | dotnet build | Requires project file |

An agent cannot trust a tool that silently returns incomplete data for some languages
and nothing for others. The Tier 1 structural lint solves this: it is the universal
baseline that is always available, always consistent, and always explicit about what
it covers.

---

## Non-Goals

- **Not a replacement for `tsc --watch` or full CI.** `get_change_impact` operates on
  the blast radius only. Errors outside the blast radius (e.g., a downstream consumer
  not yet indexed) are not reported. Users must understand this is "errors we know
  about from the graph", not "all errors in the project."
- **Not a real-time type checker.** Results are computed after each Phase B completion
  and stored, not computed on demand per edit. The latency floor is the Phase B
  background job cadence.
- **Not a linter.** Style violations, unused imports, and non-error warnings from
  external tools are filtered unless they appear in blast-radius files affected by the
  change.
- **Not building a type checker from scratch.** Tier 2 providers wrap existing tools.
  Travsr will never reimplement tsc, javac, or clang.

---

## Detailed Design

### 4.1 Architecture Overview

```
git commit
  │
  ▼
Phase A (Tree-sitter, <100ms)
  │  reindexes changed files
  │  RefCall, DefinesBinding, Depends edges updated
  │
  ▼
Phase B (background, 10–120s depending on language)
  │  scip-go, scip-typescript, pyright, etc.
  │  adds call edges, IsImplementation, Overrides
  │
  ▼
Diagnostic Pass (new, triggered after Phase B completes)
  │
  ├─ Tier 1: structural_lint(store, blast_radius)
  │    ┌─ dangling_edge_scan()   — always O(blast_radius edges)
  │    └─ [future] arg_count_scan() — requires param_count on Node
  │
  ├─ Tier 2: DiagnosticProvider::collect(files, repo_root) per language
  │    ┌─ TscProvider          (TypeScript)
  │    ├─ PyrightProvider      (Python)
  │    ├─ ClangProvider        (C, C++)
  │    ├─ BspProvider          (Java, Kotlin, Scala)
  │    └─ [future providers]
  │
  └─ write_diagnostics(store, all_diagnostics, commit_sha)
       INSERT INTO diagnostics ...
       DELETE WHERE commit_sha != current AND file_path IN blast_radius

                                    ▼
                         graph.db  (diagnostics table)
                                    │
                         MCP query: get_change_impact(file)
                                    │
                         read_diagnostics_for_files(store, blast_radius_paths)
                                    │
                         ControlResponse::query_result(...)
```

### 4.2 New Types in `travsr-core`

Add to `crates/travsr-core/src/lib.rs`:

```rust
/// Mirrors LSP DiagnosticSeverity (1–4). Serialises as lowercase string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticSeverity {
    Error       = 1,
    Warning     = 2,
    Information = 3,
    Hint        = 4,
}

/// How strongly Travsr believes a diagnostic is accurate.
///
/// `Structural` diagnostics are derived from the graph topology alone —
/// they detect shape mismatches (dangling references, wrong argument count)
/// without invoking a type checker. They can produce false positives if the
/// graph is stale but never miss a dangling reference.
///
/// `TypeChecked` diagnostics come from a language type checker and are as
/// accurate as `tsc`, `pyright`, or `clang` on the affected file set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticConfidence {
    Structural,
    TypeChecked,
}

/// A single diagnostic normalized across all source tools and languages.
///
/// Always produced by either the Tier 1 structural lint or a Tier 2
/// `DiagnosticProvider`. Stored in the `diagnostics` SQLite table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizedDiagnostic {
    /// Repo-relative file path where the error occurs (e.g. `src/checkout.ts`).
    pub file: String,
    /// 1-based line number of the diagnostic.
    pub line: u32,
    /// 1-based column number (0 = not reported by this tool).
    pub col: u32,
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub confidence: DiagnosticConfidence,
    /// Stable source identifier.
    /// Tier 1: `"tree-sitter"`.
    /// Tier 2: `"tsc"`, `"pyright"`, `"clang"`, `"bsp/gradle"`, `"bsp/bloop"`.
    pub source: String,
}
```

### 4.3 Storage Design — Migration v15 (`travsr-store`)

New file: `crates/travsr-store/src/migrations/v15_diagnostics_table.sql`

```sql
-- Migration v15: diagnostics table for get_change_impact.
--
-- Stores NormalizedDiagnostic records keyed on (file_path, commit_sha).
-- `file_path` is repo-relative (matches vname.path) — not a FK to nodes(id)
-- because diagnostic tools may report errors in files outside the indexed
-- set (generated files, external headers). Using TEXT avoids a JOIN on read.
--
-- Cache invalidation: on each commit, DELETE WHERE commit_sha != current AND
-- file_path IN blast_radius, then INSERT the new diagnostic pass results.
-- The commit_sha column makes stale entries visible and cleanable.
CREATE TABLE IF NOT EXISTS diagnostics (
    id          INTEGER PRIMARY KEY,
    file_path   TEXT    NOT NULL,
    line        INTEGER NOT NULL,
    col         INTEGER NOT NULL DEFAULT 0,
    severity    TEXT    NOT NULL CHECK(severity IN
                    ('error','warning','information','hint')),
    message     TEXT    NOT NULL,
    confidence  TEXT    NOT NULL CHECK(confidence IN
                    ('structural','type_checked')),
    source      TEXT    NOT NULL,
    commit_sha  TEXT    NOT NULL,
    created_at  INTEGER NOT NULL
);

-- Primary read path: get_change_impact fetches by file_path for a
-- set of blast-radius files. Covering index avoids main-table lookup.
CREATE INDEX IF NOT EXISTS idx_diagnostics_file_commit
    ON diagnostics(file_path, commit_sha);

-- Invalidation path: DELETE stale entries by commit_sha.
CREATE INDEX IF NOT EXISTS idx_diagnostics_commit
    ON diagnostics(commit_sha);
```

New methods on `SqliteStore`:

```rust
impl SqliteStore {
    /// Write a batch of diagnostics for the given commit, replacing any
    /// stale entries for the affected file paths.
    ///
    /// Runs in a single transaction:
    ///   1. DELETE WHERE file_path IN affected_paths AND commit_sha != sha
    ///   2. INSERT all diagnostics
    ///
    /// Idempotent: re-running for the same (paths, sha) pair is safe.
    pub fn write_diagnostics(
        &mut self,
        diagnostics: &[NormalizedDiagnostic],
        affected_paths: &[&str],
        commit_sha: &str,
    ) -> Result<(), StoreError>;

    /// Read all diagnostics for the given file paths at the current HEAD commit.
    ///
    /// Returns an empty Vec when diagnostics have not yet been computed for
    /// these files (Phase B pending, or Tier 2 tool not available).
    pub fn read_diagnostics_for_files(
        &self,
        file_paths: &[&str],
        commit_sha: &str,
    ) -> Result<Vec<NormalizedDiagnostic>, StoreError>;
}
```

### 4.4 New Crate: `travsr-diagnostic`

```
crates/travsr-diagnostic/
├── Cargo.toml
└── src/
    ├── lib.rs               — public API: run_diagnostic_pass()
    ├── structural.rs        — Tier 1: structural_lint()
    └── providers/
        ├── mod.rs           — DiagnosticProvider trait + registry
        ├── tsc.rs           — TypeScript / JavaScript
        ├── pyright.rs       — Python
        ├── clang.rs         — C, C++
        └── bsp.rs           — Java, Kotlin, Scala (BSP protocol client)
```

**Crate dependency rules (updated):**

```
travsr-core        → zero dependencies on other travsr crates
travsr-indexer     → travsr-core
travsr-store       → travsr-core
travsr-retrieval   → travsr-core + travsr-store
travsr-diagnostic  → travsr-retrieval + travsr-indexer        ← NEW
travsr-mcp         → travsr-retrieval                         (unchanged)
travsr-daemon      → travsr-mcp + travsr-indexer + travsr-diagnostic
travsr-cli         → travsr-daemon
```

`travsr-mcp` gains no new dependency — it reads diagnostics via `SqliteStore`
methods already reachable through `travsr-retrieval → travsr-store`.

**Public API of `travsr-diagnostic`:**

```rust
/// Entry point called by travsr-daemon after Phase B completes.
///
/// 1. Computes the blast radius of `changed_files`.
/// 2. Runs Tier 1 structural lint on the blast radius.
/// 3. Runs available Tier 2 providers on the blast-radius file paths.
/// 4. Writes results to `store.write_diagnostics(...)`.
///
/// Never panics. Provider failures are logged and excluded from output;
/// Tier 1 results are always written even if all Tier 2 providers fail.
pub fn run_diagnostic_pass(
    repo_root: &Path,
    store: &mut SqliteStore,
    changed_files: &[PathBuf],
    commit_sha: &str,
);

/// Build the default provider registry from installed tools.
/// Providers whose `available()` check fails are excluded silently.
pub fn default_providers(repo_root: &Path) -> Vec<Box<dyn DiagnosticProvider>>;
```

### 4.5 Tier 1 — Structural Lint (`travsr-diagnostic/src/structural.rs`)

The structural lint operates entirely on the graph — no external processes, no
network, always available.

**Algorithm A: Dangling Edge Detection — O(B × E_avg)**

After Phase A reindexes `changed_files`, some symbols may be renamed, removed, or
have their signatures changed. Any `RefCall` or `DefinesBinding` edge whose `dst`
NodeId no longer exists in the `nodes` table is a guaranteed broken reference.

```rust
/// Scan all RefCall and DefinesBinding edges whose src is in `blast_radius`
/// and whose dst no longer exists. Returns one diagnostic per dangling edge.
///
/// These are guaranteed errors — the callee/definition no longer exists.
/// Confidence: Structural. Severity: Error.
fn dangling_edge_scan(
    store: &SqliteStore,
    blast_radius: &[NodeId],
) -> Vec<NormalizedDiagnostic>;
```

SQL shape:
```sql
SELECT e.src, n_src.vname_path, n_src.line, e.dst
FROM edges e
JOIN nodes n_src ON n_src.id = e.src
LEFT JOIN nodes n_dst ON n_dst.id = e.dst
WHERE e.src IN (?...) 
  AND e.kind IN ('ref/call', 'defines/binding')
  AND n_dst.id IS NULL;
```

**Algorithm B: Argument Count Mismatch — O(B × E_avg) — Phase 2**

Requires `param_count: Option<u8>` to be added to the `Node` struct (migration v16,
separate RFC addendum). Once available:

```rust
/// For each RefCall edge in the blast radius, compare the call-site argument
/// count (from Tree-sitter AST) against the callee's new `param_count`.
///
/// Confidence: Structural. Severity: Error for count mismatch.
/// Cannot detect type mismatches — only count differences.
fn arg_count_scan(
    store: &SqliteStore,
    blast_radius: &[NodeId],
    repo_root: &Path,
) -> Vec<NormalizedDiagnostic>;
```

Algorithm B is explicitly deferred to Phase 2 of the rollout. It is documented here
to establish the schema requirement (`param_count`) so it is added in the correct
migration order.

### 4.6 Tier 2 — `DiagnosticProvider` Trait

```rust
/// A pluggable source of type-checked diagnostics for a specific language.
///
/// Implementations wrap external tools (tsc, pyright, clang, BSP servers).
/// Each provider is independent — a failing provider does not affect others.
pub trait DiagnosticProvider: Send + Sync {
    /// Stable identifier written into `NormalizedDiagnostic.source`.
    /// Must be lowercase ASCII with no spaces (e.g. `"tsc"`, `"bsp/gradle"`).
    fn source_id(&self) -> &'static str;

    /// Languages this provider covers (matches `Language::as_str()`).
    /// Used to skip providers when no files of those languages are in the
    /// blast radius.
    fn languages(&self) -> &'static [&'static str];

    /// Returns `true` when the required tool is installed and the repo is
    /// configured for it (e.g. `tsconfig.json` exists, `compile_commands.json`
    /// exists). Called before `collect` — if `false`, provider is skipped.
    fn available(&self, repo_root: &Path) -> bool;

    /// Run the tool on `files` (repo-relative paths) and return normalized
    /// diagnostics. Must not panic. Must return `vec![]` on tool failure.
    /// Tool stderr is captured and logged at `tracing::warn` level.
    fn collect(
        &self,
        files: &[&Path],
        repo_root: &Path,
    ) -> Vec<NormalizedDiagnostic>;

    /// Confidence tier for all diagnostics this provider emits.
    fn confidence(&self) -> DiagnosticConfidence {
        DiagnosticConfidence::TypeChecked
    }
}
```

**Provider: `TscProvider` (TypeScript, JavaScript)**

```
available(): tsconfig.json exists in repo_root
tool:        node_modules/.bin/tsc OR tsc on PATH
invocation:  tsc --noEmit --pretty false --listFilesOnly false \
                 --files <blast_radius_ts_files>
output:      tsc stderr line format: "path(line,col): error TSxxxx: message"
             parsed with a regex; severity always Error for TS errors
```

`tsc` does not have a stable JSON diagnostic output flag across all versions.
The stderr line format (`file(line,col): severity TScode: message`) has been stable
since TypeScript 1.0 and is safe to parse.

**Provider: `PyrightProvider` (Python)**

```
available(): pyrightconfig.json exists OR pyright on PATH
tool:        pyright
invocation:  pyright --outputjson <blast_radius_py_files>
output:      JSON: { "generalDiagnostics": [{ "file", "range", "severity",
                      "message", "rule" }] }
             Clean JSON — no regex required.
```

**Provider: `ClangProvider` (C, C++)**

```
available(): compile_commands.json exists in repo_root
tool:        clang or clang++ (Clang 15+ required for -fdiagnostics-format=json)
invocation:  clang -fdiagnostics-format=json \
                 -MF /dev/null <file> 2>diags.json
             per file, parallelised across blast-radius C/C++ files
output:      JSON array: [{ "kind", "locations": [{"caret": {"file","line","col"}}],
                             "message" }]
fallback:    if clang < 15: clang-tidy --export-fixes (YAML, parsed separately)
```

**Provider: `BspProvider` (Java, Kotlin, Scala)**

```
available(): .bloop/ directory exists (Scala) OR
             build.gradle / pom.xml exists (Java/Kotlin) with BSP plugin
tool:        bloop (Scala) / build-server-gradle (Java/Kotlin)
protocol:    Build Server Protocol (BSP) over JSON-RPC stdio
             1. Start BSP server
             2. Send build/initialize
             3. Send buildTarget/compile for affected targets
             4. Collect build/publishDiagnostics notifications
             5. Shutdown
output:      BSP DiagnosticItem: { textDocument.uri, range, severity, message }
             Identical structure to LSP publishDiagnostics — uniform parser.
```

BSP is the preferred path because it is project-build-tool agnostic (Gradle,
Maven, sbt, mill all speak BSP) and produces the same JSON schema regardless
of the underlying build system.

### 4.7 Daemon Integration

The diagnostic pass runs as a new phase after `run_background_phase_b_inner`
completes. It uses the `last_changed_paths` meta key (new, written on every
`ReindexCommit`) to scope the blast radius:

```rust
// In travsr-daemon/src/lib.rs — run_background_phase_b_inner:

// ... existing Phase B code ...

// After write_phase_b_results():
let last_changed: Vec<PathBuf> = s
    .get_meta("last_changed_paths")
    .ok()
    .flatten()
    .unwrap_or_default()
    .split('\n')
    .filter(|p| !p.is_empty())
    .map(PathBuf::from)
    .collect();

if !last_changed.is_empty() {
    let commit_sha = s
        .get_meta("last_commit")
        .ok()
        .flatten()
        .unwrap_or_default();
    travsr_diagnostic::run_diagnostic_pass(
        repo_root,
        &mut s,
        &last_changed,
        &commit_sha,
    );
}
```

`last_changed_paths` is written in `reindex_files` (called from
`handle_control_message::ReindexCommit`) as a newline-separated list of
repo-relative paths:

```rust
store.put_meta("last_changed_paths",
    &paths.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>().join("\n")
)?;
```

### 4.8 MCP Tool: `get_change_impact`

Added to `crates/travsr-mcp/src/tools.rs` alongside existing tools.

**Tool spec (MCP JSON schema):**

```json
{
  "name": "get_change_impact",
  "description": "Returns the blast radius of a changed file plus any broken call sites, type errors, and dangling references detected by structural analysis and language type checkers. Use after editing a file to find exactly what broke.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "file": {
        "type": "string",
        "description": "Repo-relative path of the changed file (e.g. src/payment/service.ts)"
      },
      "severity": {
        "type": "string",
        "enum": ["error", "warning", "all"],
        "default": "error",
        "description": "Minimum severity to include in results."
      }
    },
    "required": ["file"]
  }
}
```

**Response shape:**

```json
{
  "changed_file": "src/payment/PaymentService.ts",
  "blast_radius": {
    "affected_files": 6,
    "affected_symbols": 14
  },
  "errors": [
    {
      "file": "src/checkout/checkoutFlow.ts",
      "line": 82,
      "col": 12,
      "severity": "error",
      "message": "Expected 2 arguments, but got 1.",
      "confidence": "type_checked",
      "source": "tsc"
    },
    {
      "file": "src/billing/invoiceService.ts",
      "line": 41,
      "col": 5,
      "severity": "error",
      "message": "Argument of type 'number' is not assignable to parameter of type 'string'.",
      "confidence": "type_checked",
      "source": "tsc"
    }
  ],
  "warnings": [
    {
      "file": "src/reporting/billingReport.ts",
      "line": 119,
      "col": 0,
      "severity": "warning",
      "message": "Wrong argument count: expected 2, got 1.",
      "confidence": "structural",
      "source": "tree-sitter"
    }
  ],
  "clean_files": [
    "src/admin/refundHandler.ts",
    "src/analytics/revenueTracker.ts",
    "src/webhooks/stripeWebhook.ts",
    "src/tests/payment.test.ts"
  ],
  "coverage": {
    "structural": "complete",
    "type_checked": "available",
    "type_checked_source": "tsc"
  },
  "phase_b_complete": true,
  "diagnostic_commit": "a3f1bc9"
}
```

**When Phase B is not yet complete:**

```json
{
  "changed_file": "src/payment/PaymentService.ts",
  "blast_radius": { "affected_files": 6, "affected_symbols": 14 },
  "errors": [],
  "warnings": [],
  "clean_files": [],
  "coverage": {
    "structural": "pending",
    "type_checked": "pending"
  },
  "phase_b_complete": false,
  "diagnostic_commit": null,
  "hint": "Phase B is still indexing call edges in the background. Re-query in a few seconds for diagnostic results."
}
```

**When Tier 2 is unavailable (e.g. Ruby):**

```json
{
  "coverage": {
    "structural": "complete",
    "type_checked": "unavailable"
  }
}
```

The agent always knows exactly what it is and is not getting. Silent incompleteness
is never acceptable.

**Implementation in `tools.rs`:**

```rust
pub fn get_change_impact(
    store: &SqliteStore,
    file: &str,
    severity_filter: DiagnosticSeverity,
) -> String {
    // 1. Find file node
    let file_node = match store.search_nodes(file, 1) { ... };

    // 2. Compute blast radius (reuses existing blast_radius_raw logic)
    let blast = blast_radius_file_paths(store, file_node.id);

    // 3. Read diagnostics from store (zero compute at query time)
    let commit_sha = store.get_meta("last_commit")
        .ok().flatten().unwrap_or_default();
    let all_diags = store
        .read_diagnostics_for_files(&blast.file_paths, &commit_sha)
        .unwrap_or_default();

    // 4. Filter by severity and build response JSON
    let phase_b_complete = store.get_meta("phase_b_commit")
        .ok().flatten()
        .map(|p| p == commit_sha)
        .unwrap_or(false);

    build_change_impact_response(file, blast, all_diags, severity_filter, phase_b_complete)
}
```

### 4.9 Cache Invalidation

`get_change_impact` is served from the existing `#318 O2` LRU query cache in the
daemon. The cache key for all tools is `(tool, args, last_commit, phase_b_commit)`.
When the diagnostic pass runs after Phase B and updates `graph.db`, the
`phase_b_commit` meta key advances, invalidating the cache entry automatically.
No additional cache machinery is needed.

---

## Rollout Phases

### Phase 1 — Foundation (3–4 weeks)

- [ ] Add `NormalizedDiagnostic`, `DiagnosticSeverity`, `DiagnosticConfidence` to `travsr-core`
- [ ] Migration v15: `diagnostics` table + indices in `travsr-store`
- [ ] `write_diagnostics` + `read_diagnostics_for_files` on `SqliteStore`
- [ ] New crate `travsr-diagnostic` with `DiagnosticProvider` trait skeleton
- [ ] Tier 1: `dangling_edge_scan` in `structural.rs`
- [ ] Write `last_changed_paths` meta key in `reindex_files`
- [ ] Daemon: call `run_diagnostic_pass` after Phase B
- [ ] MCP: `get_change_impact` tool (reads from store, no compute)
- [ ] MCP: register in tools list + JSON schema
- [ ] Tests: v15 migration, `write_diagnostics` idempotency, dangling edge detection

**Gate:** `get_change_impact` returns Tier 1 structural diagnostics for all 16
languages. Tier 2 coverage always reports `"unavailable"` at this phase.

### Phase 2 — TypeScript + Python Tier 2 (2–3 weeks)

- [ ] `TscProvider` implementation + stderr parser
- [ ] `PyrightProvider` implementation + JSON parser
- [ ] Integration tests: seed a repo with a known type error, verify `get_change_impact`
      returns it
- [ ] `travsr lang` command extended with `--diagnostics` flag to check tool availability
- [ ] Schema addendum: `param_count: Option<u8>` on `Node` (migration v16)
- [ ] Tier 1: `arg_count_scan` using `param_count`

**Gate:** For TypeScript and Python repos, `get_change_impact` returns `type_checked`
errors matching what `tsc`/`pyright` would output on the blast radius.

### Phase 3 — C/C++ + JVM (4–6 weeks)

- [ ] `ClangProvider` with `compile_commands.json` detection
- [ ] Minimal BSP client (JSON-RPC over stdio) in `travsr-diagnostic`
- [ ] `BspProvider` for Gradle (Java, Kotlin) and Bloop (Scala)
- [ ] Provider registry: auto-detect available providers at `run_diagnostic_pass` time
- [ ] Fuzz target: `fuzz_diagnostic_normalizer` — feeds raw tool output through
      each provider's parser; parser must never panic
- [ ] Benchmark: `get_change_impact` end-to-end latency on a 50K-file repo
      must be < 50ms at query time (all compute is pre-baked by the diagnostic pass)

**Gate:** All four JVM/C++ languages report `type_checked` diagnostics when the
respective tools are installed and the project is configured.

### Phase 4 — Hardening + VS Code UX (2 weeks)

- [ ] VS Code extension: show diagnostic overlay in graph webview (red/yellow nodes)
- [ ] `travsr status` extended: show last diagnostic pass timestamp and coverage
- [ ] Log diagnostic pass timing in daemon startup log (post RFC-[log system])
- [ ] CI: add `get_change_impact` to the nightly accuracy eval harness

---

## Alternatives Considered

### A — Invoke type checkers at MCP query time (on-demand)

Run `tsc` when `get_change_impact` is called, block until done, return results.

**Rejected:** tsc cold start is 3–8 seconds. MCP tool calls that block for 8 seconds
will cause IDE and agent integrations to time out. The pre-baked approach (diagnostic
pass after Phase B, results in `graph.db`) gives zero query-time latency.

### B — Wire Phase B tool stderr directly into diagnostics

Parse whatever each Phase B tool prints to stderr as diagnostics.

**Rejected:** Stderr formats vary per tool, per version, and per invocation flags.
This produces brittle parsers that break on tool upgrades. The `DiagnosticProvider`
trait encapsulates each parser independently and can be versioned separately.
More importantly, Phase B tools produce stderr even when `available()` returns `false`
(tool not installed shows an error, not silence) — parsing that reliably is harder
than the structured API approach.

### C — Use LSP `textDocument/publishDiagnostics` for all languages

Spawn the LSP server for each language, open blast-radius files, collect diagnostics.

**Deferred, not rejected:** LSP is the long-term universal path. However, LSP server
startup costs (JVM warmup for jdtls = 5–30s) make it impractical for the per-commit
diagnostic pass today. BSP is a better fit for project-level diagnostics (Tier 2
Phase 3). LSP becomes the right path if Travsr ever adds per-keystroke diagnostics,
which is explicitly a non-goal of this RFC.

### D — Tree-sitter-only, no Tier 2 at all

Implement only the structural lint; defer type-checked diagnostics indefinitely.

**Rejected as final state, accepted as Phase 1:** Phase 1 ships exactly this, with
the explicit contract that Tier 2 is `"unavailable"` rather than pretending to be
complete. The `confidence` and `coverage` fields in the response make the limitation
visible to agents rather than hiding it.

---

## Drawbacks

1. **New crate complexity.** `travsr-diagnostic` introduces a new build unit. The
   BSP client in Phase 3 adds a JSON-RPC implementation (even if minimal). This is
   bounded complexity — the BSP protocol is stable and small.

2. **False negatives on incomplete graph.** If the blast radius is incomplete (Phase B
   not yet finished, or a new file not yet indexed), structural diagnostics will miss
   errors in unindexed files. The `phase_b_complete` field in the response makes this
   visible but does not fix it.

3. **`last_changed_paths` meta key is single-valued.** If two commits land in rapid
   succession before the diagnostic pass runs, the second commit's paths overwrite
   the first. The diagnostic pass then runs only on the second commit's blast radius,
   missing errors introduced by the first. Mitigation: append semantics (newline
   join) and clear only after the diagnostic pass completes. Full fix is out of scope
   for this RFC.

4. **Tier 2 tools change their output format.** `tsc` version upgrades may change
   stderr format. Each provider's parser must be version-tested. Fuzz target
   `fuzz_diagnostic_normalizer` (Phase 3) partially mitigates this.

5. **Diagnostic table growth.** On a heavily-active repo, the `diagnostics` table
   accumulates rows across many commits. The DELETE-on-invalidation strategy keeps
   only the current-commit diagnostics for blast-radius files, but rows for
   unchanged files persist indefinitely. A periodic GC job (delete WHERE commit_sha
   not in last N commits) is needed — tracked as a Phase 4 follow-up.

---

## Unresolved Questions

1. **`param_count` schema placement.** Should `param_count: Option<u8>` live on
   `Node` (requires migration v16 + schema change) or as a separate `node_attrs`
   key-value table? The key-value approach avoids a schema change but requires a
   JOIN on the arg-count scan hot path.

2. **BSP client library vs. hand-rolled.** Is there a stable, minimal Rust BSP
   client library (`bsp4rs`?) or should we hand-roll the handful of JSON-RPC messages
   we need? Hand-rolling keeps dependencies minimal; a library gives correctness
   guarantees. To be resolved in Phase 3.

3. **Diagnostic deduplication across tiers.** If Tier 1 reports a dangling reference
   at `checkout.ts:82` and Tier 2 (tsc) also reports a type error at `checkout.ts:82`,
   should they be merged into one entry or reported separately? Recommendation:
   report both with their respective `confidence` values — the agent can decide which
   to surface. But this needs a UX decision before Phase 2 ships.

4. **Severity threshold for structural warnings.** Tier 1 arg-count mismatches are
   high-confidence errors. Tier 1 signature-change detections (symbol exists but
   signature differs) are lower confidence — they may be intentional refactors. The
   right default severity for the latter is `warning`, but this may produce noise.
   Resolved experimentally in Phase 2.

5. **`get_change_impact` for multi-file changes.** The current API takes a single
   `file`. Large refactors touch many files simultaneously. Should the tool accept
   `files: Vec<String>` (union of blast radii) or should agents call it once per
   changed file? The union approach is more powerful but the response size may
   exceed token budgets. To be resolved before Phase 1 ships.
