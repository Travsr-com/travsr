# RFC-014: Phase B Symbol Unification

**Status:** Accepted
**Author:** Tech Lead / Abhishek
**Date:** 2026-06-12
**Crates affected:** `travsr-core`, `travsr-indexer`, `travsr-store`, `travsr-plugin-host`, `travsr-daemon`
**Depends on:** RFC-002 (VName versioning), RFC-003 (multi-lang indexer)
**Issue:** #317

---

## Summary

Phase B (scip-go, scip-python, etc.) currently creates a **parallel SCIP node population**
disjoint from the tree-sitter nodes that all retrieval queries resolve to. This means
`get_callers`, `get_blast_radius`, and `get_execution_path` cannot deliver function-level
answers even though scip-go produces rich call data. This RFC defines the schema changes
and ingest pipeline rewrites (G1–G3) that merge the two worlds into one coherent graph.

---

## Motivation

From the 2026-06-11 k8s forensics (613K-node index):

```
tree-sitter fn/method nodes with ≥1 incoming ref/call:   14 / 139,114   (0.01%)
ref/call edges whose SRC is a file node:                405,648 / 420,134 (96.6%)
ref/call edges into SCIP anonymous locals:              272,102   (64.8%)
```

The graph is rich but unreachable. `get_callers active_deadline` returns anonymous
`file (file)` rows because scip-go's call data lands on SCIP twin nodes, not the
tree-sitter nodes the retrieval layer sees. This is the most critical correctness gap
in the current system.

---

## Detailed Design

### G3 — Drop SCIP anonymous locals at ingest

SCIP `local N` symbols (e.g. `local 27`) represent intra-function SSA temporaries.
They account for 272,102 edges (34% of edge table) and carry zero developer-facing signal.

**Ingest filter:** in `ingest_scip` / `ingest_scip_g2`, skip node creation and edge
emission for any occurrence whose symbol matches `is_scip_anonymous_local` (already
defined in `travsr-core`).

**Schema cleanup:** migration v13 deletes existing `local N` nodes and cascades their
edges. The `is_scip_anonymous_local` predicate is extracted to a SQL helper via
inline deletion.

**Future-proofing:** if intra-function DFG lands later, local symbols return behind
a new `dfg/local` edge kind, not this path.

### G2 — Call-site attribution: re-home ref/call edges from file → enclosing function

SCIP reference occurrences carry source ranges. By binary-searching the file's
function/method spans, each occurrence can be attributed to its enclosing function
instead of the file node.

**Schema change:** add `end_line INTEGER` to `nodes` (migration v13). Tree-sitter
already provides `end_position()` from AST node extent; SCIP definition occurrences
carry 4-element ranges for multi-line spans.

**Attribution algorithm:**
1. After Phase A (tree-sitter) data is written, `end_line` is in the DB for all
   function/method nodes.
2. At Phase B ingest time, for each SCIP reference occurrence at `(path, occ_line)`:
   - `SELECT id FROM nodes WHERE corpus=? AND path=? AND kind IN ('function','method','fn') AND line <= occ_line AND end_line >= occ_line ORDER BY (end_line - line) ASC LIMIT 1`
   - Use the result as edge src; fall back to the file node if no enclosing function found.
3. New `write_scip_attributed_batch(nodes, scip_refs)` in `SqliteStore` performs the
   attribution internally as part of the write transaction.

**New public types in `travsr-core`:**
```rust
pub struct ScipRef {
    pub caller_path: String,
    pub caller_line: u32,
    pub callee_id: NodeId,
}
```

**New public function in `travsr-indexer`:**
```rust
pub struct ScipIngestOutput {
    pub nodes: Vec<Node>,
    pub refs: Vec<ScipRef>,
    pub symbol_map: HashMap<String, NodeId>,
}
pub fn ingest_scip_g2(bytes: &[u8], corpus: &str) -> anyhow::Result<ScipIngestOutput>
```

### G1 — Symbol unification: merge SCIP nodes onto tree-sitter nodes

**Problem:** SCIP definition symbols are disjoint twins of tree-sitter nodes because
SCIP uses raw symbol strings (e.g. `scip-go ... FunctionName().`) while tree-sitter
uses `fn:FunctionName`.

**Identity key:** for every SCIP definition occurrence, derive `(path, name, kind)`:
- Path: `doc.relative_path`
- Name: extracted from the SCIP descriptor suffix (language-specific; Go first)
- Kind: function/method/class/interface/var from the descriptor

Match against tree-sitter nodes: `WHERE corpus=? AND path=? AND kind=? AND signature LIKE ?`
disambiguated by `ABS(line - occ_line)` proximity.

**Schema change:** new `symbol_aliases` table in migration v13:
```sql
CREATE TABLE IF NOT EXISTS symbol_aliases (
    scip_symbol TEXT NOT NULL PRIMARY KEY,
    node_id     INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_symbol_aliases_node ON symbol_aliases(node_id);
```

**Ingest-time merge:** when a SCIP definition matches a tree-sitter node:
- Record `(scip_symbol → node_id)` in `symbol_aliases`
- Attach all that symbol's edges to the tree-sitter node_id (not a new SCIP node)
- Do NOT create a new node for matched in-repo definitions

**External symbols** (stdlib, deps) keep a SCIP node with normalized signature
`ext:<package>/<name>`, full SCIP string retained in `symbol_aliases`.

**Idempotency:** re-ingest Phase B resolves through `symbol_aliases` before creating
nodes — the alias table makes the operation idempotent.

**Per-language rollout flags:** Go first; other languages added with per-language
acceptance metrics. Disabled for a language if the `symbol_unification_enabled`
feature flag in `lang.toml` is absent (defaults to `false` pre-acceptance).

**Signature format version:** version 2 records unification being active.
`travsr status` detects version skew and prompts `travsr migrate`.

### O4 (rider) — Edge evidence: `edge_sites` table

**Schema change:** new `edge_sites` table (migration v13):
```sql
CREATE TABLE IF NOT EXISTS edge_sites (
    src  INTEGER NOT NULL,
    dst  INTEGER NOT NULL,
    kind TEXT NOT NULL,
    line INTEGER NOT NULL,
    FOREIGN KEY (src, dst, kind) REFERENCES edges(src, dst, kind) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_edge_sites_src_dst ON edge_sites(src, dst, kind);
```

Populated by G2's occurrence ingestion: each `ScipRef` carries `caller_line`, which
is written to `edge_sites`. The edge PK stays `(src, dst, kind)` — multiple call
sites share one edge row.

---

## Re-index Policy

Exactly ONE user-visible re-index event:
- Signature format version bump (v13 migration + version 2 constant)
- `end_line` backfill (tree-sitter re-index of all registered repos)
- G3 cleanup migration (automatic, one-shot)
- O7 vacuum (same event)

`travsr status` detects `signature_format_version < 2` and prints:
```
⚠  Graph schema is outdated (v1 → v2).
   Run: travsr migrate   (or: travsr init --reimport)
```

---

## Alternatives Considered

**Alternative A: SCIP-only graph, no tree-sitter.** Rejected — tree-sitter provides
instant structural parsing without build tools; SCIP requires compilation.

**Alternative B: Dual-index + query federation.** Rejected — query routing complexity
and maintenance burden outweigh the benefit. A single unified node ID space is cleaner.

**Alternative C: Post-hoc edge remapping.** Run unification after full ingest, rewire
edges in a second pass. Accepted as the implementation strategy for G1 (the alias table
enables this); rejected as a runtime query layer.

---

## Drawbacks

- G1 matching heuristics can produce wrong merges (false positives are worse than misses).
  Mitigation: line-proximity disambiguation; per-language flags; caller-set ground-truth
  assertions in nightly accuracy suite (O8).
- schema v13 migration deletes 272K+ rows — irreversible on existing indexes.
  Mitigation: re-init is the documented recovery path; `travsr migrate` makes it explicit.

---

## Unresolved Questions

- G1 multi-language: name extraction from SCIP symbols for Java/Kotlin/Python descriptors
  (Go first, others tracked per-language post-acceptance).
- G1 overloaded functions (same name, different signatures): line-proximity is a
  heuristic; richer disambiguation may require SCIP type information.
- O4 `edge_sites` write throughput: insertion of per-site rows during Phase B ingest;
  measure on k8s re-index before declaring acceptable.

---

## Acceptance Criteria (k8s re-index)

- Zero `local N` nodes in `nodes` table after re-index
- Edge count drops ~34% (272K anonymous-local edges removed)
- fn/method nodes with ≥1 incoming `ref/call`: from 14 → >40,000
- `travsr graph active_deadline --direction callers` returns `Kubelet.NewMainKubelet`
- `get_execution_path` returns non-empty path for `syncDeployment → rolloutRolling`
- Database size: k8s ≤500 MB (from 1.15 GB)
- Nightly accuracy suite: ≥80% fn-sourced call edges, caller-set precision ≥70%
