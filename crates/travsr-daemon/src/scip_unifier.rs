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

use std::collections::HashMap;

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
    let mut unmatched: Vec<(NodeId, &str)> = Vec::new();

    for node in nodes {
        let scip_sym = travsr_indexer::scip_unifier::scip_symbol_from_sig(&node.vname.signature);
        // Primary: SCIP descriptor grammar (go/java/ruby/c#/c/c++/… + rust/ts/py
        // LSIF). Fallback: bespoke sidecars (kotlin/swift) whose signatures are
        // Phase-A-style (`fn:Container.name`, `swift::Container.name`) and never
        // parse as SCIP. The fallback is gated to those languages so native
        // Phase A/rust nodes — whose signatures look identical — are never
        // re-unified against themselves.
        let parsed = match travsr_indexer::scip_unifier::scip_name_kind(scip_sym) {
            Some(p) => p,
            None if matches!(node.vname.language.as_str(), "kotlin" | "swift" | "dart") => {
                match travsr_indexer::scip_unifier::native_name_kind(
                    &node.vname.signature,
                    &node.kind,
                ) {
                    Some(p) => p,
                    None => continue,
                }
            }
            None => continue,
        };
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
            Ok(None) => unmatched.push((node.id, scip_sym)),
            Err(e) => tracing::warn!(symbol = %scip_sym, "G1: DB lookup: {e:#}"),
        }
    }

    // Cross-file duplicate collapse: an unmatched def whose symbol unified in a
    // different file is the same definition (Obj-C interface/implementation,
    // C/C++ header/source). Alias it onto that node so it is dropped as a
    // duplicate and its edges/refs rewrite onto the real node, and credit its
    // symbol as unified so the miss-rate does not penalize the benign twin.
    for (node_id, sym) in unmatched {
        if let Some(&ts_id) = sym_to_ts.get(sym) {
            aliases.push((sym.to_string(), ts_id));
            alias_map.insert(node_id, ts_id);
            // Only callable/type symbols feed the miss-rate; crediting a
            // `variable` twin would let `unified` exceed `attempted`.
            if attempted_syms.contains(sym) {
                unified_syms.insert(sym);
            }
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
    }
}
