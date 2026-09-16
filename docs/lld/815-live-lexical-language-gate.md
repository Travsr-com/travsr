# LLD 815: the live lexical floor must not resolve a name across a language boundary

## Problem

Between commits, the RFC-027 lexical floor emits a wrong `ref/call` edge when a
call's leaf name is a builtin of the caller's language but happens to be the one
repo-wide definition of that name in a *different* language.

Measured on this repo: `travsrAutomation/selftest.py:270` calls the Python
builtin `set(...)`, and the live overlay emits

```
method:CheckRegistry.test_check_names_are_unique  ->  fn:set   (provenance=live)
```

where `fn:set` is `crates/travsr-config/src/lib.rs:490`, a Rust function. Phase B
does not carry this edge, so it is a pure false positive: a wrong, un-ratified
structural edge, the failure class RFC-027 section 8.1 forbids.

## Root cause

The floor's whole precision claim is "this name has exactly one definition, so
resolving it is a lookup, not a guess". The quantifier is mis-scoped: it ranges
over the entire corpus across every indexed language, while the meaning of a
bare identifier ranges only over its own language.

- `crates/travsr-daemon/src/live_resolve.rs:931` `candidate_signatures` returns
  `vec![call.callee_sig.clone()]` (`fn:set`) for a bare free-function call.
- `crates/travsr-daemon/src/live_resolve.rs:963` `unique_definition` counts
  matching nodes repo-wide with no language predicate, so the single Rust
  `fn:set` satisfies the exactly-one gate.
- `crates/travsr-daemon/src/live_resolve.rs:987-996` `edge_if_sound` gates on
  `src_node.vname.corpus != dst_node.vname.corpus` and self-edges only. Nothing
  compares `vname.language`, which `VName` already carries
  (`crates/travsr-core/src/lib.rs:336`).

So it is not a missing check bolted onto a sound rule; the rule itself is stated
over the wrong domain. A language builtin is invisible to the graph precisely
because it is not a repo symbol, which is what makes the foreign definition look
unique.

The section 7.3 step-1 gate (`locally_bound`) is unrelated: it covers locals and
parameters in the caller's own file, not builtins.

## Options considered

### Option A: compare `vname.language` inside `edge_if_sound` (the issue's suggestion)

Rejected. `edge_if_sound` is shared by both emit lanes, and the two lanes hold
different evidence:

- the lexical floor's evidence is name uniqueness, which does not survive a
  language boundary;
- the editor lane's evidence (`resolve_one`, live_resolve.rs:174) is a language
  provider's answer for a concrete position, which does.

A gate in the shared function silently drops correct editor-witnessed edges. The
concrete regression is the C family: `Language::from_extension` maps `.h` to
`C` and `.cpp`/`.hpp` to `Cpp` (`crates/travsr-core/src/lib.rs:170-172`), and
`crates/travsr-analysis/src/cpp.rs:79 header_is_cxx` deliberately errs toward
classifying an ambiguous `.h` as C. A C++ translation unit calling a function
declared in a plain-looking `.h`, or an Objective-C `.m` calling the same, is a
`cpp -> c` / `objectivec -> c` `ref/call` that clangd resolves correctly and that
the graph is supposed to have. Those languages run the EditorOnly lane
(`crates/travsr-daemon/src/lib.rs:3742 live_language`), so option A breaks
exactly the cross-language edges that are real.

### Option B: filter candidates to the caller's language before the exactly-one test

Rejected, in the other direction. Today two definitions of one name in two
languages abstain as ambiguous; filtering first would promote the caller's-
language one to "unique" and emit it. That converts an abstention into a guess
and widens the lane's emit surface, against section 8.1 and the fail-closed
precedent of #529 / #604 / #606. The fix for a wrong edge must not buy recall.

### Option C (chosen): make the cross-language allowance an explicit per-lane argument

`edge_if_sound` takes a `CrossLanguage` argument naming what the calling lane's
evidence is worth. The lexical floor (`lexical_one`, `inheritance_edge`) passes
`CrossLanguage::Refused`; the editor lane (`resolve_one`) passes
`CrossLanguage::Allowed`.

## Chosen design

```rust
enum CrossLanguage { Allowed, Refused }

fn edge_if_sound(store, src, dst, kind, cross_language: CrossLanguage) -> Option<Edge> {
    if src == dst { return None; }
    let src_node = store.get_node(src).ok().flatten()?;
    let dst_node = store.get_node(dst).ok().flatten()?;
    if src_node.vname.corpus != dst_node.vname.corpus { return None; }
    if cross_language == CrossLanguage::Refused
        && src_node.vname.language != dst_node.vname.language
    {
        return None;
    }
    Some(Edge::new(src, dst, kind))
}
```

Both endpoint nodes are already fetched for the corpus fence, so the gate costs
no extra store round trip. The argument is a named enum rather than a bool so
every call site states its lane's evidence, and a future lane cannot inherit the
wrong default by omission.

## Why this is optimal here

- It fixes the rule at the point the rule is wrong (the lexical floor's
  uniqueness scope) instead of at a shared choke point that also carries sound
  edges.
- It is strictly precision-improving: no candidate set changes, so no edge can be
  emitted that was not emitted before.
- It reuses the existing gate function and its already-fetched nodes, so there is
  no second lookup path to keep in sync.

## Why it does not break legitimate cross-language edges

1. `EdgeKind::FFICall` is not in this lane at all. `live_edge_kind`
   (live_resolve.rs:1006) accepts only `RefCall`, `RefField` and
   `IsImplementation` and explicitly refuses `"ffi/call"`, with a test pinning
   it. Real FFI edges are emitted at index time by
   `crates/travsr-indexer/src/ffi_resolver.rs` and are untouched.
2. The lexical floor runs only for TypeScript, Rust and Python: `live_language`
   maps exactly those three to `LiveLane::Native` and every other language to
   `LiveLane::EditorOnly`. Those three share no linkage namespace, so a genuine
   call between them is an FFI boundary (napi, PyO3), handled by (1).
3. The one place a same-corpus cross-language `ref/call` is real, the C / C++ /
   Objective-C family across shared headers, is EditorOnly. It resolves through
   `resolve_one`, which keeps `CrossLanguage::Allowed`, so clangd's answers still
   become edges.
4. Java / Kotlin interop and every other polyglot or generated-binding case is
   likewise EditorOnly and keeps its cross-language ability.

The only thing removed is a cross-language edge asserted on nothing but a name
collision.

## Test plan

In `crates/travsr-daemon/src/live_resolve.rs` tests:

1. `a_builtin_call_does_not_resolve_across_a_language_boundary`: a Python caller
   with an `UnresolvedCall` for `fn:set` and a unique Rust `fn:set` in the store
   abstains (`emitted: 0, pending: 1`, no edge).
2. `a_same_language_unique_definition_still_resolves`: the same shape with a
   Python `fn:set` still emits, so the guard is not a blanket disable.
3. `an_editor_resolved_cross_language_target_still_emits`: a `cpp` caller and a
   `c` header definition through `apply_live_resolutions` still emits, pinning
   that the editor lane keeps its cross-language reach.
4. Existing tests continue to pass unchanged (their fixtures are single-language).

## Risks

- **Recall on a mixed-language file.** None reachable: node language comes from
  the file's extension, so a caller and a callee in the same language always
  agree. A misclassified file would abstain, which is the fail-safe direction.
- **A future language promoted to the Native lane.** If C or C++ ever gains a
  native extractor, the lexical floor would start abstaining on genuine
  `cpp -> c` header calls. That is a recall loss, not a wrong edge, and the fix
  is then a language-compatibility predicate. Encoding one now would be
  speculative: no Native-lane language can reach it.
- **Behavioral drift with #814.** This changes only the emit decision, not the
  preserve-unchanged-edges mechanism, so the two are independent.
