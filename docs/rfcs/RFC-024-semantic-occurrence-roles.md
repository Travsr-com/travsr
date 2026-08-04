# RFC-024: Semantic Occurrence Roles — Stop Discarding What Phase B Already Computes

**Status:** Draft — for discussion, not yet proposed for sign-off
**Author:** Ritik
**Date:** 2026-08-04
**Crates affected:** `travsr-core`, `travsr-plugin-protocol`, `travsr-plugin-host`, `travsr-indexer`, `travsr-store`, `travsr-mcp`
**Sidecars affected:** every Phase B plugin repo (`Travsr-com/travsr-lang-*`)
**Depends on:** RFC-011 (two-transport plugin architecture), RFC-014 (Phase B symbol unification)
**Issue:** #549

---

## Summary

Phase B runs a full semantic analysis — the same work an IDE performs — and the
ingest path distils it to nodes and edges. Roughly ten percent of what arrives is
stored; the rest is computed, serialized, received, and dropped.

This RFC does not propose new analysis. It proposes carrying semantic facts the
analysers already produce through to the store, behind a **travsr-native
vocabulary** rather than a passthrough of any one indexer's wire format, so that
languages with no SCIP indexer — and languages not yet supported — participate on
the same terms.

---

## Motivation

### What is discarded today

Every SCIP occurrence carries a `symbol_roles` bitfield. All four uses in the
codebase test the same bit:

```
crates/travsr-indexer/src/lsif.rs:864    if occ.symbol_roles & 1 != 0 …
crates/travsr-indexer/src/lsif.rs:911    if occ.symbol_roles & 1 == 0 …
crates/travsr-indexer/src/lsif.rs:963    if occ.symbol_roles & 1 == 0 …
crates/travsr-indexer/src/lsif.rs:1006   if occ.symbol_roles & 1 != 0 …
```

Bit 1 is `Definition`. From `scip-0.7.1/src/generated/scip.rs:3209`:

| Role | Bit | Read | Consequence of dropping it |
|---|---|---|---|
| `Definition` | 1 | ✅ | — |
| `Import` | 2 | ❌ | an import and a call are indistinguishable |
| `WriteAccess` | 4 | ❌ | cannot tell assignment from consumption |
| `ReadAccess` | 8 | ❌ | ↑ |
| `Generated` | 16 | ❌ | generated code looks hand-written |
| **`Test`** | **32** | ❌ | **a test caller looks like a production caller** |
| `ForwardDefinition` | 64 | ❌ | declaration looks like definition |

Everything that is not bit 1 collapses into "reference". `SymbolInformation`
(`scip.rs:1459`) is similarly reduced to its `symbol` field, discarding
`documentation`, `relationships`, `kind`, `display_name` and `enclosing_symbol`.

### The cost, measured

Unreachable-code detection (#543) was prototyped against this repo's own graph.
The naive query — symbols with no incoming `ref/call` — flagged **2,201 of 3,412**
functions. Of the false positives, **557 were tests**, filtered afterwards with
name heuristics (`fn:test_*`, `*_works`, `*_returns`) that are fragile and wrong
for any project not following that convention.

SCIP marked those occurrences `Test` at analysis time. The bit arrived and was
dropped.

The same argument applies to `Generated`, the next largest false-positive class,
and to `documentation`, which is the prose signal absent from embed text while
conceptual retrieval sits at hit@1 0.208.

### Why this is not a feature request

No new analysis is proposed, no new dependency, no new algorithm. The compute has
already been paid for by the analyser; the pipeline is lossy on the way home.
Three separate efforts — #543, `get_callers`/`find_references`, and conceptual
recall — are each blocked on data that already arrives.

---

## Detailed Design

### D1 — A travsr-native occurrence vocabulary, not a SCIP passthrough

The obvious design is to widen the protocol with `symbol_roles: i32` and let
ingest interpret it. **This RFC rejects that**, for a reason visible in the
current plugin set: Rust, TypeScript, Python and Dart use *native tree-sitter*
Phase B with no SCIP anywhere. Those plugins could not populate a SCIP bitfield,
and a protocol field only half the plugins can fill is not a protocol field.

Instead, define travsr's own closed vocabulary in `travsr-core`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum OccurrenceRole {
    Definition,
    Reference,
    Import,
    Read,
    Write,
    Test,
    Generated,
    ForwardDeclaration,
    /// Wire-compatibility escape hatch, not a role. A newer plugin sending a
    /// variant this build does not know deserializes here instead of failing.
    /// See "Adding a variant later" below — this is what keeps D3's
    /// no-version-bump claim true for the *enum* as well as the field.
    #[serde(other)]
    Unknown,
}
```

Each plugin maps its own source vocabulary into it:

| Plugin | Source signal | Maps to |
|---|---|---|
| scip-go, scip-python, scip-java | `symbol_roles & 32` | `Test` |
| scip-* | `symbol_roles & 2` | `Import` |
| LSIF-based | `moniker.kind`, `hoverResult` | `Import`, `Reference` |
| native tree-sitter (rs/ts/py/dart) | enclosing `#[cfg(test)]`, `describe()`, `def test_*` | `Test` |
| any future plugin | whatever it has | same enum |

This is the pattern `EdgeKind` already establishes. Travsr defines `ref/call`;
sixteen languages map into it; the graph never learns which tool produced an
edge. `OccurrenceRole` extends that principle one level down.

**Consequence:** a language added tomorrow participates without a protocol
change. That is the requirement this design exists to satisfy.

#### Adding a variant later

Review on #552 caught a real gap in the first draft, which claimed
`#[non_exhaustive]` was sufficient. It is not, and the two attributes solve
different problems:

- `#[non_exhaustive]` is a **compile-time** guarantee. It stops downstream crates
  writing exhaustive `match` arms and constructing the enum, so adding a variant
  is not a semver break for Rust consumers.
- It says nothing about **deserialization**. An older daemon receiving a variant
  its enum does not contain would fail to deserialize — and because roles arrive
  inside `InvokeResponse`, that failure is not confined to one occurrence. It
  fails the payload, which means a newer plugin silently breaks an older daemon.

That would quietly undo D3. `#[serde(other)]` closes it: the unknown variant
deserializes to `Unknown`, the payload survives, and the occurrence degrades to
"a role we do not understand" rather than taking the response down with it.

**Consumers must treat `Unknown` as they treat `None` from D2** — as absence of
information, never as a negative finding. A role this build cannot interpret is
not evidence that the occurrence is not a test.

This does constrain how variants get added: a new role is only safe if every
consumer already handles `Unknown` conservatively. That constraint is stated here
rather than left to be discovered when the first new role ships.

### D2 — Absent is not false

The field is optional, and the distinction is load-bearing:

```rust
/// `None`  — this plugin cannot determine roles for this occurrence.
/// `Some([])` — plugin looked; none of the known roles apply.
pub roles: Option<Vec<OccurrenceRole>>,
```

A plugin that cannot classify test code must send `None`, **not** an empty
vector. The two mean different things downstream:

- `None` → the consumer must abstain. "We did not look."
- `Some([])` → the consumer may act. "We looked; it is not a test."

Collapsing these is how naive dead-code tools acquire their reputation: silence
from an analyser gets read as a negative finding. This is the same discipline the
retrieval abstention gate already applies to seeds, expressed at the protocol
layer.

It is also what makes partial plugin support safe. A plugin that implements
nothing sends `None` everywhere and degrades to exactly today's behaviour.

### D3 — Wire compatibility: additive, no version bump

`PROTOCOL_VERSION` stays at `1`. The precedent is already in the tree — `refs`
and `unresolved_calls` were both added to `InvokeResponse` after the fact:

```rust
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub refs: Vec<ScipRef>,
```

New fields follow the same shape. An old sidecar omits them; `serde(default)`
yields `None`; per D2 the consumer abstains. A new sidecar against an old daemon
has its extra fields ignored by serde. Neither direction breaks.

A version bump would force every sidecar repo to release in lockstep, which is
precisely the coordination cost this design avoids.

### D4 — Storage

`edge_sites` already keys on `(src, dst, kind, line)` `WITHOUT ROWID`. Roles
attach naturally:

```sql
ALTER TABLE edge_sites ADD COLUMN roles INTEGER;  -- bitset, NULL = unknown
```

`NULL` carries D2's "unknown" through to SQL, so the distinction survives to
query time rather than being flattened at ingest.

### D5 — Phase B coverage map

Roles alone are insufficient for reachability work. "No callers" is only
actionable alongside "and we did analyse this file". Phase B knows its own
coverage at analysis time and never records it.

**This gap is already being worked around.** #450 landed
`SqliteStore::language_occurrence_coverage`, which derives a per-language
file-coverage ratio from `edge_sites` so `find_references` can stop claiming a
definitive zero on partially-indexed languages. It is explicitly a proxy — a
file with genuinely no outbound references is indistinguishable from an
unanalysed one, so the ratio understates true coverage and the threshold has to
be forgiving (95%).

The measured numbers on travsr's own graph show why the real thing is worth
having:

```
rust        161/169 files   95%
typescript    4/65   files    6%
go            1/17   files    5%
```

A recorded coverage map replaces inference with fact, and lets the same question
be answered per file rather than per language. D5 should supersede that proxy
rather than sit alongside it.

```sql
CREATE TABLE phase_b_coverage (
    path      TEXT NOT NULL,
    language  TEXT NOT NULL,
    analyzed  INTEGER NOT NULL,   -- 0/1
    reason    TEXT,               -- when 0: no_analyzer | not_trusted | crashed | skipped
    PRIMARY KEY (path, language)
) WITHOUT ROWID;
```

`PhaseBOutcome` (`plugin-host/src/indexer.rs:14`) already tracks skip reasons per
language — `skipped_no_analyzer`, `skipped_needs_approval`, `failed`. This
persists that at file granularity instead of discarding it after the run.

Without D5, #543 cannot distinguish "nothing calls this" from "this language has
no analyser installed", and cannot abstain honestly.

### D6 — Symbol-level fields

Lower priority; listed for completeness and to keep the scope visible rather than
rediscovered later.

| Field | Value |
|---|---|
| `documentation` | docstrings for embed text — conceptual recall |
| `relationships` | `is_implementation` / `is_type_definition` — a real `get_type_hierarchy` |
| `enclosing_symbol` | **ground truth for RFC-014's G2 attribution**, which currently binary-searches span ranges to infer the enclosing function |

`enclosing_symbol` deserves emphasis: G2 approximates something SCIP states
outright.

---

## Alternatives Considered

| Alternative | Why not |
|---|---|
| **Pass `symbol_roles: i32` through** | Couples the protocol to SCIP. Four of the current plugins are native tree-sitter with no SCIP; they could not populate it. Fails the stated requirement that unsupported languages be accommodated. |
| **`Vec<OccurrenceRole>` with no `Option`** | Loses the unknown/absent distinction, which is the mechanism that makes partial plugin support safe. Absent would read as "not a test", reintroducing the false positives this exists to remove. |
| **Bump `PROTOCOL_VERSION` to 2** | Forces lockstep releases across every sidecar repo. The additive `serde(default)` pattern is already established here and costs nothing. |
| **Free-form `HashMap<String, String>` facts** | Maximum flexibility, no contract. Every consumer would parse strings and guess; a closed enum is what makes `EdgeKind` reliable and the same reasoning applies. |
| **Infer roles in the daemon from paths** (`/tests/`, `_test.go`) | Heuristic, per-language, and wrong for anything unconventional. It is what the #543 prototype did, and it is why that prototype's precision was unusable. |

---

## Drawbacks

- **Work lands in repos this one does not control.** Every sidecar must map its
  vocabulary. Mitigated by D2/D3: unmapped plugins send `None` and keep working,
  so rollout is per-plugin and never blocking.
- **`OccurrenceRole` is a closed enum**, so a genuinely new category requires a
  workspace change. `#[non_exhaustive]` softens external breakage; the friction is
  deliberate, matching `EdgeKind`.
- **`edge_sites` grows a column** on a `WITHOUT ROWID` table — a rewrite on
  migration for large graphs.
- **D5 adds a row per file per language.** Bounded by repo size, but non-trivial
  on a monorepo.
- **Mapping quality varies by plugin**, so `Test` will be more trustworthy for
  scip-go than for a tree-sitter heuristic. D2 lets a plugin be honest about that;
  it does not make the underlying signal uniform.

---

## Unresolved Questions

- Should `roles` live on `edge_sites` or in its own occurrence table? The former
  is cheaper; the latter is cleaner if occurrences later need attributes with no
  corresponding edge.
- Does D5 belong at file granularity or symbol granularity? File is cheaper and
  probably sufficient for abstention; symbol is strictly more informative.
- Should the native tree-sitter plugins attempt `Test` detection at all, or send
  `None` and defer to a future LSP-backed path? A weak signal that is honestly
  labelled may still beat no signal.
- Who owns the mapping table as plugins are added — this RFC, or each plugin's
  own documentation?

---

## Acceptance Criteria

1. A plugin that sends no role data produces **byte-identical** graph output to
   today. Verified by running the existing Phase B fixtures unchanged.
2. `travsr-store` distinguishes `NULL` from `0` in `edge_sites.roles`, and the
   distinction survives to `get_callers` output.
3. With `scip-go` emitting roles, `get_callers` can identify a caller as
   test-only on a Go fixture.
4. D5 coverage map answers, for any file, whether Phase B analysed it — and when
   not, why.
5. #543's naive precision baseline is re-measured with roles available. The
   557-test false-positive class should collapse without name heuristics.

---

## Rollout

Each step is independently shippable and independently useful.

| Phase | Scope | Unblocks |
|---|---|---|
| 1 | `OccurrenceRole` in core; protocol field; store column; ingest writes `None` everywhere | nothing yet — establishes the contract |
| 2 | One plugin (`scip-go`) emits roles | proves the mapping end to end |
| 3 | D5 coverage map | #543 abstention |
| 4 | Surface roles in `get_callers` / `find_references` | user-visible |
| 5 | Remaining plugins; then D6 | conceptual recall, type hierarchy |

Phase 1 alone changes no behaviour. That is intentional — the contract should
land and be reviewed before any plugin depends on it.
