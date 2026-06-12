//! G1 unification pass — bridges indexer SCIP output with the store.
//!
//! For each SCIP Phase B node that can be matched to an existing tree-sitter
//! node by (path, name, kind, line proximity), this module:
//!   1. Registers a `symbol_aliases` row so future lookups resolve correctly.
//!   2. Patches `ScipRef.callee_id` to point to the unified TS node so that
//!      `write_scip_attributed_batch` emits edges on the right node.
//!
//! RFC-014 §G1. Go language first; others added per acceptance metrics.

use travsr_core::{NodeId, ScipRef};
use travsr_store::SqliteStore;

/// Maximum line distance allowed between a SCIP definition and its TS match.
const MAX_LINE_DELTA: i64 = 5;

/// G1 unification pass for Go nodes.
///
/// Must be called **after** Phase A tree-sitter nodes with `end_line` spans
/// are written to the DB, and **before** `write_scip_attributed_batch`.
pub fn unify_go(
    store: &mut SqliteStore,
    corpus: &str,
    nodes: &[travsr_core::Node],
    refs: &mut [ScipRef],
) {
    // Maps SCIP NodeId → unified TS NodeId for ref-patching.
    let mut alias_map: Vec<(NodeId, NodeId)> = Vec::new();

    for node in nodes {
        if node.vname.language != "go" {
            continue;
        }
        let scip_sym = travsr_indexer::scip_unifier::scip_symbol_from_sig(&node.vname.signature);
        let Some((name, ts_kind)) = travsr_indexer::scip_unifier::go_name_kind(scip_sym) else {
            continue;
        };
        let scip_line = node.line.unwrap_or(0) as i64;

        match store.find_ts_node_for_unification(
            corpus,
            &node.vname.path,
            ts_kind,
            name,
            scip_line,
            MAX_LINE_DELTA,
        ) {
            Ok(Some(ts_id)) => {
                if let Err(e) = store.register_symbol_alias(scip_sym, ts_id) {
                    tracing::warn!(symbol = %scip_sym, "G1: register_symbol_alias: {e:#}");
                } else {
                    alias_map.push((node.id, ts_id));
                    tracing::trace!(symbol = %scip_sym, ?ts_id, "G1: unified");
                }
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(symbol = %scip_sym, "G1: DB lookup: {e:#}"),
        }
    }

    for r in refs.iter_mut() {
        for &(scip_id, ts_id) in &alias_map {
            if r.callee_id == scip_id {
                r.callee_id = ts_id;
                break;
            }
        }
    }

    tracing::debug!(
        unified = alias_map.len(),
        total = nodes.len(),
        "G1: Go unification complete"
    );
}
