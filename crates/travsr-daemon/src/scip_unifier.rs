//! G1 unification pass — bridges indexer SCIP output with the store.
//!
//! For each SCIP Phase B node that can be matched to an existing tree-sitter
//! node by (path, signature candidates, line proximity), this module:
//!   1. Registers a `symbol_aliases` row so future lookups resolve correctly.
//!   2. Patches `ScipRef.callee_id` to point to the unified TS node so that
//!      `write_scip_attributed_batch` emits edges on the right node.
//!
//! RFC-014 §G1. Language-agnostic: the SCIP descriptor suffix grammar is
//! shared by every SCIP indexer, and `candidate_signatures` covers every
//! Phase A parser's signature convention.  Non-SCIP node signatures (builtin
//! native Phase B plugins) yield no descriptor parse and fall through.

use std::collections::{HashMap, HashSet};

use travsr_core::{NodeId, ScipRef};
use travsr_store::SqliteStore;

/// Fallback line window used only when span-containment finds no match (E6:
/// degenerate/zero-width Phase A spans). Positional span-containment is the
/// primary matcher; this bound just keeps the proximity fallback conservative.
const MAX_LINE_DELTA: i64 = 5;

/// Outcome of a [`unify_all`] pass: the SCIP→TS alias map plus the counts that
/// feed the E6 unification miss-rate on the `travsr status` degradation channel.
#[derive(Debug, Default)]
pub struct UnifyOutcome {
    /// SCIP `NodeId` → unified tree-sitter `NodeId` (all kinds, for ref/edge
    /// remapping — may exceed `unified`, which is callable/type-scoped).
    pub alias_map: HashMap<NodeId, NodeId>,
    /// Callable/type (`function`/`class`) SCIP defs that were real unification
    /// candidates (parsed to a name *and* carried a definition line).
    pub attempted: usize,
    /// Callable/type candidates that matched an existing Phase A node.
    pub unified: usize,
    /// SCIP def nodes that are Sorbet synthetic DSL meta-scopes with no twin and
    /// no possibility of one (#780): RSpec `describe`/`context`/`it` blocks whose
    /// *leaf* is the block itself, and a def defined inside a block that still
    /// found no Phase A twin after its unreconcilable container was cleared.
    /// Neither is a reconciliation failure, so both are excluded from
    /// `attempted`/`unified` and dropped by the caller (node plus its inbound
    /// refs/edges) so they stop surviving as orphan duplicates that steal edges.
    /// Defs in tree-sitter-unindexed files (vendored gem code) are NOT dropped:
    /// they are real navigable definitions, only excluded from the counters.
    pub dropped: HashSet<NodeId>,
    /// #825: one detail row per distinct callable/type SCIP symbol counted in
    /// `attempted` but not in `unified` — i.e. exactly the defs behind the E6
    /// miss rate. Carried out so `travsr status` can name the symbols instead of
    /// only reporting a count, and so a dev can see which language/construct is
    /// failing. The miss set is deterministic, so re-running never changes it.
    pub misses: Vec<UnifyMiss>,
}

/// A single unreconciled SCIP definition: a callable/type the compiler defined
/// that could not be matched to its Phase A tree-sitter node, so its references
/// attribute to an orphan duplicate. One row per distinct SCIP symbol (its
/// first-seen occurrence), mirroring how the miss rate counts symbols.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnifyMiss {
    pub language: String,
    pub symbol: String,
    pub path: String,
    pub line: u32,
    pub kind: String,
}

/// G1 unification pass for all SCIP-indexed languages.
///
/// Must be called **after** Phase A tree-sitter nodes with `end_line` spans
/// are written to the DB, and **before** `write_scip_attributed_batch`.
///
/// Returns the alias map (SCIP `NodeId` → unified tree-sitter `NodeId`) so
/// the caller can drop the now-duplicate SCIP definition nodes from its
/// Phase B batch and rewrite structural edges onto the unified nodes.
pub fn unify_all(
    store: &mut SqliteStore,
    corpus: &str,
    nodes: &[travsr_core::Node],
    refs: &mut [ScipRef],
) -> UnifyOutcome {
    // Maps SCIP NodeId → unified TS NodeId for ref-patching.
    let mut alias_map: HashMap<NodeId, NodeId> = HashMap::new();
    // #780: SCIP def nodes that are synthetic DSL meta-scopes — no twin exists
    // and none can, so they are neither an attempt nor a miss. Collected here so
    // the caller drops them (and their inbound refs/edges) outright.
    let mut dropped: HashSet<NodeId> = HashSet::new();
    // (scip_symbol, ts_id) pairs, registered in one batch transaction below.
    let mut aliases: Vec<(String, NodeId)> = Vec::new();
    // E6: unification attempts/matches for the miss-rate signal. Scoped to
    // callable/type defs (`function`/`class`) — the only kinds that own
    // ref/call edges and can be stolen by an orphaned SCIP twin. `variable`
    // defs (struct fields, module vars) are excluded: many Phase A parsers
    // don't model them at all (e.g. Go struct fields), so counting them would
    // inflate the rate with benign non-matches on every real repo.
    //
    // Counted per *distinct SCIP symbol*, not per occurrence: a symbol whose
    // definition appears in several files (Obj-C `@interface` in the `.h` and
    // `@implementation` in the `.m`, C/C++ `.h` decl + `.cpp` def) is one
    // definition. It unifies against the file that carries the Phase A node,
    // and the other files' occurrences find no same-file tree-sitter node — a
    // benign duplicate, not a real miss. Tracking symbols (rather than adding
    // to the per-occurrence counters on each file) keeps those from inflating
    // the rate regardless of the order files are visited (#596).
    let mut attempted_syms: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut unified_syms: std::collections::HashSet<&str> = std::collections::HashSet::new();
    // SCIP symbol → the tree-sitter node it unified onto (first match wins).
    // Used after the pass to collapse the *other* files' occurrences of the
    // same symbol (Obj-C `@implementation` twin, C/C++ header-vs-source) onto
    // that node so they are dropped as duplicates instead of surviving as
    // orphan def nodes (#596).
    let mut sym_to_ts: HashMap<&str, NodeId> = HashMap::new();
    // Def nodes whose own file carried no matching tree-sitter node — revisited
    // below to see whether the symbol unified in some other file.
    // Carries the candidate signatures too, so the cross-file rung below can
    // re-query without re-parsing the SCIP descriptor.
    // Carries the kind too: the cross-file rung below is restricted by it, and
    // re-deriving it would mean re-parsing the SCIP descriptor.
    // Carries the path and the container-qualified candidates too, for the
    // overload-collapse rung, which is same-file and ignores line distance.
    // Carries the language too: the cross-file rung only matches within it.
    #[allow(clippy::type_complexity)]
    let mut unmatched: Vec<(NodeId, &str, Vec<String>, &str, &str, Vec<String>, &str)> = Vec::new();
    // #825: first-seen detail for each callable/type SCIP symbol that becomes an
    // attempt, so the residual misses (`attempted - unified`) can be named in
    // `travsr status`. Keyed by scip symbol to match the per-symbol counters.
    let mut miss_detail: HashMap<&str, UnifyMiss> = HashMap::new();

    // #780: paths the tree-sitter parser actually indexed. A SCIP def in a file
    // absent from this set is one only the SCIP tool saw — gitignored vendored
    // code (scip-ruby indexes `vendor/bundle`, tree-sitter skips it) — and can
    // never reconcile, so it is excluded from the miss counters and dropped
    // rather than counted as a failure or kept as an edge-stealing orphan.
    let indexed_paths = store.phase_a_indexed_paths(corpus).unwrap_or_default();

    // scip-clang puts the fields of `typedef struct { .. } Animal;` under an
    // anonymous type, while Phase A qualifies them by the typedef. The type
    // names defined in each file are the containers such a field can have.
    let mut clang_types: HashMap<&str, Vec<&str>> = HashMap::new();
    for node in nodes.iter().filter(|n| is_clang(&n.vname.language)) {
        let sym = travsr_indexer::scip_unifier::scip_symbol_from_sig(&node.vname.signature);
        if let Some(p) = travsr_indexer::scip_unifier::scip_name_kind(sym) {
            if p.kind == "class" && !p.name.starts_with(ANONYMOUS_TYPE) {
                clang_types
                    .entry(&node.vname.path)
                    .or_default()
                    .push(p.name);
            }
        }
    }

    for node in nodes {
        let scip_sym = travsr_indexer::scip_unifier::scip_symbol_from_sig(&node.vname.signature);
        // scip-go defines the package (`…/pkg/`) in every file of it, while
        // Phase A models a package once per directory (`go-pkg:`). The per-file
        // def has no twin; kept, it orphans the file and every save of it
        // becomes a whole-file purge.
        if node.vname.language == "go" && scip_sym.ends_with('/') {
            dropped.insert(node.id);
            continue;
        }
        // scip-clang defines a per-file namespace (`<file>/src/main.c`/) in
        // every file, which Phase A has no twin for, as with Go's package.
        if is_clang(&node.vname.language)
            && scip_sym.ends_with("`/")
            && scip_sym.contains("`<file>/")
        {
            dropped.insert(node.id);
            continue;
        }
        // Primary: SCIP descriptor grammar (go/java/ruby/c#/c/c++/… + rust/ts/py
        // LSIF). Fallback: bespoke sidecars (kotlin/swift) whose signatures are
        // Phase-A-style (`fn:Container.name`, `swift::Container.name`) and never
        // parse as SCIP. The fallback is gated to those languages so native
        // Phase A/rust nodes — whose signatures look identical — are never
        // re-unified against themselves.
        let (parsed, is_scip) = match travsr_indexer::scip_unifier::scip_name_kind(scip_sym) {
            Some(p) => (p, true),
            // Scala's sidecar reads SemanticDB, whose symbols are a bare
            // descriptor chain with none of SCIP's `<scheme> <mgr> <pkg>
            // <version>` prefix, so `scip_name_kind` rejects all of them. Parsed
            // as SCIP-shaped rather than Phase-A-shaped, because the descriptor
            // grammar is SCIP's: `…/Parsers#phrase().`.
            None if node.vname.language.as_str() == "scala" => {
                match travsr_indexer::scip_unifier::semanticdb_name_kind(&node.vname.signature) {
                    Some(p) => (p, true),
                    None => continue,
                }
            }
            None if matches!(node.vname.language.as_str(), "kotlin" | "swift" | "dart") => {
                // KLS gives properties and locals no kind (`sym:`); parse them as
                // terms so a property meets its Phase A field.
                let kind = match (node.vname.language.as_str(), node.kind.as_str()) {
                    ("kotlin", "symbol") => "variable",
                    (_, kind) => kind,
                };
                match travsr_indexer::scip_unifier::native_name_kind(&node.vname.signature, kind) {
                    Some(p) => (p, false),
                    None => continue,
                }
            }
            None => continue,
        };
        // #780: Sorbet models RSpec `describe`/`context`/`it` blocks as singleton
        // scopes and emits SCIP defs for them. When the *leaf* itself is the
        // block (`<it 'does x'>` as the def's own name) tree-sitter (correctly)
        // sees a method call with a block, not a definition, so no Phase A twin
        // exists or can — in ANY file, so this is checked before the
        // unindexed-path rule below. Drop it so it stops orphaning as a duplicate
        // that steals spec-file reference edges (~70% of #780's headline rate).
        // (The *container*-only block case — a real `class Helper` / `def helper`
        // defined inside `describe 'Foo' do … end` — is handled after the path
        // check: Phase A emits an unqualified twin, so it can reconcile.)
        if travsr_indexer::scip_unifier::is_dsl_scope_leaf(&parsed) {
            dropped.insert(node.id);
            continue;
        }
        // An anonymous C/C++ type has no name, so Phase A can give it no node.
        if is_clang(&node.vname.language)
            && parsed.kind == "class"
            && parsed.name.starts_with(ANONYMOUS_TYPE)
        {
            dropped.insert(node.id);
            continue;
        }
        // #780: a SCIP def in a file the tree-sitter parser never indexed
        // (gitignored vendored code that scip-ruby indexes anyway) has no twin
        // and none can exist — it is not a reconciliation failure. Exclude it
        // from the counters but KEEP the node: it is a real definition at a real
        // on-disk file (`vendor/bundle/.../rake/task.rb`), and calls from app
        // code into gem code are exactly the cross-boundary edges the graph is
        // most useful for. Dropping it would take every inbound `ScipRef` with
        // it. Native sidecar nodes (kotlin/swift/dart) are never path-excluded:
        // their twins live in the same indexed sources.
        if is_scip && !indexed_paths.contains(&node.vname.path) {
            continue;
        }
        // Container-only DSL block: Phase A DOES emit a twin, unqualified,
        // because a block is not a `method_container`. Clearing the
        // unreconcilable container lets the def unify onto that twin. It is
        // counted only if it unifies (below), so recovering it can never regress
        // the reported miss rate.
        let dsl_contained = travsr_indexer::scip_unifier::is_dsl_scope_container(&parsed);
        let parsed = if dsl_contained {
            travsr_indexer::scip_unifier::ScipName {
                container: None,
                ..parsed
            }
        } else {
            parsed
        };
        // scip-php names a property with its `$` sigil; Phase A does not.
        let parsed = match parsed.name.strip_prefix('$') {
            Some(name) if node.vname.language == "php" => {
                travsr_indexer::scip_unifier::ScipName { name, ..parsed }
            }
            _ => parsed,
        };
        // No definition line means line-proximity matching is meaningless —
        // unwrapping to 0 would let any same-named node on lines 1..=5 of the
        // file match wrongly. Skip instead.
        let Some(line) = node.line else {
            // SemanticDB defines a `var`'s `_=` setter with no occurrence, so it
            // has no line. Its field's signature is unique in its file, which
            // makes the exact (path, signature) match safe without one.
            if let (true, Some(c), Some(field)) = (
                node.vname.language == "scala",
                parsed.container,
                parsed.name.strip_suffix("_="),
            ) {
                let sig = format!("field:{c}.{field}");
                let path = node.vname.path.as_str();
                if let Some(ts) = store
                    .lookup_nodes_exact(&sig, Some(path))
                    .unwrap_or_default()
                    .into_iter()
                    .find(|n| n.vname.path == path && n.vname.corpus == corpus)
                {
                    aliases.push((scip_sym.to_string(), ts.id));
                    alias_map.insert(node.id, ts.id);
                }
            }
            continue;
        };
        let mut candidates = travsr_indexer::scip_unifier::candidate_signatures(&parsed);
        // A KLS `sym:` node is only ever a class property here; its bare
        // `var:`/`const:` forms would let a local meet a nearby top-level one.
        if node.vname.language == "kotlin" && node.kind == "symbol" {
            candidates.retain(|c| c.starts_with("field:"));
        }
        // The signatures below match only by span in the definition's own file
        // (rung 1). They are not package-qualified, so the cross-file rungs
        // never see them.
        let mut same_file = candidates.clone();
        // SemanticDB models a Scala `var`/`val` read and write as a getter and
        // `_=` setter on the field's own line, where Phase A wrote the field.
        // Unified there, their references are `ref/field`, not calls.
        if node.vname.language == "scala" && parsed.kind == "function" {
            if let Some(c) = parsed.container {
                let field = parsed.name.strip_suffix("_=").unwrap_or(parsed.name);
                same_file.push(format!("field:{c}.{field}"));
            }
        }
        // scip-go writes an interface method as a term (`Animal#Name.`), where
        // Phase A wrote the method spec.
        if node.vname.language == "go" && parsed.kind == "variable" {
            if let Some(c) = parsed.container {
                same_file.push(format!("method:{c}.{}", parsed.name));
            }
        }
        if is_clang(&node.vname.language)
            && parsed.kind == "variable"
            && parsed
                .container
                .is_some_and(|c| c.starts_with(ANONYMOUS_TYPE))
        {
            for t in clang_types
                .get(node.vname.path.as_str())
                .into_iter()
                .flatten()
            {
                same_file.push(format!("field:{t}.{}", parsed.name));
            }
        }
        // A Scala `object` is a term (`Main.`); Phase A wrote it as the type
        // node `class:Main` on the same line.
        if node.vname.language == "scala" && parsed.kind == "variable" && parsed.container.is_none()
        {
            same_file.push(format!("class:{}", parsed.name));
        }
        // A constructor with no declaration of its own (a Kotlin primary
        // constructor, an implicit JVM `<init>`, which Scala's sidecar kinds
        // `sym`) is defined on its class's line, and Phase A wrote only the
        // class it constructs: `new Zoo()` constructs the class, as `Zoo()`
        // does in Python. An explicit constructor still wins as the narrower
        // span containing its line.
        if node.kind == "constructor" || parsed.name == "<init>" {
            if let Some(c) = parsed.container {
                same_file.push(format!("class:{c}"));
            }
        }
        let scip_line = line as i64;
        let is_callable_type = matches!(parsed.kind, "function" | "class");
        // A normal callable/type def is an attempt up front — a miss raises the
        // rate. A DSL-contained def is credited only when it unifies (in the
        // match arm), so its recovery cannot push the miss rate up.
        if is_callable_type && !dsl_contained {
            attempted_syms.insert(scip_sym);
            miss_detail.entry(scip_sym).or_insert_with(|| UnifyMiss {
                language: node.vname.language.clone(),
                // Readable `Container.name` (or bare `name`) rather than the raw
                // SCIP moniker — enough for a dev to spot the construct/language.
                symbol: match parsed.container {
                    Some(c) => format!("{c}.{}", parsed.name),
                    None => parsed.name.to_string(),
                },
                path: node.vname.path.clone(),
                line,
                kind: parsed.kind.to_string(),
            });
        }

        match store.find_ts_node_for_unification(
            corpus,
            &node.vname.path,
            &same_file,
            scip_line,
            MAX_LINE_DELTA,
        ) {
            Ok(Some(ts_id)) => {
                aliases.push((scip_sym.to_string(), ts_id));
                alias_map.insert(node.id, ts_id);
                sym_to_ts.entry(scip_sym).or_insert(ts_id);
                if is_callable_type {
                    // A DSL-contained def enters `attempted` only now, on the
                    // success path, so the pair stays metric-neutral.
                    attempted_syms.insert(scip_sym);
                    unified_syms.insert(scip_sym);
                }
                tracing::trace!(symbol = %scip_sym, ?ts_id, "G1: unified");
            }
            // A DSL-contained def that still misses after the container clear has
            // no reconcilable twin — drop it un-counted rather than counting a
            // synthetic miss or letting the cross-file rungs key on its
            // block-qualified symbol.
            Ok(None) if dsl_contained => {
                dropped.insert(node.id);
            }
            Ok(None) => unmatched.push((
                node.id,
                scip_sym,
                candidates,
                parsed.kind,
                node.vname.path.as_str(),
                travsr_indexer::scip_unifier::overload_collapse_signatures(&parsed),
                node.vname.language.as_str(),
            )),
            Err(e) => tracing::warn!(symbol = %scip_sym, "G1: DB lookup: {e:#}"),
        }
    }

    // Cross-file duplicate collapse: an unmatched def whose symbol unified in a
    // different file is the same definition (Obj-C interface/implementation,
    // C/C++ header/source). Alias it onto that node so it is dropped as a
    // duplicate and its edges/refs rewrite onto the real node, and credit its
    // symbol as unified so the miss-rate does not penalize the benign twin.
    for (node_id, sym, candidates, kind, path, overload_sigs, language) in unmatched {
        // Rung 1: the symbol unified in another file, so this occurrence is
        // the benign twin.
        if let Some(&ts_id) = sym_to_ts.get(sym) {
            aliases.push((sym.to_string(), ts_id));
            alias_map.insert(node_id, ts_id);
            // Only callable/type symbols feed the miss-rate; crediting a
            // `variable` twin would let `unified` exceed `attempted`.
            if attempted_syms.contains(sym) {
                unified_syms.insert(sym);
            }
            continue;
        }

        // Rung 1b: the SCIP def is one overload of a method group whose Phase A
        // node is shared by every overload. Phase A signatures carry no
        // parameter list, so `C#n().`, `C#n(+1).`, ... all belong to the single
        // `method:C.n` node anchored at the first overload's span, and every
        // later overload misses both span-containment and the +/-5 window above.
        // Resolving that by same-file uniqueness on the *container-qualified*
        // signature is exact, not heuristic: one match means one method group.
        //
        // Measured on the C# fixture (commandlineparser): constructors are the
        // dominant case, 113 of the 120 unreconciled callable defs. The first
        // overload of `OptionAttribute` carried 0 ref/call edges while overloads
        // +1..+4 carried all 103, so the candidate fix alone recovered nothing;
        // this rung is what makes those edges reachable from the type.
        //
        // Placed before rung 2: a same-file qualified match is stronger evidence
        // than rung 2's corpus-wide uniqueness, and it is checked first so the
        // wider rung never gets to answer a question this one already can.
        if !overload_sigs.is_empty() {
            match store.find_unique_ts_node_in_file(corpus, path, &overload_sigs) {
                Ok(Some(ts_id)) => {
                    aliases.push((sym.to_string(), ts_id));
                    alias_map.insert(node_id, ts_id);
                    sym_to_ts.entry(sym).or_insert(ts_id);
                    if attempted_syms.contains(sym) {
                        unified_syms.insert(sym);
                    }
                    tracing::trace!(symbol = %sym, ?ts_id, "G1: unified onto overload group");
                    continue;
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(symbol = %sym, "G1: overload lookup: {e:#}"),
            }
        }

        // Rung 2: the declaration is the *only* Phase A node and it lives in
        // another file, so rung 1 has nothing to key on. This is the C/C++
        // out-of-line member definition: `widget.h` declares `Widget::draw`,
        // `widget.cpp` defines it, and Phase A anchored the header. Without
        // this the SCIP definition survives as an orphan and takes every
        // ref/call edge with it, so `travsr references draw` answers zero
        // while the edges exist and point somewhere unreachable.
        //
        // Guarded on uniqueness rather than on position: a declaration's line
        // says nothing about its definition's, and more than one candidate
        // means the name is ambiguous in this repo (two same-named `static`
        // functions in different translation units are different functions).
        //
        // Restricted to callables and types (#708 review). `candidate_signatures`
        // qualifies those by container where it can (`method:Widget.draw`), but
        // a `variable` yields only bare `var:name` / `const:name` /
        // `static:name`. Names like `count`, `size`, `buf` are common enough
        // that an unrelated `static int count;` elsewhere in the repo is often
        // the single other match, and uniqueness cannot tell "the only match"
        // from "the right match": it would alias the definition onto an
        // unrelated node and corrupt its ref/call edges with no error at all.
        if !matches!(kind, "function" | "class") {
            continue;
        }

        // A same-file exclusion was tried here and removed: the #708 review
        // suggested this rung should not overturn rung 1's positional
        // rejection, but rung 1 rejects the *legitimate* same-file case too.
        // `Box<T>::unwrap` is declared inside the class and defined out of
        // line six lines later, past the +/-5 window, and excluding it
        // reintroduced the orphan this rung exists to prevent. For a callable
        // whose name is unique in the whole corpus, distance is not evidence
        // of a different symbol; it is what an out-of-line definition looks
        // like. The kind restriction above is what addresses the risk the
        // review actually described.
        match store.find_unique_ts_node_across_files(
            corpus,
            // A header is shared across the C family and tagged by content,
            // so a `.cpp` definition's declaration may sit in a `c` header.
            match language {
                "c" | "cpp" | "objectivec" => &["c", "cpp", "objectivec"],
                _ => std::slice::from_ref(&language),
            },
            &candidates,
        ) {
            Ok(Some(ts_id)) => {
                aliases.push((sym.to_string(), ts_id));
                alias_map.insert(node_id, ts_id);
                sym_to_ts.entry(sym).or_insert(ts_id);
                if attempted_syms.contains(sym) {
                    unified_syms.insert(sym);
                }
                tracing::trace!(symbol = %sym, ?ts_id, "G1: unified across files");
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(symbol = %sym, "G1: cross-file lookup: {e:#}"),
        }
    }

    // A Kotlin `sym:` node that met no Phase A term and sits inside a function
    // (its container is a callable whose span encloses it) is a local variable:
    // intra-function noise with no twin, dropped like a SCIP `local N`.
    for node in nodes {
        if node.vname.language != "kotlin"
            || node.kind != "symbol"
            || alias_map.contains_key(&node.id)
        {
            continue;
        }
        let (Some(line), Some(parsed)) = (
            node.line,
            travsr_indexer::scip_unifier::native_name_kind(&node.vname.signature, "variable"),
        ) else {
            continue;
        };
        let Some(container) = parsed.container else {
            continue;
        };
        let callable = format!("fn:{container}");
        let Some(owner) = travsr_indexer::scip_unifier::native_name_kind(&callable, "function")
        else {
            continue;
        };
        let owners = travsr_indexer::scip_unifier::candidate_signatures(&owner);
        if let Ok(Some(_)) =
            store.find_ts_node_for_unification(corpus, &node.vname.path, &owners, line as i64, 0)
        {
            dropped.insert(node.id);
        }
    }

    let attempted = attempted_syms.len();
    let unified = unified_syms.len();
    // #825: the residual misses are exactly the attempted symbols that never
    // unified. Emit one detail row each (sorted by path:line for a stable list)
    // so `travsr status` can name them rather than only counting them.
    let mut misses: Vec<UnifyMiss> = attempted_syms
        .difference(&unified_syms)
        .filter_map(|s| miss_detail.get(s).cloned())
        .collect();
    misses.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));

    if let Err(e) = store.register_symbol_aliases(&aliases) {
        tracing::warn!("G1: register_symbol_aliases batch: {e:#}");
    }

    for r in refs.iter_mut() {
        if let Some(&ts_id) = alias_map.get(&r.callee_id) {
            r.callee_id = ts_id;
        }
    }

    // #780: report the excluded-outright count alongside the attempt/miss
    // figures so a mass silent exclusion (e.g. Phase A having indexed nothing,
    // so every DSL-contained def fails to unify and is dropped) is
    // distinguishable from a genuinely low miss rate on the same debug line.
    tracing::debug!(
        aliased = alias_map.len(),
        callable_unified = unified,
        callable_attempted = attempted,
        dropped = dropped.len(),
        total = nodes.len(),
        "G1: unification complete"
    );
    UnifyOutcome {
        unified,
        attempted,
        alias_map,
        dropped,
        misses,
    }
}

/// scip-clang's name for an unnamed struct, union or enum.
const ANONYMOUS_TYPE: &str = "$anonymous_type_";

/// The languages scip-clang indexes.
fn is_clang(language: &str) -> bool {
    matches!(language, "c" | "cpp" | "objectivec")
}

#[cfg(test)]
mod tests {
    use super::*;
    use travsr_core::{Node, VName};

    fn scip_node(path: &str, symbol: &str, line: u32) -> Node {
        // scip-reader packs def signatures as `scip:{rel_path}:{symbol}`.
        let sig = format!("scip:{path}:{symbol}");
        Node::new(VName::new("c", "main", path, "ruby", &sig), "definition").with_line(line)
    }

    #[test]
    fn dsl_scopes_excluded_and_dropped_real_method_counted() {
        // #780: a real `Class#method().` def unifies onto its Phase A twin and
        // counts toward the miss rate; a Sorbet RSpec DSL block def is neither
        // counted nor written — it is dropped so it cannot steal ref edges.
        let mut store = SqliteStore::open_in_memory().unwrap();
        // Phase A twin for the real accessor (as emitted by the Ruby attr_* /
        // method captures), same path + line as the SCIP def.
        let ts = Node::new(
            VName::new(
                "c",
                "main",
                "supply/lib/supply/generated_universal_apk.rb",
                "ruby",
                "method:GeneratedUniversalApk.package_name",
            ),
            "method",
        )
        .with_line(4)
        .with_end_line(4);
        store
            .write_phase_b_batch(std::slice::from_ref(&ts), &[], "scip")
            .unwrap();

        let real = scip_node(
            "supply/lib/supply/generated_universal_apk.rb",
            "scip-ruby gem fastlane 0.0.0 Supply#GeneratedUniversalApk#package_name().",
            4,
        );
        let dsl_method = scip_node(
            "fastlane/spec/actions_specs/carthage_spec.rb",
            "scip-ruby gem fastlane 0.0.0 `<describe 'Fastlane'>`#`<it 'sets the platform to iOS'>`().",
            7,
        );
        let dsl_type = scip_node(
            "match/spec/setup_spec.rb",
            "scip-ruby gem fastlane 0.0.0 `<describe 'Match'>`#`<describe 'Setup'>`#",
            1,
        );

        let nodes = vec![real.clone(), dsl_method.clone(), dsl_type.clone()];
        let mut refs: Vec<ScipRef> = Vec::new();
        let out = unify_all(&mut store, "c", &nodes, &mut refs);

        assert_eq!(out.attempted, 1, "only the real Class#method is an attempt");
        assert_eq!(out.unified, 1, "the real method unifies onto its twin");
        assert_eq!(out.alias_map.get(&real.id), Some(&ts.id));
        assert!(out.dropped.contains(&dsl_method.id), "DSL block dropped");
        assert!(out.dropped.contains(&dsl_type.id), "DSL type block dropped");
        assert!(
            !out.dropped.contains(&real.id),
            "the real method must not be dropped"
        );
    }

    #[test]
    fn a_real_miss_is_named_in_the_outcome() {
        // #825: an indexed-file callable def with no Phase A twin is a real miss.
        // It must appear in `out.misses` with its language, kind, readable
        // `Container.name`, and path:line, so `travsr status` can name it instead
        // of only counting it.
        let mut store = SqliteStore::open_in_memory().unwrap();
        // A Phase A twin at lib/app.rb makes that path "indexed"; the orphan def
        // lives in the same file but has no twin of its own.
        let ts = Node::new(
            VName::new("c", "main", "lib/app.rb", "ruby", "method:App.run"),
            "method",
        )
        .with_line(2)
        .with_end_line(2);
        store
            .write_phase_b_batch(std::slice::from_ref(&ts), &[], "scip")
            .unwrap();

        let good = scip_node("lib/app.rb", "scip-ruby gem g 0.0.0 App#run().", 2);
        let orphan = scip_node("lib/app.rb", "scip-ruby gem g 0.0.0 App#missing().", 99);

        let nodes = vec![good.clone(), orphan.clone()];
        let mut refs: Vec<ScipRef> = Vec::new();
        let out = unify_all(&mut store, "c", &nodes, &mut refs);

        assert_eq!(out.attempted, 2, "both callable defs are attempts");
        assert_eq!(out.unified, 1, "only App#run unifies onto its twin");
        assert_eq!(out.misses.len(), 1, "exactly the orphan is a named miss");
        let m = &out.misses[0];
        assert_eq!(m.language, "ruby");
        assert_eq!(m.kind, "function");
        assert_eq!(m.symbol, "App.missing");
        assert_eq!(m.path, "lib/app.rb");
        assert_eq!(m.line, 99);
    }

    #[test]
    fn java_constructor_unifies_onto_its_phase_a_constructor() {
        // scip-java names a constructor `Dog#`<init>`().`; Phase A wrote
        // `method:Dog.Dog`. Unmatched, `new Dog(...)` reached no node.
        let mut store = SqliteStore::open_in_memory().unwrap();
        let ctor = Node::new(
            VName::new("c", "", "java/src/Dog.java", "java", "method:Dog.Dog"),
            "constructor",
        )
        .with_line(2)
        .with_end_line(4);
        store
            .write_phase_b_batch(std::slice::from_ref(&ctor), &[], "scip")
            .unwrap();
        let sig = "scip:java/src/Dog.java:scip-java maven . . Dog#`<init>`().";
        let scip = Node::new(
            VName::new("c", "", "java/src/Dog.java", "java", sig),
            "constructor",
        )
        .with_line(2)
        .with_end_line(4);
        // An implicit constructor has no Phase A node; scip-java defines it on
        // the class line, and the class is what `new Zoo()` constructs.
        let class = Node::new(
            VName::new("c", "", "java/src/Zoo.java", "java", "class:Zoo"),
            "class",
        )
        .with_line(4)
        .with_end_line(16);
        store
            .write_phase_b_batch(std::slice::from_ref(&class), &[], "scip")
            .unwrap();
        let implicit = Node::new(
            VName::new(
                "c",
                "",
                "java/src/Zoo.java",
                "java",
                "scip:java/src/Zoo.java:scip-java maven . . Zoo#`<init>`().",
            ),
            "constructor",
        )
        .with_line(4)
        .with_end_line(4);
        let mut refs: Vec<ScipRef> = Vec::new();
        let out = unify_all(
            &mut store,
            "c",
            &[scip.clone(), implicit.clone()],
            &mut refs,
        );
        assert_eq!(out.alias_map.get(&scip.id), Some(&ctor.id));
        assert_eq!(out.alias_map.get(&implicit.id), Some(&class.id));
        assert!(out.misses.is_empty(), "{:?}", out.misses);
    }

    #[test]
    fn kotlin_primary_constructor_unifies_onto_its_class() {
        // A primary constructor lives in the class header; Phase A Kotlin has
        // no constructor capture, only the class it constructs.
        let mut store = SqliteStore::open_in_memory().unwrap();
        let class = Node::new(
            VName::new("c", "", "k/Animal.kt", "kotlin", "class:Animal"),
            "class",
        )
        .with_line(1)
        .with_end_line(6);
        store
            .write_phase_b_batch(std::slice::from_ref(&class), &[], "scip")
            .unwrap();
        let ctor = Node::new(
            VName::new("c", "", "k/Animal.kt", "kotlin", "method:Animal.Animal"),
            "constructor",
        )
        .with_line(1)
        .with_end_line(1);
        let mut refs: Vec<ScipRef> = Vec::new();
        let out = unify_all(&mut store, "c", std::slice::from_ref(&ctor), &mut refs);
        assert_eq!(out.alias_map.get(&ctor.id), Some(&class.id));
        assert!(out.misses.is_empty(), "{:?}", out.misses);
    }

    #[test]
    fn kotlin_property_unifies_and_local_is_dropped() {
        // KLS reports properties and locals alike as untyped `sym:` symbols.
        // A property has its Phase A field; a local has no twin and is noise,
        // like a SCIP `local N`.
        let mut store = SqliteStore::open_in_memory().unwrap();
        let field = Node::new(
            VName::new("c", "", "k/Animal.kt", "kotlin", "field:Zoo.animals"),
            "field",
        )
        .with_line(8)
        .with_end_line(8);
        store
            .write_phase_b_batch(std::slice::from_ref(&field), &[], "scip")
            .unwrap();
        let property = Node::new(
            VName::new("c", "", "k/Animal.kt", "kotlin", "sym:Zoo.animals"),
            "symbol",
        )
        .with_line(8)
        .with_end_line(8);
        let local = Node::new(
            VName::new("c", "", "k/Main.kt", "kotlin", "sym:main.dog"),
            "symbol",
        )
        .with_line(3)
        .with_end_line(3);
        let main = Node::new(
            VName::new("c", "", "k/Main.kt", "kotlin", "fn:main"),
            "function",
        )
        .with_line(1)
        .with_end_line(12);
        store
            .write_phase_b_batch(std::slice::from_ref(&main), &[], "scip")
            .unwrap();
        // Untyped too, but not inside a function: kept.
        let entry = Node::new(
            VName::new("c", "", "k/Color.kt", "kotlin", "sym:Color.RED"),
            "symbol",
        )
        .with_line(2)
        .with_end_line(2);
        let mut refs: Vec<ScipRef> = Vec::new();
        let nodes = [property.clone(), local.clone(), entry.clone()];
        let out = unify_all(&mut store, "c", &nodes, &mut refs);
        assert_eq!(out.alias_map.get(&property.id), Some(&field.id));
        assert!(out.dropped.contains(&local.id));
        assert!(!out.dropped.contains(&entry.id));
        assert!(out.misses.is_empty(), "{:?}", out.misses);
    }

    #[test]
    fn a_go_package_def_is_dropped_from_every_file() {
        // scip-go defines the package (`…/pkg/`) in every file of it. Phase A
        // models a package once per directory (`go-pkg:`), so the per-file def
        // has no twin; kept, it orphaned every file and made each save a
        // whole-file purge.
        let mut store = SqliteStore::open_in_memory().unwrap();
        let go = |path: &str, symbol: &str, line: u32| {
            Node::new(
                VName::new("c", "", path, "go", &format!("scip:{path}:{symbol}")),
                "module",
            )
            .with_line(line)
        };
        let short = go("go/main.go", "scip-go gomod zoo . zoo/", 1);
        let long = go(
            "go/dog.go",
            "scip-go gomod github.com/o/zoo . github.com/o/zoo/",
            1,
        );
        let method = go("go/dog.go", "scip-go gomod zoo . zoo/Dog#Fetch().", 5);
        let mut refs: Vec<ScipRef> = Vec::new();
        let out = unify_all(
            &mut store,
            "c",
            &[short.clone(), long.clone(), method.clone()],
            &mut refs,
        );
        assert!(out.dropped.contains(&short.id));
        assert!(out.dropped.contains(&long.id));
        assert!(!out.dropped.contains(&method.id));
    }

    #[test]
    fn a_clang_file_namespace_def_is_dropped_from_every_file() {
        // scip-clang defines a per-file namespace (`<file>/src/main.c`/) in
        // every C-family file. Phase A has no twin for it, so it orphaned every
        // file and made each save a whole-file purge. A real namespace stays.
        let mut store = SqliteStore::open_in_memory().unwrap();
        let clang = |path: &str, lang: &str, symbol: &str| {
            Node::new(
                VName::new("c", "", path, lang, format!("scip:{path}:{symbol}")),
                "definition",
            )
            .with_line(1)
        };
        let c = clang("c/src/main.c", "c", "cxx . . $ `<file>/src/main.c`/");
        let cpp = clang("cpp/src/dog.h", "cpp", "cxx . . $ `<file>/src/dog.h`/");
        let ns = clang("cpp/src/zoo.h", "cpp", "cxx . . $ zoo/");
        let mut refs: Vec<ScipRef> = Vec::new();
        let out = unify_all(
            &mut store,
            "c",
            &[c.clone(), cpp.clone(), ns.clone()],
            &mut refs,
        );
        assert!(out.dropped.contains(&c.id));
        assert!(out.dropped.contains(&cpp.id));
        assert!(!out.dropped.contains(&ns.id));
    }

    #[test]
    fn an_anonymous_typedef_struct_unifies_its_fields_onto_the_typedef() {
        // `typedef struct { int age; } Animal;`: scip-clang defines the struct
        // as `$anonymous_type_<hash>_0#` and its fields under it, while Phase A
        // names the fields by the typedef (`field:Animal.age`). The anonymous
        // type itself has no name Phase A could give a node, so it is dropped.
        let mut store = SqliteStore::open_in_memory().unwrap();
        let path = "c/src/utils.h";
        let phase_a = |sig: &str, kind: &str, line: u32| {
            Node::new(VName::new("c", "", path, "c", sig), kind)
                .with_line(line)
                .with_end_line(line)
        };
        let ty = phase_a("type:Animal", "typedef", 6);
        let age = phase_a("field:Animal.age", "field", 5);
        store
            .write_phase_b_batch(&[ty.clone(), age.clone()], &[], "tree-sitter")
            .unwrap();
        let scip = |symbol: &str, line: u32| {
            Node::new(
                VName::new(
                    "c",
                    "",
                    path,
                    "c",
                    format!("scip:src/utils.h:cxx . . $ {symbol}"),
                ),
                "definition",
            )
            .with_line(line)
        };
        let anon = scip("$anonymous_type_ff1e22b05cf4cd3e_0#", 3);
        let anon_age = scip("$anonymous_type_ff1e22b05cf4cd3e_0#age.", 5);
        let typedef = scip("Animal#", 6);
        let mut refs: Vec<ScipRef> = Vec::new();
        let out = unify_all(
            &mut store,
            "c",
            &[anon.clone(), anon_age.clone(), typedef.clone()],
            &mut refs,
        );
        assert!(out.dropped.contains(&anon.id));
        assert_eq!(out.alias_map.get(&anon_age.id), Some(&age.id));
        assert_eq!(out.alias_map.get(&typedef.id), Some(&ty.id));
        assert!(out.misses.is_empty(), "{:?}", out.misses);
    }

    #[test]
    fn a_go_interface_method_term_unifies_onto_its_method_spec() {
        // scip-go writes an interface method as a term (`Animal#Name.`), which
        // parses as a field; Phase A models the spec as `method:Animal.Name`.
        let mut store = SqliteStore::open_in_memory().unwrap();
        let spec = Node::new(
            VName::new("c", "", "go/animal.go", "go", "method:Animal.Name"),
            "method",
        )
        .with_line(4)
        .with_end_line(4);
        store
            .write_phase_b_batch(std::slice::from_ref(&spec), &[], "scip")
            .unwrap();
        let term = Node::new(
            VName::new(
                "c",
                "",
                "go/animal.go",
                "go",
                "scip:go/animal.go:scip-go gomod zoo . zoo/Animal#Name.",
            ),
            "function",
        )
        .with_line(4);
        let mut refs: Vec<ScipRef> = Vec::new();
        let out = unify_all(&mut store, "c", std::slice::from_ref(&term), &mut refs);
        assert_eq!(out.alias_map.get(&term.id), Some(&spec.id));
    }

    #[test]
    fn a_php_property_unifies_without_its_sigil() {
        // scip-php names a property with its `$` (`Animal#$name.`); Phase A
        // writes `field:Animal.name`.
        let mut store = SqliteStore::open_in_memory().unwrap();
        let field = Node::new(
            VName::new("c", "", "php/src/Animal.php", "php", "field:Animal.name"),
            "field",
        )
        .with_line(4)
        .with_end_line(4);
        store
            .write_phase_b_batch(std::slice::from_ref(&field), &[], "scip")
            .unwrap();
        let property = Node::new(
            VName::new(
                "c",
                "",
                "php/src/Animal.php",
                "php",
                "scip:src/Animal.php:scip-php composer t/p 0 Animal#$name.",
            ),
            "definition",
        )
        .with_line(4);
        let mut refs: Vec<ScipRef> = Vec::new();
        let out = unify_all(&mut store, "c", std::slice::from_ref(&property), &mut refs);
        assert_eq!(out.alias_map.get(&property.id), Some(&field.id));
    }

    #[test]
    fn scala_object_and_its_members_unify() {
        // `object Main { def main(..) }`: SemanticDB owns the member by a term
        // (`Main.main().`), not a type (`Main#`), so it parsed with no container
        // and never met `method:Main.main`; the object itself (`Main.`) never met
        // Phase A's `class:Main`. Both survived as orphans in the file.
        let mut store = SqliteStore::open_in_memory().unwrap();
        let object = Node::new(
            VName::new("c", "", "s/Main.scala", "scala", "class:Main"),
            "object",
        )
        .with_line(1)
        .with_end_line(15);
        let main = Node::new(
            VName::new("c", "", "s/Main.scala", "scala", "method:Main.main"),
            "method",
        )
        .with_line(2)
        .with_end_line(14);
        store
            .write_phase_b_batch(&[object.clone(), main.clone()], &[], "scip")
            .unwrap();
        let sdb = |sig: &str, kind: &str, line: u32| {
            Node::new(VName::new("c", "", "s/Main.scala", "scala", sig), kind)
                .with_line(line)
                .with_end_line(line)
        };
        let sdb_object = sdb("sdb:_empty_/Main.", "object", 1);
        let sdb_main = sdb("sdb:_empty_/Main.main().", "method", 2);
        let mut refs: Vec<ScipRef> = Vec::new();
        let out = unify_all(
            &mut store,
            "c",
            &[sdb_object.clone(), sdb_main.clone()],
            &mut refs,
        );
        assert_eq!(out.alias_map.get(&sdb_object.id), Some(&object.id));
        assert_eq!(out.alias_map.get(&sdb_main.id), Some(&main.id));
        assert!(out.misses.is_empty(), "{:?}", out.misses);
    }

    #[test]
    fn scala_var_accessors_unify_onto_the_field() {
        // SemanticDB models a `var` read and write as getter/setter methods on
        // the field's own line; Phase A wrote the field. Unmatched, every
        // read and write was a `ref/call` to an orphan accessor.
        let mut store = SqliteStore::open_in_memory().unwrap();
        let field = Node::new(
            VName::new("c", "", "s/A.scala", "scala", "field:Zoo.animals"),
            "field",
        )
        .with_line(8)
        .with_end_line(8);
        store
            .write_phase_b_batch(std::slice::from_ref(&field), &[], "scip")
            .unwrap();
        let sdb = |sig: &str| {
            Node::new(VName::new("c", "", "s/A.scala", "scala", sig), "method")
                .with_line(8)
                .with_end_line(8)
        };
        let getter = sdb("sdb:_empty_/Zoo#animals().");
        // The setter has no definition occurrence, so no line.
        let setter = Node::new(
            VName::new(
                "c",
                "",
                "s/A.scala",
                "scala",
                "sdb:_empty_/Zoo#`animals_=`().",
            ),
            "method",
        );
        let mut refs: Vec<ScipRef> = Vec::new();
        let out = unify_all(
            &mut store,
            "c",
            &[getter.clone(), setter.clone()],
            &mut refs,
        );
        assert_eq!(out.alias_map.get(&getter.id), Some(&field.id));
        assert_eq!(out.alias_map.get(&setter.id), Some(&field.id));
    }

    #[test]
    fn constructor_class_fallback_stays_in_its_own_file() {
        // The class fallback is a same-file span match. Across files a bare
        // `class:Zoo` is not package-qualified, so the unique one elsewhere may
        // be another package's class.
        let mut store = SqliteStore::open_in_memory().unwrap();
        let elsewhere = Node::new(
            VName::new("c", "", "other/Zoo.java", "java", "class:Zoo"),
            "class",
        )
        .with_line(1)
        .with_end_line(9);
        let here = Node::new(
            VName::new("c", "", "java/src/Zoo.java", "java", "method:Zoo.add"),
            "method",
        )
        .with_line(30)
        .with_end_line(32);
        store
            .write_phase_b_batch(&[elsewhere, here], &[], "scip")
            .unwrap();
        let ctor = Node::new(
            VName::new(
                "c",
                "",
                "java/src/Zoo.java",
                "java",
                "scip:java/src/Zoo.java:scip-java maven . . Zoo#`<init>`().",
            ),
            "constructor",
        )
        .with_line(4)
        .with_end_line(4);
        let mut refs: Vec<ScipRef> = Vec::new();
        let out = unify_all(&mut store, "c", std::slice::from_ref(&ctor), &mut refs);
        assert_eq!(out.alias_map.get(&ctor.id), None);
    }

    #[test]
    fn kotlin_companion_property_meets_its_field() {
        // KLS nests the container (`Zoo.Companion`); Phase A qualifies by the
        // nearest named type (`field:Zoo.MAX`).
        let mut store = SqliteStore::open_in_memory().unwrap();
        let field = Node::new(
            VName::new("c", "", "k/Zoo.kt", "kotlin", "field:Zoo.MAX"),
            "field",
        )
        .with_line(3)
        .with_end_line(3);
        store
            .write_phase_b_batch(std::slice::from_ref(&field), &[], "scip")
            .unwrap();
        let prop = Node::new(
            VName::new("c", "", "k/Zoo.kt", "kotlin", "sym:Zoo.Companion.MAX"),
            "symbol",
        )
        .with_line(3)
        .with_end_line(3);
        let mut refs: Vec<ScipRef> = Vec::new();
        let out = unify_all(&mut store, "c", std::slice::from_ref(&prop), &mut refs);
        assert_eq!(out.alias_map.get(&prop.id), Some(&field.id));
    }

    #[test]
    fn kotlin_local_never_meets_a_nearby_top_level_property() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let top = Node::new(
            VName::new("c", "", "k/Main.kt", "kotlin", "var:logger"),
            "variable",
        )
        .with_line(5)
        .with_end_line(5);
        let main = Node::new(
            VName::new("c", "", "k/Main.kt", "kotlin", "fn:main"),
            "function",
        )
        .with_line(7)
        .with_end_line(12);
        store
            .write_phase_b_batch(&[top, main], &[], "scip")
            .unwrap();
        let local = Node::new(
            VName::new("c", "", "k/Main.kt", "kotlin", "sym:main.logger"),
            "symbol",
        )
        .with_line(8)
        .with_end_line(8);
        let mut refs: Vec<ScipRef> = Vec::new();
        let out = unify_all(&mut store, "c", std::slice::from_ref(&local), &mut refs);
        assert_eq!(out.alias_map.get(&local.id), None);
        assert!(out.dropped.contains(&local.id));
    }

    #[test]
    fn cross_file_unification_spans_the_c_family_header() {
        // A C-compatible header is tagged `c` even when a `.cpp` file defines
        // what it declares; the out-of-line definition must still meet it.
        let mut store = SqliteStore::open_in_memory().unwrap();
        let decl = Node::new(
            VName::new("c", "", "src/widget.h", "c", "method:Widget.draw"),
            "method",
        )
        .with_line(3)
        .with_end_line(3);
        let anchor = Node::new(
            VName::new("c", "", "src/widget.cpp", "cpp", "fn:main"),
            "function",
        )
        .with_line(40)
        .with_end_line(42);
        store
            .write_phase_b_batch(&[decl.clone(), anchor], &[], "scip")
            .unwrap();
        let def = Node::new(
            VName::new(
                "c",
                "",
                "src/widget.cpp",
                "cpp",
                "scip:src/widget.cpp:cxx . . $ Widget#draw(49f6e7a06ebc5aa8).",
            ),
            "function",
        )
        .with_line(12);
        let mut refs: Vec<ScipRef> = Vec::new();
        let out = unify_all(&mut store, "c", std::slice::from_ref(&def), &mut refs);
        assert_eq!(out.alias_map.get(&def.id), Some(&decl.id));
    }

    #[test]
    fn cross_file_unification_never_crosses_languages() {
        // A Scala def with no Scala twin must not alias onto the only
        // `method:Animal.name` in the corpus when that one is Ruby: the refs
        // redirected through the alias became a Scala -> Ruby `ref/call`.
        let mut store = SqliteStore::open_in_memory().unwrap();
        let scala_class = Node::new(
            VName::new("c", "", "scala/src/Animal.scala", "scala", "class:Animal"),
            "class",
        )
        .with_line(1)
        .with_end_line(6);
        let ruby_name = Node::new(
            VName::new("c", "", "ruby/src/animal.rb", "ruby", "method:Animal.name"),
            "method",
        )
        .with_line(4)
        .with_end_line(4);
        store
            .write_phase_b_batch(&[scala_class, ruby_name.clone()], &[], "scip")
            .unwrap();

        let sdb = Node::new(
            VName::new(
                "c",
                "",
                "scala/src/Animal.scala",
                "scala",
                "sdb:_empty_/Animal#name().",
            ),
            "method",
        )
        .with_line(2);
        let mut refs = vec![ScipRef {
            caller_path: "scala/src/Animal.scala".to_string(),
            caller_line: 4,
            callee_id: sdb.id,
            is_call: true,
            caller_col: None,
        }];
        let out = unify_all(&mut store, "c", std::slice::from_ref(&sdb), &mut refs);

        assert_eq!(out.alias_map.get(&sdb.id), None);
        assert_eq!(refs[0].callee_id, sdb.id, "ref not redirected to Ruby");
    }

    #[test]
    fn a_fully_unified_pass_reports_no_misses() {
        // The list must be empty when nothing misses, so status prints no stray
        // rows.
        let mut store = SqliteStore::open_in_memory().unwrap();
        let ts = Node::new(
            VName::new("c", "main", "lib/app.rb", "ruby", "method:App.run"),
            "method",
        )
        .with_line(2)
        .with_end_line(2);
        store
            .write_phase_b_batch(std::slice::from_ref(&ts), &[], "scip")
            .unwrap();
        let good = scip_node("lib/app.rb", "scip-ruby gem g 0.0.0 App#run().", 2);
        let mut refs: Vec<ScipRef> = Vec::new();
        let out = unify_all(&mut store, "c", &[good], &mut refs);
        assert_eq!(out.unified, out.attempted);
        assert!(out.misses.is_empty(), "no misses => empty list");
    }

    #[test]
    fn scip_def_in_unindexed_file_is_kept_not_counted() {
        // #780: scip-ruby indexes gitignored vendored code the tree-sitter parser
        // skips, so those files hold only SCIP defs and no Phase A node. Such a
        // def can never reconcile, so it is excluded from the miss rate — but it
        // is a real navigable definition (calls from app code into gem code are
        // exactly the cross-boundary edges the graph is most useful for), so it
        // is KEPT, not dropped.
        let mut store = SqliteStore::open_in_memory().unwrap();
        // An indexed app file with a real Phase A twin (so its path is "indexed").
        let ts = Node::new(
            VName::new("c", "main", "lib/app.rb", "ruby", "method:App.run"),
            "method",
        )
        .with_line(2)
        .with_end_line(2);
        store
            .write_phase_b_batch(std::slice::from_ref(&ts), &[], "scip")
            .unwrap();

        let app = scip_node("lib/app.rb", "scip-ruby gem g 0.0.0 App#run().", 2);
        // A vendored def whose file has NO Phase A node at all.
        let vendored = scip_node(
            "vendor/bundle/ruby/3.4.0/gems/rake/lib/rake/task.rb",
            "scip-ruby gem g 0.0.0 Rake#Task#invoke().",
            10,
        );

        let nodes = vec![app.clone(), vendored.clone()];
        let mut refs: Vec<ScipRef> = Vec::new();
        let out = unify_all(&mut store, "c", &nodes, &mut refs);

        assert_eq!(out.attempted, 1, "only the indexed-file def is an attempt");
        assert_eq!(out.unified, 1);
        assert!(
            !out.dropped.contains(&vendored.id),
            "def in an unindexed (vendored) file must be kept, not dropped"
        );
        assert!(
            !out.alias_map.contains_key(&vendored.id),
            "the vendored def has no twin, so it is not aliased away either"
        );
        assert!(!out.dropped.contains(&app.id));
    }

    #[test]
    fn real_def_inside_describe_block_unifies_not_dropped() {
        // #780 defect 1: a spec routinely defines helpers inside a `describe`
        // block. scip-ruby qualifies them by the block (`<describe 'Foo'>#…`),
        // but Phase A emits an unqualified twin (a block is not a
        // `method_container`). Only the container is a DSL scope, so clearing it
        // lets the def reconcile onto the real twin instead of being deleted with
        // every reference to it.
        let mut store = SqliteStore::open_in_memory().unwrap();
        let spec = "fastlane/spec/foo_spec.rb";
        // Phase A twins: an unqualified helper method and a helper class, both
        // inside the `describe` block, on the lines scip-ruby reports them.
        let helper_fn = Node::new(
            VName::new("c", "main", spec, "ruby", "fn:helper"),
            "function",
        )
        .with_line(4)
        .with_end_line(4);
        let helper_class = Node::new(
            VName::new("c", "main", spec, "ruby", "class:Helper"),
            "class",
        )
        .with_line(2)
        .with_end_line(2);
        store
            .write_phase_b_batch(&[helper_fn.clone(), helper_class.clone()], &[], "scip")
            .unwrap();

        let scip_fn = scip_node(
            spec,
            "scip-ruby gem fastlane 0.0.0 `<describe 'Foo'>`#`helper`().",
            4,
        );
        let scip_class = scip_node(
            spec,
            "scip-ruby gem fastlane 0.0.0 `<describe 'Foo'>`#Helper#",
            2,
        );

        let nodes = vec![scip_fn.clone(), scip_class.clone()];
        let mut refs: Vec<ScipRef> = Vec::new();
        let out = unify_all(&mut store, "c", &nodes, &mut refs);

        assert_eq!(
            out.alias_map.get(&scip_fn.id),
            Some(&helper_fn.id),
            "describe-block helper method must unify onto its unqualified twin"
        );
        assert_eq!(
            out.alias_map.get(&scip_class.id),
            Some(&helper_class.id),
            "describe-block helper class must unify onto its twin"
        );
        assert!(
            !out.dropped.contains(&scip_fn.id) && !out.dropped.contains(&scip_class.id),
            "recovered defs must not be dropped"
        );
        assert_eq!(out.attempted, 2, "both count once they have unified");
        assert_eq!(out.unified, 2);
    }

    #[test]
    fn unreconcilable_describe_block_def_is_dropped_not_counted() {
        // A def qualified only by a `describe` block that has NO Phase A twin
        // (e.g. metaprogramming the parser cannot see) still misses after the
        // container clear. It is dropped un-counted, so it neither steals edges
        // nor regresses the miss rate.
        let mut store = SqliteStore::open_in_memory().unwrap();
        let spec = "fastlane/spec/bar_spec.rb";
        // An unrelated indexed twin so the path is "indexed".
        let other = Node::new(
            VName::new("c", "main", spec, "ruby", "fn:other"),
            "function",
        )
        .with_line(1)
        .with_end_line(1);
        store
            .write_phase_b_batch(std::slice::from_ref(&other), &[], "scip")
            .unwrap();

        let orphan = scip_node(
            spec,
            "scip-ruby gem fastlane 0.0.0 `<describe 'Bar'>`#`ghost`().",
            9,
        );
        let nodes = vec![orphan.clone()];
        let mut refs: Vec<ScipRef> = Vec::new();
        let out = unify_all(&mut store, "c", &nodes, &mut refs);

        assert!(out.dropped.contains(&orphan.id), "no twin -> dropped");
        assert_eq!(out.attempted, 0, "a DSL-contained miss is not an attempt");
        assert_eq!(out.unified, 0);
    }
}
