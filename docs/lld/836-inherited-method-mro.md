# 836 - Python Phase B: inherited-method calls produce no ref/call edges

## Problem

`self.method()` where `method` is defined on a base class produces no `ref/call`
edge, so `get_callers` / `find_references` miss every caller of an inherited
method. In `psf/requests`, `Session.send` calls `self.resolve_redirects()`,
defined on `SessionRedirectMixin`; `travsr references resolve_redirects` returns
0 references while same-class calls in the same function resolve fine.

## Root cause

The Python extractor recovers the receiver of `self.method()` as the *enclosing*
class only (`resolve_receiver_type_py`, `crates/travsr-analysis/src/phase_b_python.rs:290`),
which is correct: the enclosing class is the receiver's static type.

The gap is in the resolver. `resolve_unresolved_calls` looks up exactly one
signature per receiver-recovered method call, `method:{T}.{leaf}`
(`crates/travsr-daemon/src/lib.rs:2513` before this change), and treated a miss
as terminal. Branch (3) of the #529/#606 three-way split
(`crates/travsr-daemon/src/lib.rs:2549` before this change,
`None => continue`) is the observed failure point, but the true root cause is one
level up: the resolver never consults the `IsImplementation` edges Phase B
already emits for `class Foo(Bar)`
(`crates/travsr-analysis/src/phase_b_python.rs:272`). A class's method table is
its own methods plus its bases' methods; the resolver only ever models the first
half, so every inherited call is invisible to it regardless of how branch (3)
behaves.

The issue's own diagnosis is incomplete in two ways:

1. It frames the fix as relaxing branch (3). Branch (3) is a *guard*, and its
   measurement (1 false : 0 real) stands. What is missing is a resolution tier,
   not a weaker guard. Nothing about branch (3) changes for a receiver with no
   recorded base class.
2. It assumes an `IsImplementation` edge is generally usable for lookup. It is
   not, as recorded today: Phase B derives the base's node id from the
   *subclass's own file path*, so the edge only resolves to a real node when the
   base class is declared in the same file. Cross-module bases (`from .base
   import Base`) point at a node that does not exist and stay unresolvable. The
   `requests` case works because `Session` and `SessionRedirectMixin` share
   `sessions.py`. This bounds the fix's recall, and it is why the retry is keyed
   by file.

## Options considered

**A. Retry the leaf pool for branch (3) when the receiver is a graph type.**
Rejected: this is exactly the pre-#606 behaviour, measured 1 false : 0 real
against rust-analyzer LSIF, and the pre-#604 behaviour at 201 false : 2 real.
In-graph uniqueness is an artifact of std/external types not being indexed.

**B. Have the Python extractor emit `method:{Subclass}.{leaf}` alias nodes for
inherited methods.** Rejected: the extractor is file-local by construction (an
incremental single-file reindex must reach the same answer), so it cannot know a
base's method list. It would also fabricate definition nodes for methods that
have no definition site, corrupting `go_to_definition` and node counts.

**C. Teach the resolver the class hierarchy and retry `method:{Base}.{leaf}` up
the MRO, using only recorded `IsImplementation` edges.** Chosen.

**D. Widen the extractor so the base id is corpus-global rather than
file-scoped.** Rejected for this issue: it changes the node-identity scheme for
inheritance edges across every Phase B language, and a name-keyed global base
lookup reintroduces exactly the same-name collision risk #606 closed. It is a
separate change with its own measurement burden.

## Chosen design

`ClassHierarchy` (`crates/travsr-daemon/src/lib.rs`) is built once per resolution
pass from this pass's `pb_edges`:

- keep the `IsImplementation` edges,
- resolve both endpoints to real store nodes via `get_nodes`,
- keep pairs whose signatures are `class:{name}`,
- index them as `path -> subclass -> [base names in declaration order]`.

An edge whose base endpoint has no node (the cross-file case above) is dropped,
so the index only ever contains inheritance the graph can actually name.

`ClassHierarchy::ancestors(path, class)` walks that index breadth-first with a
visited set and a depth cap, which reproduces C3 linearization for single
inheritance and orders nearer bases before farther ones for mixins.

For every method call that would reach branch (3) (receiver recovered, receiver
is a known graph type, exact `method:{T}.{leaf}` absent), the ancestor chain is
computed once per `(caller path, receiver type)` and its `method:{Base}.{leaf}`
signatures are added to one extra batched `nodes_by_signatures` query. Branch (3)
then takes the first ancestor with an exact hit, and still `continue`s when there
is none.

The resulting candidate list flows through the unchanged downstream gates:
caller-language scoping (E4), the CO-A1 uniqueness gate, and
`call_target_reachable` (#521 F1/F2).

## Why this is optimal here

It reuses all three existing mechanisms rather than adding parallel machinery:
the `IsImplementation` edges the extractors already emit, the exact
`by_recv_sig` signature-lookup tier, and the existing candidate gates. It adds
one batched store query and one batched node lookup, both empty when a repo has
no inheritance edges, so it costs nothing on Rust-only repos. It is
language-general: TypeScript `extends` / `implements` emit the same edge kind and
get the same behaviour for free.

## Why it does not violate the #529/#604/#606 fail-closed precedent

The precedent is that *proximity or uniqueness in the graph is never proof of a
target*. This change never consults leaf-name uniqueness or the leaf pool. It
only ever resolves `method:{Base}.{leaf}` exactly, where `Base` is named by an
inheritance edge the extractor recorded from the source text `class T(Base)`.
That is positive type evidence of the same kind branch (1) uses.

Specifically:

- The #606 counterexample (`std::process::Command.stdin` colliding with a clap
  `enum Command`) is untouched: an external receiver type has no
  `IsImplementation` edge, so its ancestor chain is empty and branch (3) still
  fails closed.
- The #604 receiver-less branch is untouched.
- Branch (2) (receiver type absent from the graph) is untouched and still runs
  first.
- Same-named classes in different files cannot cross-contaminate: the hierarchy
  is keyed by the declaring file, and the lookup uses the caller's own file.
- A recorded base whose `method:{Base}.{leaf}` node does not exist resolves
  nothing.

Net: the set of resolvable calls grows only by calls whose target is named by a
recorded inheritance edge plus an exact method signature.

## Test plan

In `crates/travsr-daemon/src/lib.rs` tests:

- `e4_inherited_method_resolves_through_is_implementation_836` (positive): the
  `requests` shape. `class:Session` and `class:SessionRedirectMixin` in one file,
  `method:SessionRedirectMixin.resolve_redirects` defined, an `IsImplementation`
  edge in `pb_edges`, `recv_type = Session`. Expects one `RefCall` onto the base
  method and one call site.
- `e4_inherited_method_walks_multi_level_mro_836`: a two-level chain resolves
  through the grandparent.
- `e4_inherited_method_requires_a_same_file_inheritance_edge_836` (negative): a
  same-named `class:Session` in an unrelated file carries the inheritance edge;
  the caller's own `Session` does not. Expects no edge.
- `e4_class_receiver_is_a_graph_type_without_method_fails_closed_606` (existing,
  unchanged): no inheritance edge at all, still no edge.
- Existing `t7` / `t13` / `t17` / `t5` / `t10` guards unchanged.

## Risks

- **Recall is bounded to same-file inheritance** by the extractor's file-scoped
  base ids. Cross-module bases stay unresolved. Documented above; a follow-up to
  option D can lift it with its own precision measurement.
- **Multiple inheritance ordering** is breadth-first, not full C3. It differs
  from CPython only for diamonds where two ancestors at different depths both
  define the method, and it always picks a real ancestor's real definition.
- **Cost**: one extra `get_nodes` and one extra `nodes_by_signatures` per pass,
  both skipped when there are no inheritance edges or no branch (3) candidates.
