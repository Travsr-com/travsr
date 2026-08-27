# RFC-027: Live Semantic Resolution — LSP-Assisted Incremental Phase B

**Date:** 2026-08-24
**Status:** Draft
**Phase:** Post-v0.11.0
**Author:** Abhishek
**Related:** ADR-009 (SCIP vs LSIF wire format), ADR-002 (edge provenance policy), ADR-006 (subprocess trust), ADR-017 (unified plugin sandbox trust), RFC-005 (cross-language edge resolution), RFC-008 (multi-language extension architecture), RFC-009 (cross-language bridge plugins), RFC-014 (Phase B symbol unification)
**Supersedes:** N/A

---

## 1. Summary

Travsr's semantic graph (Phase B) is **commit-gated**: cross-file, type-resolved edges are only rebuilt when the git hook fires. Between commits, only Tree-sitter (Phase A) updates the graph, which sees *that* a reference exists but not *which* symbol it resolves to. The result is **mid-edit degradation**: the semantic graph goes vague on exactly the code the developer is actively changing.

This RFC proposes a **live semantic resolution lane** that fills the between-commits gap without regressing the commit-gated guarantee. It rests on a strict separation of responsibilities:

```
Tree-sitter  →  DETECTS      a reference exists            (live, cheap, always on)
SCIP graph   →  OWNS         node identity (Kythe VName)   (durable ground truth)
LSP          →  DISAMBIGUATES a specific position          (live, type-aware, surgical, optional)
Commit SCIP  →  RATIFIES     the region, heals drift       (deterministic convergence)
```

LSP is used **only as a disambiguation oracle over SCIP-identified candidates** — never as a symbol source, never for identity, never for full-repo extraction. When no language server is available, the system degrades gracefully to today's commit-gated behavior with **zero regression**.

The design is precision-first and fail-closed: the live lane may only emit an edge when resolution is provably correct, and abstains (marks the reference *pending*) otherwise. Honest staleness beats confident wrongness.

---

## 2. Motivation

### 2.1 The felt problem (dogfooded)

CLAUDE.md's dogfooding mandate says "the friction you feel is the bug report." This RFC responds to a friction hit repeatedly while developing Travsr itself: after editing a file, `get_callers`, `get_blast_radius`, and `find_references` on symbols in that file return stale or incomplete semantic edges until the next commit. The developer (or agent) navigating their own in-progress change gets the *least* reliable graph at the *moment they most need it*.

### 2.2 Why this is not solved by the existing stack

- **Phase A (Tree-sitter)** runs live on save and updates structural nodes/edges, but cannot resolve semantic edges: it knows `user.save()` is a call, not *which* `save`. Name resolution requires scope + type analysis Tree-sitter does not perform.
- **Phase B (SCIP)** is correct but commit-gated by deliberate decision (recorded in code at `travsr-daemon/src/lib.rs` — "Phase B stays commit-gated" / "still commit-gated on purpose"; commit-gating is a cost concession, not a belief that staleness is acceptable). Running full SCIP on every keystroke is not viable.

The gap is narrow but real: **cross-file, type-resolved edges for uncommitted edits.** This RFC closes that gap for the surface where it hurts (the IDE) while preserving every existing correctness property.

### 2.3 Scope of the value

Be precise about what this buys: the durable graph converges at commit **regardless** of this feature. What the live lane buys is **correctness latency** — precise resolution in seconds mid-edit instead of at the next commit. That is a freshness improvement, not new capability. It is nonetheless high-value because the pain it addresses *is* a freshness problem.

---

## 3. Non-Goals

- **Not** a replacement for SCIP. SCIP remains the durable, deterministic ground truth and the sole source of cross-corpus identity.
- **Not** LSP as a graph builder or identity source. Prior analysis (see §11 Alternatives) rejected both.
- **Not** a cross-language feature. Live edges are strictly intra-corpus; they never enter the `BridgeRegistry` (RFC-009).
- **Not** a change to the commit-gated Phase B pipeline's outputs. The committed graph is byte-for-byte what a full reindex would produce (Invariant #4).
- **Not** a headless-daemon feature in v1. Initial delivery targets the IDE surface where a language server is already running (§7.6).

---

## 4. Background & Prior Art

- **ADR-009** established SCIP for new languages and LSIF for incumbents, keyed on *stable, package-qualified symbol identity*. That identity is the foundation this RFC builds on and must never be weakened.
- **ADR-009 Rule 4** forbids feeding *synthetic* (non-corpus-verified) symbols into the bridge registry, because they can collide with a real symbol from a different corpus and violate RFC-005's `src.corpus == dst.corpus` invariant. LSP-derived resolutions are exactly such synthetic symbols; this RFC's fencing rule (§8.2) is a direct consequence.
- **ADR-002 Rule 1** requires every edge to carry a provenance tag (`provenance TEXT NOT NULL DEFAULT 'tree-sitter'`). This RFC adds a new value (§9.1).
- **Principal Architect Invariant #4** (incremental correctness): a full reindex and an incremental reindex of the same codebase must produce identical graphs. This is the load-bearing correctness property and §8.3 discharges it.
- **Principal Architect Invariant #3** (LLM prohibition on structural reasoning): note LSP resolution is *deterministic compiler-grade analysis*, not an LLM. It does not violate Invariant #3. It is an algorithm, used as an oracle.

---

## 5. The Core Model — Four Lanes

| Lane | Produces | Lifetime | Determinism | Provenance |
|---|---|---|---|---|
| Tree-sitter (Phase A) | structural nodes/edges, reference *detection* | live | deterministic | `tree-sitter` |
| SCIP (Phase B) | semantic edges, stable identity | durable | deterministic (pinned indexer) | `scip` / `lsif` |
| **Live resolution (new)** | semantic edges for dirty regions | ephemeral (until commit) | non-deterministic (env-dependent) | `live` |
| Commit ratification | replaces live edges with SCIP truth | durable | deterministic | `scip` / `lsif` |

The live lane is an **overlay**. It never persists past the commit that ratifies it. The committed graph is always SCIP-derived, which is why Invariant #4 holds (§8.3).

---

## 6. Edit Classification

The correct invalidation region depends on *what* changed. The live engine classifies each save:

### 6.1 Body edit
A change that does **not** alter any symbol's referenced surface — no rename, no signature change, no added/removed export, no new/removed reference crossing the file boundary.

→ **Local re-resolution only.** Re-resolve the edited file's outgoing references. No other file is touched. This is the common case (most edits are body edits) and is cheap.

### 6.2 Interface edit
A change to the *referenced surface* of a symbol that has incoming edges or is exported: rename, signature change, visibility change, deletion, or introduction of a symbol that other files already reference-by-name.

→ **Reverse-closure invalidation** (§6.3). This is the expensive case, paid only when the public surface actually changes — mirroring how incremental compilers (salsa, `tsc --incremental`) invalidate downstream only on interface change, not on every body edit.

### 6.3 Reverse-dependency closure
Given the set `S` of changed symbols in the edited file:

```
closure = { f : file f contains an edge whose target ∈ S }
        ∪ { f : file f contains an unresolved reference whose name ∈ names(new symbols in S) }
```

The store already maintains reverse edges, so the first set is a direct lookup. The second set catches *newly resolvable* references (a symbol that other files referenced-by-name now exists). Files in `closure` have their boundary edges re-resolved against the changed surface.

**Asymmetry (critical):** *outgoing* edges (edited file → others) are recomputable from the edited file alone; *incoming* edges (others → edited file) are not, because their sources live elsewhere. Incoming edges are stable **only if** the referenced surface is unchanged. This is exactly why interface edits trigger the closure and body edits do not.

---

## 7. Resolution Flow

For each new or unresolved reference `R` at position `P` in a dirty file:

```
1. LOCAL SCOPE CHECK (Tree-sitter, deterministic)
   Is R a free/unbound reference (not a local var, param, or shadowed name)?
   If R is import-bound, record the import's declared target.
   → if R binds to a local/param: resolve locally, no graph edge needed. DONE.

2. CANDIDATE SET (graph, by name, narrowed by any import target)
   candidates = graph.symbols_named(R.name) filtered by import/module hint

3a. UNAMBIGUOUS LEXICAL  (|candidates| == 1  AND  local-clean)
    → emit edge to the single candidate.
      provenance = "live", confidence = high.
      This lane requires NO language server.

3b. AMBIGUOUS + LSP AVAILABLE  (|candidates| > 1)
    → query textDocument/definition at P, tagged to the current buffer version.
      location L ← definition response (must match buffer version, else wait/retry)
      node    N ← location_to_node(L)          // §7.5
      if N resolved → emit edge to N. provenance = "live", confidence = high.
      else          → ABSTAIN (mark pending).  // L pointed outside the graph

3c. AMBIGUOUS + NO LSP
    → ABSTAIN. Mark R as ref_resolution_state = pending.

4. ABSTENTION IS HONEST
   A pending reference is surfaced as "call site present, target not yet
   resolved (stale since your edit; resolves at commit)" — never as a
   fabricated edge.
```

### 7.4 Why 3b restores precision without sacrificing it
`textDocument/definition` is **real type-aware resolution** — the language server knows the static type of the receiver, so it resolves `user.save()` to the correct `save`. This converts the fail-closed lexical lane's *"abstain on ambiguity"* into *"resolve precisely,"* raising recall on precisely the cases (method-on-receiver, overloads) the lexical lane must give up — **without** a precision cost, because it is resolution, not a guess. `definition` is also the most universally implemented LSP method (unlike `callHierarchy`), so capability coverage is broad; where absent, 3c applies and precision is never at risk.

### 7.5 `location_to_node` — the dirty-file span nuance
A definition location `L = (uri, range)` is in the *current* buffer state. Mapping it to a graph node requires ranges that match the buffer:

- **Def in a clean file:** SCIP ranges are still valid → direct range lookup.
- **Def in a dirty file:** SCIP ranges are stale (shifted by the edit) → map via Tree-sitter's *current* spans, which were just re-parsed.

The location→node index is therefore **range-source-aware**: current spans for dirty files, SCIP ranges for clean files. The reconciliation cost is confined to the (small) dirty set.

### 7.6 Surface-specific server access
| Surface | Server access | Mechanism |
|---|---|---|
| **VS Code / IDE extension** | piggyback the **already-running** language provider | `vscode.executeDefinitionProvider(uri, position)` routes to the active language extension; no separate process spawned |
| **JetBrains** | piggyback platform's resolution API | equivalent PSI-reference resolution |
| **Headless CLI daemon** | spawn-own or degrade | out of scope for v1; falls to §7.3c / commit-gated |

The value concentrates where the assumption ("a dev has LSP running") holds *and* where mid-edit pain lives: the IDE. In the IDE, LSP usage is genuinely free (no spawn) and surgical (a handful of point queries on dirty files), so LSP's chattiness weakness never bites.

### 7.7 End-to-end sequence

Live path (on save) and ratification path (on commit) in one view. Note the three
terminal states of a reference — **emit (lexical)**, **emit (LSP-resolved)**, and
**abstain (pending)** — and that every `live` edge is deleted and replaced by SCIP
at commit (§8.3), which is why the committed graph is always deterministic.

```mermaid
sequenceDiagram
    autonumber
    actor Dev
    participant ED as Editor / IDE
    participant TS as Tree-sitter (Phase A)
    participant LE as Live Engine
    participant G as SCIP Graph (store)
    participant LSP as Language Server
    participant CI as Commit Hook (Phase B / SCIP)

    Note over Dev,LSP: LIVE PATH — between commits
    Dev->>ED: save file
    ED->>TS: re-parse dirty file
    TS->>LE: nodes + detected references (structural)
    LE->>LE: classify edit (body vs interface §6)
    alt interface edit
        LE->>G: reverse-dependency closure (§6.3)
        G-->>LE: files whose boundary edges need re-resolution
    end

    loop each new / unresolved reference R @ P
        LE->>TS: local scope check (free? import-bound?)
        alt binds to local/param
            LE->>LE: resolve locally — no graph edge
        else free reference
            LE->>G: candidates = symbols_named(R) (import-narrowed)
            alt exactly 1 candidate (local-clean)
                LE->>G: emit edge — provenance="live" (lexical)
            else ambiguous AND LSP available
                LE->>LSP: definition(uri, P) [buffer version v]
                LSP-->>LE: location L (must match v, else wait/abstain §11)
                LE->>G: N = location_to_node(L) (§7.5)
                alt N in graph
                    LE->>G: emit edge — provenance="live" (LSP-resolved)
                else L outside graph
                    LE->>G: mark R pending (abstain)
                end
            else ambiguous AND no LSP
                LE->>G: mark R pending (abstain)
            end
        end
    end

    Note over Dev,CI: RATIFICATION PATH — on commit (Invariant #4)
    Dev->>CI: git commit (hook fires)
    CI->>CI: incremental SCIP over changed ∪ closure
    CI->>G: insert SCIP edges (SCIP wins by ADR-002 precedence)
    CI->>G: delete leftover provenance="live"; clear resolved pendings; GC ghosts
    Note over G: steps run in one WAL transaction (§8.3 — no torn intermediate)
    Note over G: committed graph == full reindex (§8.3) — deterministic, live-free
```

---

## 8. Correctness Contract

### 8.1 Precision-first, fail-closed
The live lane's target is **precision ≈ 1.0**, achieved by construction: it emits only (a) unambiguous lexical matches that are local-clean, or (b) LSP-resolved locations that map to a graph node. Everything else abstains. Rationale: for a product whose thesis is "zero structural hallucinations," a false edge is not a quality regression but a **breach of the value proposition**, and the failure modes are asymmetric — a missing edge fails safe and recoverable (fall back to grep/read), a wrong edge fails dangerous and silent (agent traverses to the wrong target). Precision ~1.0 or do not emit.

### 8.2 Fencing rule (Invariant #1 + ADR-009 Rule 4)
Live edges are **intra-corpus only**. They:
- carry `provenance = "live"`,
- are **never** inserted into the `BridgeRegistry` or any cross-corpus resolution path,
- never create or mutate node *identity* — they only attach edges between **already-existing** SCIP-identified nodes (or Tree-sitter nodes within the dirty file). VName minting remains SCIP's exclusive responsibility.

This preserves RFC-005's `src.corpus == dst.corpus` invariant and Principal Architect Invariant #1 (VName uniqueness): the live lane cannot synthesize a colliding identity because it never synthesizes identity at all.

### 8.3 Convergence (Principal Architect Invariant #4)
At commit, ratification rides the **whole-project** Phase B run. (The RFC originally specified incremental SCIP over `changed_set ∪ reverse_closure`; that machinery does not exist — `invoke_phase_b_all` runs over full-project inputs and file-level delta is unbuilt, `DEBT(travsr-25)`. Region-scoped ratification is a future optimization gated on that debt.) The pass then:
1. inserts the SCIP-derived edges (which dominate any co-located `live` edge by ADR-002 precedence — the existing `ON CONFLICT(src,dst,kind) DO UPDATE` upsert in `travsr-store/src/lib.rs` already lets `lsif`/`scip` win),
2. deletes the leftover `provenance = "live"` edges in that region that SCIP did not overwrite,
3. clears `pending` reference markers SCIP resolved; GCs ghost nodes/edges from deletions.

**Ordering rationale (delete-old / insert-new hazard).** A naive "delete all `live` edges, then insert SCIP" sequence opens a transient-gap window: a concurrent query landing between the two steps would see *neither* the live edge (already deleted) nor the SCIP edge (not yet inserted), momentarily reporting a missing edge on ratified code. Insert-then-delete closes that window: the only observable intermediate is a **superset** of the ratified graph, never a gap.

**Atomicity, corrected (confirmed in implementation).** These steps do **not** run in one WAL transaction, and cannot without restructuring the Phase B write path. That path is already a sequence of independently-committing statements — `put_edge_lsif` per LSIF edge, `write_phase_b_batch`, `write_scip_attributed_batch`, a second `write_phase_b_batch` for natively-resolved edges, `record_edge_sites`, `reconcile_edge_languages` — all under one *process-local* store mutex, which is not a transaction. The realizable guarantee is **ordering + store-lock exclusion + marker-gated visibility**: the sweep runs while that mutex is still held, after the last ratification write and before the `phase_b_commit` marker advances. In-process readers share the mutex and never observe an intermediate; a separate-process reader can, and sees only the harmless superset above. This answers review-ask #1: the hazard is real, and is dissolved by ordering, not by transaction atomicity.

**Most live edges never reach the sweep.** A live edge that Phase B re-derives is ratified *in place*: the ratification write upserts the same `(src, dst, kind)` row and relabels its provenance (`lsif`/`scip`, or `tree-sitter` when the edge came from native leaf-name resolution, which is itself only ever written on a Phase B path). By the time the sweep runs, the rows still marked `live` are exactly those Phase B did **not** re-derive, so deleting them cannot lose a real edge. This is why the ratification writes must not be prevented from overwriting a `live` row.

**The overlay must be purely additive** (confirmed in implementation; this is what makes the sweep safe). Emitting a live edge creates a row that was absent, or refreshes one the live lane already owns. It must **never** relabel a row another lane wrote. Otherwise the sweep — which deletes rows — could reach an edge the overlay did not create, and retiring the overlay would destroy pre-existing truth instead of returning the graph to it.

The hazard is concrete rather than theoretical: an interface edit re-resolves the files that *reference* the edited one (§6.3), and those files were not re-parsed, so their `tree-sitter` edges are still in place. An upsert that relabelled them would hand them to the sweep, and any one Phase B did not happen to re-derive would vanish. The convergence property below is what surfaced this.

Stated as an invariant: **every row the sweep can delete is a row the live lane created**, so ratification is a return to the pre-overlay graph rather than a mutation of it.

**The sweep is language-scoped, not blanket.** `made_progress` advances the `phase_b_commit` marker whenever *any* language produced results, even when another language's sidecar crashed (#712). A blanket `DELETE FROM edges WHERE provenance='live'` would therefore discard live edges for a language whose SCIP truth was never re-derived in that run. The sweep is restricted to the languages that completed, keyed on the src node's language. Live edges for a crashed language survive, still labeled `live`, which is honest. Invariant #4 is unaffected: a clean run has nothing crashed, so the scoped sweep is total, and that is the case the convergence property below asserts.

**Property (must hold, property-tested):**
```
graph(G) --overlay--> G' --ratify--> graph(G)
```
Ratifying the overlay returns the graph to exactly what it was before the overlay existed, provenance included. This is the form the property actually takes in code, and it is stronger than a count comparison: a `live` row sitting where a ratified one belongs has the same count and the wrong meaning.

It composes with in-place ratification to give the original statement. An overlay edge Phase B re-derives is relabelled and survives; one it does not re-derive is swept; either way the committed graph carries zero live edges and is what the deterministic pipeline produces. Invariant #4 is discharged: the live overlay never survives the run that ratifies it.

Note the property is asserted against the *pre-overlay* graph rather than against a from-scratch reindex. Those differ for an unrelated, pre-existing reason — Phase B is whole-project and commit-gated (`DEBT(travsr-25)`), so an incremental Phase A pass legitimately lacks semantic edges a full index would have. Asserting equality with a full reindex would be asserting something RFC-027 does not claim and does not fix.

### 8.4 Determinism fence
Live resolution depends on the installed server version and is therefore non-deterministic across environments. This is **fenced**: non-determinism exists only in the ephemeral overlay, between commits. The durable (committed) graph is SCIP-pinned and deterministic. The overlay was never part of the deterministic ground-truth contract, so the fence is sound. Live edges MUST be visibly distinguishable at query time (§10) so no consumer mistakes the overlay for ratified truth.

**Reconciliation with the #688 editor plane.** The daemon already has an editor plane (`ControlMessage::ReportLspDiagnostics`) whose written contract is that editor-derived data is volatile and *"never enters the graph … can never be an edge or a node."* Persisting `live` edges appears to cross that line. It does not, and the distinction is what makes this sound:

- **#688 carries the editor's claim about the code.** A diagnostic is the editor's own judgement, it has no counterpart in the repository, and nothing later replaces it with a derived truth. Admitting it to the graph would make the graph unreproducible with no path back.
- **RFC-027 carries a position, not a claim.** The editor answers only "what does the cursor at this position point at" — the one question a language server is authoritative for. It never names a node, never mints a VName, and never asserts a relationship. The daemon maps both endpoints to nodes itself against SCIP-owned identity (§7.5, §8.2), so the graph owns everything downstream of that answer.
- **The result is bounded by ratification, not by trust.** Every live edge is either relabelled by a Phase B write or swept (§8.3), so the non-determinism has a guaranteed end. A diagnostic has no such terminating event, which is exactly why it stays in its own plane.

The two rules are therefore the same rule: *nothing whose non-determinism has no terminating event may enter the graph.* #688 has none, so it stays out; a live edge's terminating event is the next Phase B run.

---

## 9. Data Model & Schema

### 9.1 Provenance (ADR-002 Rule 1)
Add one value to the edge provenance enum:

| Source | `edges.provenance` |
|---|---|
| Tree-sitter (Phase A) | `tree-sitter` |
| LSIF | `lsif` |
| SCIP | `scip` |
| Cross-language bridge | `bridge:<mech>` |
| **Live resolution (new)** | `live` |

Note: this table reflects the **de-facto** enum in the shipped store (`tree-sitter` / `lsif` / `scip`, plus `bridge:<mech>` from ADR-009), which already diverged from ADR-002's originally written value list (`tree-sitter` / `lsif` / `merged`, where `merged` was reserved-for-future and `scip` was added later). `live` extends that de-facto set; it does not reinstate `merged`.

`live` edges are the only provenance the commit ratifier is permitted to bulk-delete-and-replace.

**No schema migration is required** (confirmed in implementation). `edges.provenance` is an unconstrained `TEXT NOT NULL DEFAULT 'tree-sitter'` column with no `CHECK` (`travsr-store/src/migrations/v2_edge_provenance.sql`), so `'live'` is a code change at the precedence-bearing insert sites, not a migration. Only the `ref_resolution_state` table (§9.2) needs one.

**Precedence in code.** `put_edge_live` upserts with `WHERE edges.provenance IN ('tree-sitter','live')`, so a live edge upgrades a heuristic row, is idempotent over itself, and can never overwrite `lsif`/`scip`/`bridge:*`. Re-emission has to be idempotent because `reindex_replace` deletes every outbound edge of a file on each save, so the engine re-emits for the whole file rather than only for references it believes are new.

### 9.2 Reference resolution state
References detected by Tree-sitter but not yet edge-resolved carry a `ref_resolution_state` (named to avoid collision with the daemon's existing `record_dart_resolution_state` bookkeeping in `travsr-daemon/src/lib.rs`, which tracks Dart Phase B availability and is an unrelated concept):
- `resolved` — an edge exists (any provenance).
- `pending` — detected, not resolved, awaiting live resolution or commit ratification. Surfaced honestly; never rendered as an edge.

**Schema changes require Principal Architect sign-off (§13).**

---

## 10. MCP Surface Impact (Solution Architect)

The live overlay must be *legible* to consumers, never silently blended into ratified truth:

- `get_callers`, `find_references`, `get_blast_radius`, `get_graph_json` gain an optional per-edge `provenance` field, and `live` edges are labeled. Two corrections from implementation: per-edge provenance was **not** "already present" at the MCP surface (`DEBT(travsr-75)` — core `Edge` carried no provenance field, so every BFS-traversed edge reported `tree-sitter` regardless of its stored row), and there is **no** existing `provenance` filter (only `kind_filter`). Threading provenance through `Edge` and the store readers is a prerequisite, landed separately; the filter argument is a small additive schema edit.
- Responses that include a dirty region append a freshness note in the `<travsr-data>` envelope: `live_overlay: { dirty_files: N, pending_refs: M }`, so an agent knows the answer includes un-ratified edges and where the gaps are.
- **No new MCP tool.** This is a data-quality/provenance annotation on existing tools, not a new contract. MCP-as-only-interface (Invariant #6) is unaffected.
- Default behavior is additive: consumers that ignore provenance see a *fresher* graph; consumers that require ground truth filter to `provenance != live`.

---

## 11. Consistency & LSP Settle Protocol

LSP is eventually-consistent after `didChange`. The engine MUST:
1. push the current buffer version to the server (`didChange`) before querying,
2. tag each `definition` request with the buffer `version`,
3. accept a response only if it corresponds to the version queried; otherwise wait for the next settle or retry (bounded),
4. treat a settle timeout as **abstention** (§7.3c), never as a resolved edge.

In the IDE-piggyback path (§7.6), the editor owns `didChange`; Travsr reads through `executeDefinitionProvider`, which already reflects the current buffer, simplifying settle handling.

---

## 12. Measurement & Quality Gates (QA)

The "is the live lane worse than nothing?" question is answered empirically, not by assertion. Ground truth is on tap every commit:

- **Continuous precision meter:** at each commit, diff `live` edges in the ratified region against the SCIP truth. Precision = agreement rate. **SCIP wins all ties** (it is the ratified ground truth); disagreements are logged with the LSP-resolved vs SCIP-resolved target.
- **Gate:** the live lane ships enabled only if measured precision ≥ 0.99 on the fixture corpus (target: zero false positives). If it cannot hold that bar for a language, the lane is disabled for that language — a measured decision, not a guess.
- **Recall telemetry:** the same diff reports the recall the live lane buys over the pure fail-closed lexical lane, quantifying whether the LSP dependency earns its operational cost.
- Fixture corpus reuses the RFC-003 §6 fixtures plus the existing LSIF path as a differential oracle (available for TS/Rust/Python).

---

## 13. Security & Threat Model (defer to Principal Security Engineer)

| Concern | Assessment |
|---|---|
| Persistent language server = long-lived process running build logic (ADR-006/017) | **IDE-piggyback path spawns nothing** — it reuses the server the developer already trusts and runs. The headless spawn path (out of v1 scope) inherits the ADR-006/017 review. |
| Live edges poisoning cross-corpus resolution | Blocked by the fencing rule (§8.2): `live` edges never enter the `BridgeRegistry`. |
| Non-deterministic durable graph | Impossible: live edges never survive a commit (§8.3). |
| Prompt-injection via edge content | Unchanged from existing pipeline; `<travsr-data>` sanitization applies identically. |

**Sign-offs required before Accepted:**
- Principal Architect — schema change (§9), new provenance value, convergence proof (§8.3).
- Principal Security Engineer — headless spawn path (when scoped), IDE-piggyback trust assumption.
- Solution Architect — MCP provenance surface (§10).

---

## 14. Phased Delivery

| Phase | Deliverable | Gate |
|---|---|---|
| **0 — Spike** | TS-only, IDE-piggyback, unambiguous-lexical lane (§7.3a) + LSP disambiguation (§7.3b) on a clean repo. Prove the resolution flow end-to-end. | Demo; measured precision on fixtures. |
| **1 — Edit classification + invalidation** | Body vs interface classifier (§6), reverse-closure invalidation, `pending` state (§9.2). | Property test: no ghost edges after rename/delete fixtures. |
| **2 — Commit ratification + convergence** | Incremental SCIP over `changed ∪ closure`, live-edge replacement, Invariant #4 property test (§8.3). | `graph(full) == graph(incremental+ratified)` green on fixtures. |
| **3 — MCP surface + precision gate** | Provenance labeling, `live_overlay` envelope note (§10), continuous precision meter (§12). | Precision ≥ 0.99 on fixture corpus. |
| **4 — Second language + headless decision** | Rust (differential vs rust-analyzer LSIF). Decide headless spawn path scope with Security. | Per-language precision gate. |

Language choice for the spike is **TypeScript**: mature `tsserver` resolution + an existing LSIF path to use as the differential oracle.

---

## 15. Alternatives Considered

1. **LSP as symbol/identity source.** Rejected: LSP speaks `(file, position)` with no package-qualified identity; reconstructing stable identity from it is lossy and, per ADR-009 Rule 4, forbidden from bridges. (This RFC uses LSP for *location*, not identity.)
2. **Common protocol layer over LSP for all languages.** Rejected as a foundation: LSP's uniformity is skin-deep; capability coverage, identity semantics, per-server bootstrap, and trust surface are all per-language, so it degenerates into "common format + N per-language producers" — which is the SCIP model on a worse substrate.
3. **Pure lexical name-matching heuristic (guess best candidate).** Rejected outright: fabricates edges on ambiguous cases (method-on-receiver, overloads), violating the zero-hallucination thesis. Only the *unambiguous* subset survives, as §7.3a.
4. **Incremental SCIP only, no live lane.** Viable and is effectively the fail-safe floor (§7.3c degradation). Rejected as *sufficient* because it does not close the mid-edit window — SCIP still runs at commit, not on save. Retained as the graceful-degradation baseline.
5. **Do nothing (status quo commit-gated).** The zero-cost option and the guaranteed floor. This RFC is strictly additive over it: no server → exactly status quo.

---

## 16. Open Questions

1. **Headless daemon path.** Should Travsr ever spawn its own pinned language servers for non-IDE consumers, or is commit-gated the permanent answer there? Deferred to Phase 4 with Security.
2. **Interface-edit detection granularity.** Can the classifier reliably distinguish rename from delete+add without the commit SCIP pass? Rename mis-classification only affects the *live* overlay (healed at commit), so a conservative "treat ambiguous as interface edit" is safe but costs recall. Measure in Phase 1.
3. **Multi-dirty-file settle ordering.** When several files are dirty and cross-reference each other, is single-pass resolution sufficient or is a fixpoint needed? Bound the iterations; measure convergence in Phase 1.
4. **Confidence exposure.** Do we surface `live` as a boolean provenance only, or a graded confidence? Solution Architect leans boolean (provenance) for MCP simplicity; revisit if consumers ask. Note the plumbing for graded already exists independently of provenance: the `edges` table already carries a `confidence` column (written today by the LSIF/SCIP upserts in `travsr-store/src/lib.rs`), so a future graded signal can ride that field **without** touching the provenance enum. Recommended resolution: keep `provenance` boolean, reserve `confidence` as the graded channel if a consumer ever needs it.

---

## 17. Risk Register

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Live precision < 0.99 for a language | Medium | High | Per-language gate (§12); disable lane, keep floor. No regression. |
| Dirty-file span mapping bug → wrong node | Medium | High | Confine to dirty set (§7.5); property tests; commit ratification heals. |
| LSP settle race → stale resolution | Medium | Medium | Version-tagged requests; timeout → abstain (§11). |
| Reverse-closure too large on hot files | Low | Medium | Interface-edit-only trigger (§6); bound closure depth; fall to pending. |
| Ghost edges from missed deletions | Medium | High | Reverse-edge cleanup on interface edits; commit GC (§8.3). |
| Consumers treat overlay as ground truth | Medium | Medium | Mandatory provenance labeling + envelope note (§10); filterable. |

---

## 18. References

- ADR-002 — Edge Provenance Policy
- ADR-006 — rust-analyzer Subprocess Trust Model
- ADR-009 — SCIP vs LSIF Wire Format (esp. Rule 4, fencing rationale)
- ADR-017 — Unified Plugin Sandbox Trust
- ADR-018 — Drop Kùzu Backend (SQLite+WAL is the only backend)
- RFC-005 — Cross-Language Edge Resolution (`src.corpus == dst.corpus` invariant)
- RFC-008 — Multi-Language Extension Architecture (Phase A/B)
- RFC-009 — Cross-Language Bridge Plugin System
- RFC-014 — Phase B Symbol Unification
- Principal Architect Invariants #1 (VName uniqueness), #3 (LLM prohibition), #4 (incremental correctness)
- LSP specification — `textDocument/definition`
- VS Code API — `vscode.executeDefinitionProvider`
