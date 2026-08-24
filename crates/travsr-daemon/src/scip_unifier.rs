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
    /// SCIP def nodes with no reconcilable Phase A counterpart that must not be
    /// counted as misses (#780): Sorbet synthetic DSL meta-scopes (RSpec
    /// `describe`/`context`/`it` blocks), and defs in files the tree-sitter parser
    /// never indexed (gitignored vendored code scip-ruby indexes anyway). Neither
    /// is a reconciliation failure — no twin exists or can. Excluded from
    /// `attempted`/`unified`, and dropped by the caller (node plus its inbound
    /// refs/edges) so they stop surviving as orphan duplicates that steal edges.
    pub dropped: HashSet<NodeId>,
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
    let mut unmatched: Vec<(NodeId, &str, Vec<String>, &str)> = Vec::new();

    // #780: paths the tree-sitter parser actually indexed. A SCIP def in a file
    // absent from this set is one only the SCIP tool saw — gitignored vendored
    // code (scip-ruby indexes `vendor/bundle`, tree-sitter skips it) — and can
    // never reconcile, so it is excluded from the miss counters and dropped
    // rather than counted as a failure or kept as an edge-stealing orphan.
    let indexed_paths = store.phase_a_indexed_paths(corpus).unwrap_or_default();

    for node in nodes {
        let scip_sym = travsr_indexer::scip_unifier::scip_symbol_from_sig(&node.vname.signature);
        // Primary: SCIP descriptor grammar (go/java/ruby/c#/c/c++/… + rust/ts/py
        // LSIF). Fallback: bespoke sidecars (kotlin/swift) whose signatures are
        // Phase-A-style (`fn:Container.name`, `swift::Container.name`) and never
        // parse as SCIP. The fallback is gated to those languages so native
        // Phase A/rust nodes — whose signatures look identical — are never
        // re-unified against themselves.
        let (parsed, is_scip) = match travsr_indexer::scip_unifier::scip_name_kind(scip_sym) {
            Some(p) => (p, true),
            None if matches!(node.vname.language.as_str(), "kotlin" | "swift" | "dart") => {
                match travsr_indexer::scip_unifier::native_name_kind(
                    &node.vname.signature,
                    &node.kind,
                ) {
                    Some(p) => (p, false),
                    None => continue,
                }
            }
            None => continue,
        };
        // #780: a SCIP def in a file the tree-sitter parser never indexed
        // (gitignored vendored code that scip-ruby indexes anyway) has no twin
        // and none can exist — it is not a reconciliation failure. Drop it and
        // exclude it from the counters. Native sidecar nodes (kotlin/swift/dart)
        // are never path-excluded: their twins live in the same indexed sources.
        if is_scip && !indexed_paths.contains(&node.vname.path) {
            dropped.insert(node.id);
            continue;
        }
        // #780: Sorbet models RSpec `describe`/`context`/`it` blocks as singleton
        // scopes and emits SCIP defs for them, but tree-sitter (correctly) sees a
        // method call with a block, so no Phase A twin exists or can. These are
        // not definitions: exclude them from the miss counters entirely and drop
        // the node so it stops orphaning as a duplicate that steals spec-file
        // reference edges (~70% of issue #780's headline rate).
        if travsr_indexer::scip_unifier::is_synthetic_dsl_scope(&parsed) {
            dropped.insert(node.id);
            continue;
        }
        // No definition line means line-proximity matching is meaningless —
        // unwrapping to 0 would let any same-named node on lines 1..=5 of the
        // file match wrongly. Skip instead.
        let Some(line) = node.line else {
            continue;
        };
        let candidates = travsr_indexer::scip_unifier::candidate_signatures(&parsed);
        let scip_line = line as i64;
        let counts_toward_miss_rate = matches!(parsed.kind, "function" | "class");
        if counts_toward_miss_rate {
            attempted_syms.insert(scip_sym);
        }

        match store.find_ts_node_for_unification(
            corpus,
            &node.vname.path,
            &candidates,
            scip_line,
            MAX_LINE_DELTA,
        ) {
            Ok(Some(ts_id)) => {
                aliases.push((scip_sym.to_string(), ts_id));
                alias_map.insert(node.id, ts_id);
                sym_to_ts.entry(scip_sym).or_insert(ts_id);
                if counts_toward_miss_rate {
                    unified_syms.insert(scip_sym);
                }
                tracing::trace!(symbol = %scip_sym, ?ts_id, "G1: unified");
            }
            Ok(None) => unmatched.push((node.id, scip_sym, candidates, parsed.kind)),
            Err(e) => tracing::warn!(symbol = %scip_sym, "G1: DB lookup: {e:#}"),
        }
    }

    // Cross-file duplicate collapse: an unmatched def whose symbol unified in a
    // different file is the same definition (Obj-C interface/implementation,
    // C/C++ header/source). Alias it onto that node so it is dropped as a
    // duplicate and its edges/refs rewrite onto the real node, and credit its
    // symbol as unified so the miss-rate does not penalize the benign twin.
    for (node_id, sym, candidates, kind) in unmatched {
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
        match store.find_unique_ts_node_across_files(corpus, &candidates) {
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

    let attempted = attempted_syms.len();
    let unified = unified_syms.len();

    if let Err(e) = store.register_symbol_aliases(&aliases) {
        tracing::warn!("G1: register_symbol_aliases batch: {e:#}");
    }

    for r in refs.iter_mut() {
        if let Some(&ts_id) = alias_map.get(&r.callee_id) {
            r.callee_id = ts_id;
        }
    }

    tracing::debug!(
        aliased = alias_map.len(),
        callable_unified = unified,
        callable_attempted = attempted,
        total = nodes.len(),
        "G1: unification complete"
    );
    UnifyOutcome {
        unified,
        attempted,
        alias_map,
        dropped,
    }
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
    fn scip_def_in_unindexed_file_is_dropped_not_counted() {
        // #780: scip-ruby indexes gitignored vendored code the tree-sitter parser
        // skips, so those files hold only SCIP defs and no Phase A node. Such a
        // def can never reconcile — it must be dropped and excluded from the miss
        // rate, not counted as a failure.
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
            out.dropped.contains(&vendored.id),
            "def in an unindexed (vendored) file must be dropped"
        );
        assert!(!out.dropped.contains(&app.id));
    }
}
