# RFC-0NN: <Title — the change, not the problem>

**Status:** Draft
**Author:** <name>
**Date:** <YYYY-MM-DD>
**Crates affected:** `travsr-x`, `travsr-y`
**Depends on:** RFC-0NN (<one-line reason>), ADR-0NN (<one-line reason>)
**Supersedes:** <RFC-0NN, or N/A>
**Issue:** #NNN



---

## Summary

One paragraph. What changes, and what it buys. A reader who stops here should be able to
repeat the gist accurately.

Write this last.

---

## Motivation

The problem, with evidence. Numbers if you have them — this repo has a benchmark harness
and the accepted RFCs use it. `RFC-014` opens with a measurement:

```
tree-sitter fn/method nodes with ≥1 incoming ref/call:   14 / 139,114   (0.01%)
ref/call edges whose SRC is a file node:            405,648 / 420,134   (96.6%)
```

That is why it was accepted quickly. State what is broken and how you know, not what you
intend to build.

---

## Detailed Design

The body. Split into numbered decisions when there is more than one — `D1`, `D2`, … — and
reference those identifiers from the code comments you write later. That is what makes a
citation like `// RFC-022 D2: rerank-weighted PPR personalisation` resolvable years on.

### D1 — <decision>

What it does, where it lives, what the contract is. Be explicit about anything a reader
cannot infer from the code:

- invariants that must hold
- what happens on the failure path
- anything deliberately left unchanged, and why

### D2 — <decision>

…

---

## Alternatives Considered

**Do not skip this. It is the section with the longest useful life.**

For each alternative: what it was, and the specific reason it lost. "Simpler" or "slower"
alone is not a reason — say simpler than what, slower by how much.

This is what stops the same option being re-proposed in six months, and it is what tells a
future reader whether the constraint that ruled it out still holds.

| Alternative | Why not |
|---|---|
| <option> | <specific reason> |

---

## Drawbacks

What this costs. Every accepted RFC in this repo has real entries here — `ADR-007` lists
"not tuned to Travsr-specific graph structure" against its own decision.

An empty Drawbacks section reads as an unexamined proposal.

---

## Unresolved Questions

What is deliberately undecided, and who decides it. If something is being deferred, say
what evidence would settle it.

Better an honest open question than a confident guess that later gets cited as a decision.

---

## Acceptance Criteria

How anyone verifies this shipped and works. Prefer commands and thresholds over prose:

```bash
cargo test -p travsr-x
node bench/run.mjs        # hit@1 must not regress below N
```

If behaviour must stay identical under some condition, state it as a contract and name the
test that pins it.

---

## Notes on using this template

Delete this section before submitting.
        
**Status values in use:** `Draft` → `Proposed` → `Accepted` → `Superseded by RFC-0NN`.
Update it when the code ships. Six documents in this repo currently describe shipped
features while still marked `Proposed` or `Draft` — don't add to that.

**File location:** `docs/rfcs/RFC-0NN-kebab-case-title.md`. ADRs go in `docs/adrs/`.
(Two RFCs currently sit in `docs/adrs/` by accident; that is not the convention.)

**RFC or ADR?** An RFC proposes a change and argues for it. An ADR records a decision that
was made and why. `ADR-018` — dropping Kùzu — is a good short example of the latter:
Context, Decision, Consequences, done.

**Numbering:** take the next free number in `docs/rfcs/`. Check for gaps first — `013`,
`019` and `022` are currently referenced from code with no document behind them (#535).

**Cite it from the code.** The point of the number is that a comment can say
`// RFC-0NN D2: <what>` instead of restating the reasoning. That contract only holds while
the document exists — so if you cite it, land it.
