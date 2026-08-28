//! RFC-027 live semantic resolution — the between-commits overlay.
//!
//! Phase B (SCIP) is commit-gated on purpose, so between commits only Phase A
//! runs and a cross-file reference stays unresolved: Tree-sitter knows that
//! `user.save()` is a call, not *which* `save`. This module closes that window
//! by emitting `provenance='live'` edges for references it can resolve
//! **precisely**, and abstaining otherwise.
//!
//! The division of labour is the whole design (RFC-027 section 5):
//!
//! ```text
//! Tree-sitter  ->  DETECTS      a reference exists
//! SCIP graph   ->  OWNS         node identity (Kythe VName)
//! LSP          ->  DISAMBIGUATES a specific position
//! Commit SCIP  ->  RATIFIES     the region, heals drift
//! ```
//!
//! Two emit lanes, both fail-closed (section 8.1):
//!
//! - [`resolve_unambiguous_lexical`] (section 7.3a) needs no language server.
//!   A name with exactly one definition in the graph has only one thing it can
//!   mean, so resolving it is a lookup, not a guess.
//! - [`apply_live_resolutions`] (section 7.3b) consumes positions an editor's
//!   language provider resolved. The editor answers "what does this position
//!   point at"; this module maps both ends to nodes itself, so identity stays
//!   SCIP's (section 8.2 fencing rule). Nothing here mints a VName.
//!
//! Anything ambiguous, unmappable, or cross-corpus abstains. A missing edge
//! fails safe (fall back to grep); a wrong edge fails silent and dangerous,
//! which for a "zero structural hallucinations" product is a breach of the
//! value proposition rather than a quality regression.

use travsr_core::{Edge, EdgeKind, NodeId, UnresolvedCall};
use travsr_ipc::message::LiveResolution;
use travsr_store::{SqliteStore, Store};

/// What one live-resolution pass did, for logging and the Phase 3 precision
/// meter. `pending` is not a failure: it is the fail-closed path working.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LiveOutcome {
    /// Edges written with `provenance='live'`.
    pub emitted: usize,
    /// References that could not be resolved precisely and were abstained on.
    pub pending: usize,
}

impl LiveOutcome {
    fn emit(&mut self) {
        self.emitted += 1;
    }
    fn abstain(&mut self) {
        self.pending += 1;
    }
}

/// Section 7.3b: turn editor-resolved positions into `live` edges.
///
/// `file` is the dirty file the references live in, repo-relative with forward
/// slashes, matching the graph's path keys.
///
/// Each resolution is mapped independently and a failure abstains rather than
/// aborting the batch: one unmappable position must not cost the others their
/// freshness.
pub fn apply_live_resolutions(
    store: &mut SqliteStore,
    corpus: &str,
    file: &str,
    resolutions: &[LiveResolution],
) -> LiveOutcome {
    let mut outcome = LiveOutcome::default();
    for r in resolutions {
        match resolve_one(store, corpus, file, r) {
            Some(edge) => {
                if let Err(e) = store.put_edge_live(&edge) {
                    // A write failure is a freshness loss, never a correctness
                    // one: the commit-gated path still ratifies this region.
                    tracing::debug!(error = %e, "live edge write failed");
                    outcome.abstain();
                } else {
                    outcome.emit();
                }
            }
            None => outcome.abstain(),
        }
    }
    if outcome.emitted > 0 || outcome.pending > 0 {
        tracing::debug!(
            event = "live.resolved",
            file = %file,
            emitted = outcome.emitted,
            pending = outcome.pending,
            "live semantic resolution pass complete"
        );
    }
    outcome
}

/// Map one editor resolution to an edge, or `None` to abstain.
fn resolve_one(store: &SqliteStore, corpus: &str, file: &str, r: &LiveResolution) -> Option<Edge> {
    // The call site's enclosing definition is the edge's source. A reference at
    // top level (no enclosing function) has no caller node to attach to.
    let src = store
        .enclosing_definition_at(corpus, file, r.ref_line)
        .ok()
        .flatten()?;
    // Section 7.5: the definition the editor pointed at. If the position lands
    // in no definition span the target is outside the graph (a node_modules
    // file, a generated stub, a file the indexer skipped), so abstain.
    let dst = store
        .enclosing_definition_at(corpus, &r.target_path, r.target_line)
        .ok()
        .flatten()?;
    edge_if_sound(store, src, dst)
}

/// RFC-027 section 6: what an edit actually invalidates.
///
/// The distinction is not cosmetic. *Outgoing* edges (edited file to others) are
/// recomputable from the edited file alone. *Incoming* edges (others to the
/// edited file) are not, because their sources live elsewhere — and they stay
/// correct only while the referenced surface is unchanged. That asymmetry is
/// the whole reason a body edit can be resolved locally and an interface edit
/// cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    /// No symbol's referenced surface changed. Re-resolve this file's outgoing
    /// references and touch nothing else. This is the common case, and the one
    /// that has to stay cheap or the live lane is not worth having.
    Body,
    /// A symbol other files reference was renamed, removed, or replaced. Their
    /// edges into this file may now be wrong, so the reverse closure is
    /// re-resolved (section 6.3).
    Interface,
}

/// Classify an edit from the report `reindex_replace` already returns.
///
/// Deliberately does not compute a second diff. `ReplaceReport` is derived
/// inside the same transaction that rewrote the file, from the authoritative
/// old-versus-new NodeId sets, so any diff computed here would be a less
/// reliable copy of it. `removed_count` counts symbols whose id vanished, and
/// `callers` names the files that had edges into them.
///
/// Conservative by construction: a rename is a remove plus an add, so it lands
/// in `Interface` without needing to be told apart from a delete. Section 16.2
/// notes that mis-classifying toward `Interface` costs recall and never
/// correctness, and this is where that choice is made.
pub fn classify_edit(report: &travsr_core::ReplaceReport) -> EditKind {
    if report.removed_count > 0 || !report.callers.is_empty() {
        EditKind::Interface
    } else {
        EditKind::Body
    }
}

/// Section 6.3: the files whose edges into `changed` may now be wrong.
///
/// Reverse lookup over `iter_edges_to`, which the covering index
/// `idx_edges_dst_kind_cov` serves without touching the nodes table for the
/// edge scan itself.
///
/// **Depth one, not transitive** (Risk R4). A rename invalidates the edges that
/// point *at* the renamed symbol; it does not invalidate the callers of those
/// callers, whose own referenced surface did not change. Walking transitively
/// would pull in most of a repo from one edit to a hot file, for no additional
/// correctness. The existing Tier-0 propagation makes the same choice for the
/// same reason.
///
/// `MAX_CLOSURE_FILES` bounds the result even at depth one: a symbol with
/// thousands of callers is a hot utility, and re-resolving every one of them on
/// each keystroke would cost more than the freshness is worth. Truncation costs
/// recall, which the commit-gated path then repairs.
pub fn reverse_closure(
    store: &SqliteStore,
    changed: &[NodeId],
) -> std::collections::HashSet<String> {
    let mut files = std::collections::HashSet::new();
    for &id in changed {
        let Ok(incoming) = store.iter_edges_to(id) else {
            continue;
        };
        for edge in incoming {
            if files.len() >= MAX_CLOSURE_FILES {
                return files;
            }
            if let Ok(Some(node)) = store.get_node(edge.src) {
                files.insert(node.vname.path);
            }
        }
    }
    files
}

/// Upper bound on files pulled into one reverse closure (Risk R4).
const MAX_CLOSURE_FILES: usize = 64;

/// Section 7.3a: emit for calls whose callee signature is unambiguous repo-wide.
///
/// This lane needs no language server and is the zero-regression floor: with no
/// editor attached it is the only lane that runs. It is seeded by the
/// [`UnresolvedCall`]s the native Phase B call-site extractor already produces,
/// so the live lane detects references with the same machinery Phase B does
/// rather than inventing a second extractor.
///
/// **It deliberately does not reuse the daemon's `resolve_unresolved_calls`.**
/// That resolver is recall-biased by design: its own contract says
/// "overconnection is safe: when multiple nodes share a signature all matches
/// are emitted. PPR damping absorbs the noise." The live lane has the opposite
/// policy (section 8.1): it is precision-first and abstains on ambiguity,
/// because an un-ratified wrong edge is exactly the failure the RFC forbids.
/// The two policies are both correct for their lane, and they must not be
/// merged.
///
/// `lookup_nodes_exact(sig, None)` returns the candidate set; only a set of
/// size one is emitted. `alt_callee_sig` is tried when the primary misses (the
/// #709 PascalCase class-vs-function ambiguity), under the same exactly-one
/// rule, never to widen the net.
pub fn resolve_unambiguous_lexical(
    store: &mut SqliteStore,
    corpus: &str,
    path: &str,
    unresolved: &[UnresolvedCall],
) -> LiveOutcome {
    let mut outcome = LiveOutcome::default();
    // Section 9.2: every reference this pass saw, and what became of it. An
    // abstention is recorded, not dropped — "there is a call here and its
    // target is not yet known" is true and useful, and saying it is what makes
    // fail-closed honest rather than merely quiet.
    let mut states: Vec<travsr_store::RefResolution> = Vec::with_capacity(unresolved.len());

    for call in unresolved {
        let name = travsr_core::ident::leaf_of(&call.callee_sig).to_string();
        // The target this lane claimed, kept whatever later happens to the edge.
        // Section 12's meter needs the claim itself: by ratification a re-derived
        // live edge has been relabelled and an unratified one is about to be
        // swept, so the edges table can no longer say what the lane decided.
        let claimed = match lexical_one(store, call) {
            Some(edge) => match store.put_edge_live(&edge) {
                Ok(()) => Some(edge.dst),
                Err(e) => {
                    tracing::debug!(error = %e, "live lexical edge write failed");
                    None
                }
            },
            None => None,
        };
        if claimed.is_some() {
            outcome.emit();
        } else {
            outcome.abstain();
        }
        // `ref_col` is 0: the native extractor records the call's line but not
        // its column. The PK tolerates it, and a second call to the same name
        // on one line is the only collision it can cause, which merges two
        // identical facts rather than losing one.
        states.push(travsr_store::RefResolution {
            src: call.src,
            ref_line: call.caller_line,
            ref_col: 0,
            name,
            state: if claimed.is_some() {
                "resolved"
            } else {
                "pending"
            },
            resolved_dst: claimed,
        });
    }

    if let Err(e) = store.replace_ref_resolution_states(corpus, path, &states) {
        // Losing the pending record costs the freshness note its detail, never
        // the graph its correctness.
        tracing::debug!(error = %e, "recording ref_resolution_state failed");
    }
    if outcome.emitted > 0 || outcome.pending > 0 {
        tracing::debug!(
            event = "live.lexical",
            path = %path,
            emitted = outcome.emitted,
            pending = outcome.pending,
            "unambiguous-lexical live resolution complete"
        );
    }
    outcome
}

fn lexical_one(store: &SqliteStore, call: &UnresolvedCall) -> Option<Edge> {
    let dst = candidate_signatures(call)
        .into_iter()
        .find_map(|sig| unique_definition(store, &sig))?;
    edge_if_sound(store, call.src, dst)
}

/// The exact signatures a call could name, most specific first.
///
/// This mirrors how the native extractor encodes a call site, which is *not*
/// simply `callee_sig`. For `u.save()` the extractor emits
/// `callee_sig = "fn:save"` with `is_method_call = true` and the receiver type
/// recovered into `recv_type`, while the definition node is `method:User.save`.
/// Looking up `callee_sig` alone therefore finds nothing.
///
/// A method or field access **without** a recovered receiver type yields no
/// candidate at all, and so abstains. That is deliberate: an untyped receiver
/// is exactly the method-on-receiver ambiguity section 7.3a cannot settle and
/// section 7.3b's LSP lane exists for. Guessing by leaf name here is what the
/// recall-biased Phase B resolver does, and it is the wrong policy for an
/// un-ratified edge.
fn candidate_signatures(call: &UnresolvedCall) -> Vec<String> {
    let leaf = travsr_core::ident::leaf_of(&call.callee_sig);
    if let Some(recv) = call.recv_type.as_deref() {
        // Positive type evidence: resolve against the qualified definition.
        let kind = if call.callee_sig.starts_with("field:") {
            "field"
        } else {
            "method"
        };
        return vec![format!("{kind}:{recv}.{leaf}")];
    }
    if call.is_method_call || call.callee_sig.starts_with("field:") {
        return Vec::new();
    }
    // A free function or constructor names its definition directly.
    let mut sigs = vec![call.callee_sig.clone()];
    // #709: a second exact try for the genuinely ambiguous PascalCase shape,
    // never a widening of the net.
    if let Some(alt) = call.alt_callee_sig.clone() {
        sigs.push(alt);
    }
    sigs
}

/// The single definition named by `signature`, or `None` when there are zero or
/// more than one.
///
/// `lookup_nodes_exact(sig, None)` documents exactly this contract: one row is
/// an unambiguous match, more than one is "genuinely ambiguous; caller must
/// disambiguate". Ambiguity abstains, which is the whole precision guarantee of
/// section 7.3a.
fn unique_definition(store: &SqliteStore, signature: &str) -> Option<NodeId> {
    let candidates = store.lookup_nodes_exact(signature, None).ok()?;
    let defs: Vec<&travsr_core::Node> = candidates
        .iter()
        .filter(|n| is_definition(&n.kind))
        .collect();
    match defs.as_slice() {
        [only] => Some(only.id),
        _ => None,
    }
}

/// Shared soundness gate for both lanes.
///
/// Section 8.2 fencing rule: live edges are intra-corpus only, so they can
/// never violate RFC-005's `src.corpus == dst.corpus` invariant or feed a
/// synthetic symbol into the bridge registry (ADR-009 Rule 4). Self-edges are
/// dropped because a symbol referencing itself carries no traversal value and
/// would show up as a spurious self-loop in `get_callers`.
fn edge_if_sound(store: &SqliteStore, src: NodeId, dst: NodeId) -> Option<Edge> {
    if src == dst {
        return None;
    }
    let src_node = store.get_node(src).ok().flatten()?;
    let dst_node = store.get_node(dst).ok().flatten()?;
    if src_node.vname.corpus != dst_node.vname.corpus {
        return None;
    }
    Some(Edge::new(src, dst, EdgeKind::RefCall))
}

/// Kinds a reference can resolve to. Mirrors the definition kinds the store's
/// own `definition_node_ids_in_file` recognises, so the two agree on what
/// counts as a definition.
fn is_definition(kind: &str) -> bool {
    matches!(
        kind,
        "function"
            | "method"
            | "fn"
            | "class"
            | "interface"
            | "struct"
            | "trait"
            | "enum"
            | "type"
            | "typedef"
            | "union"
            | "object"
            | "protocol"
            | "mixin"
            | "extension"
            | "namespace"
            | "init"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use travsr_core::{Node, VName};

    const CORPUS: &str = "testrepo";

    fn store_with(nodes: &[(&str, &str, &str, u32, u32)]) -> SqliteStore {
        let mut store = SqliteStore::open_in_memory().expect("in-memory store");
        for (path, sig, kind, line, end_line) in nodes {
            let vname = VName::new(CORPUS, "", *path, "typescript", *sig);
            let mut node = Node::new(vname, *kind);
            node.line = Some(*line);
            node.end_line = Some(*end_line);
            store.put_node(&node).expect("put_node");
        }
        store
    }

    fn node_id(path: &str, sig: &str) -> NodeId {
        VName::new(CORPUS, "", path, "typescript", sig).id()
    }

    fn resolution(
        ref_line: u32,
        name: &str,
        target_path: &str,
        target_line: u32,
    ) -> LiveResolution {
        LiveResolution {
            ref_line,
            ref_col: 4,
            name: name.to_string(),
            target_path: target_path.to_string(),
            target_line,
            buffer_version: 1,
        }
    }

    fn provenance_of(store: &SqliteStore, src: NodeId, dst: NodeId) -> Option<String> {
        store
            .iter_edges_from(src)
            .expect("iter_edges_from")
            .into_iter()
            .find(|e| e.dst == dst)
            .and_then(|e| e.provenance)
    }

    /// The happy path (section 7.3b): an editor resolves `save()` inside
    /// `placeOrder` to `User.save`, and a live edge appears between the two
    /// enclosing definitions.
    #[test]
    fn an_editor_resolution_becomes_a_live_edge() {
        let mut store = store_with(&[
            ("src/order.ts", "fn:placeOrder", "function", 10, 30),
            ("src/user.ts", "method:User.save", "method", 15, 20),
        ]);
        let out = apply_live_resolutions(
            &mut store,
            CORPUS,
            "src/order.ts",
            &[resolution(18, "save", "src/user.ts", 17)],
        );

        assert_eq!(
            out,
            LiveOutcome {
                emitted: 1,
                pending: 0
            }
        );
        assert_eq!(
            provenance_of(
                &store,
                node_id("src/order.ts", "fn:placeOrder"),
                node_id("src/user.ts", "method:User.save"),
            )
            .as_deref(),
            Some("live"),
            "the edge must be tagged live, never blended into ratified truth"
        );
    }

    /// Section 8.1: a definition position that lands outside every known span
    /// (node_modules, a generated stub, an unindexed file) abstains. It must
    /// not fall back to any nearest-node heuristic.
    #[test]
    fn a_target_outside_the_graph_abstains_instead_of_guessing() {
        let mut store = store_with(&[
            ("src/order.ts", "fn:placeOrder", "function", 10, 30),
            ("src/user.ts", "method:User.save", "method", 15, 20),
        ]);
        let out = apply_live_resolutions(
            &mut store,
            CORPUS,
            "src/order.ts",
            &[resolution(18, "save", "node_modules/lib/index.d.ts", 4)],
        );

        assert_eq!(
            out,
            LiveOutcome {
                emitted: 0,
                pending: 1
            }
        );
        assert!(
            store
                .iter_edges_from(node_id("src/order.ts", "fn:placeOrder"))
                .expect("iter_edges_from")
                .is_empty(),
            "abstention must write no edge at all"
        );
    }

    /// A reference at top level has no enclosing definition to hang an edge
    /// from, so it abstains rather than attaching to an arbitrary node.
    #[test]
    fn a_reference_with_no_enclosing_definition_abstains() {
        let mut store = store_with(&[
            ("src/order.ts", "fn:placeOrder", "function", 10, 30),
            ("src/user.ts", "method:User.save", "method", 15, 20),
        ]);
        // Line 2 is outside placeOrder's 10..30 span.
        let out = apply_live_resolutions(
            &mut store,
            CORPUS,
            "src/order.ts",
            &[resolution(2, "save", "src/user.ts", 17)],
        );
        assert_eq!(
            out,
            LiveOutcome {
                emitted: 0,
                pending: 1
            }
        );
    }

    /// One unmappable resolution must not cost the others their freshness.
    #[test]
    fn a_bad_resolution_does_not_abort_the_batch() {
        let mut store = store_with(&[
            ("src/order.ts", "fn:placeOrder", "function", 10, 30),
            ("src/user.ts", "method:User.save", "method", 15, 20),
        ]);
        let out = apply_live_resolutions(
            &mut store,
            CORPUS,
            "src/order.ts",
            &[
                resolution(18, "nope", "src/ghost.ts", 3),
                resolution(19, "save", "src/user.ts", 17),
            ],
        );
        assert_eq!(
            out,
            LiveOutcome {
                emitted: 1,
                pending: 1
            }
        );
    }

    fn call(src: NodeId, callee_sig: &str, line: u32) -> UnresolvedCall {
        UnresolvedCall {
            src,
            callee_sig: callee_sig.to_string(),
            caller_line: line,
            ..UnresolvedCall::default()
        }
    }

    /// Section 7.3a: a signature with exactly one definition repo-wide resolves
    /// with no language server. This is the zero-regression floor.
    #[test]
    fn an_unambiguous_name_resolves_without_a_language_server() {
        let mut store = store_with(&[
            ("src/order.ts", "fn:placeOrder", "function", 10, 30),
            ("src/user.ts", "method:User.save", "method", 15, 20),
        ]);
        let src = node_id("src/order.ts", "fn:placeOrder");
        let out = resolve_unambiguous_lexical(
            &mut store,
            CORPUS,
            "src/order.ts",
            &[call(src, "method:User.save", 18)],
        );

        assert_eq!(
            out,
            LiveOutcome {
                emitted: 1,
                pending: 0
            }
        );
        assert_eq!(
            provenance_of(
                &store,
                node_id("src/order.ts", "fn:placeOrder"),
                node_id("src/user.ts", "method:User.save"),
            )
            .as_deref(),
            Some("live")
        );
    }

    /// Section 7.3a's precision guarantee: two definitions sharing a signature
    /// is exactly the method-on-receiver case lexical matching cannot settle,
    /// so it abstains rather than picking one.
    ///
    /// This is also where the live lane deliberately diverges from the daemon's
    /// `resolve_unresolved_calls`, whose contract is to emit *all* matches and
    /// let PPR damping absorb the noise.
    #[test]
    fn an_ambiguous_name_abstains_rather_than_picking_one() {
        let mut store = store_with(&[
            ("src/order.ts", "fn:placeOrder", "function", 10, 30),
            ("src/user.ts", "method:save", "method", 15, 20),
            ("src/draft.ts", "method:save", "method", 5, 9),
        ]);
        let src = node_id("src/order.ts", "fn:placeOrder");
        let out = resolve_unambiguous_lexical(
            &mut store,
            CORPUS,
            "src/order.ts",
            &[call(src, "method:save", 18)],
        );

        assert_eq!(
            out,
            LiveOutcome {
                emitted: 0,
                pending: 1
            },
            "two candidates must abstain, never guess"
        );
        assert!(store
            .iter_edges_from(node_id("src/order.ts", "fn:placeOrder"))
            .expect("iter_edges_from")
            .is_empty());
    }

    /// `alt_callee_sig` is the #709 PascalCase class-vs-function ambiguity: the
    /// extractor names both shapes and lets the resolver decide on evidence. It
    /// is a second exact try, not a widening of the net, so it is still subject
    /// to the exactly-one rule.
    #[test]
    fn the_alt_signature_is_a_second_exact_try_not_a_wider_net() {
        let mut store = store_with(&[
            ("src/order.ts", "fn:placeOrder", "function", 10, 30),
            ("src/field.ts", "fn:Field", "function", 3, 8),
        ]);
        let src = node_id("src/order.ts", "fn:placeOrder");
        let mut c = call(src, "class:Field", 18);
        c.alt_callee_sig = Some("fn:Field".to_string());

        let out = resolve_unambiguous_lexical(&mut store, CORPUS, "src/order.ts", &[c]);
        assert_eq!(
            out,
            LiveOutcome {
                emitted: 1,
                pending: 0
            }
        );
        assert_eq!(
            provenance_of(&store, src, node_id("src/field.ts", "fn:Field")).as_deref(),
            Some("live")
        );
    }

    /// Section 8.2 fencing rule: live edges are intra-corpus only, so they can
    /// never violate RFC-005's `src.corpus == dst.corpus` invariant.
    #[test]
    fn a_cross_corpus_target_is_fenced_off() {
        let mut store = SqliteStore::open_in_memory().expect("in-memory store");
        let mut caller = Node::new(
            VName::new(CORPUS, "", "src/order.ts", "typescript", "fn:placeOrder"),
            "function",
        );
        caller.line = Some(10);
        caller.end_line = Some(30);
        let mut foreign = Node::new(
            VName::new(
                "othercorpus",
                "",
                "src/user.ts",
                "typescript",
                "method:User.save",
            ),
            "method",
        );
        foreign.line = Some(15);
        foreign.end_line = Some(20);
        store.put_node(&caller).expect("put_node");
        store.put_node(&foreign).expect("put_node");

        // Resolving within CORPUS cannot see the foreign node's path at all,
        // which is the fence doing its job one layer earlier.
        let out = apply_live_resolutions(
            &mut store,
            CORPUS,
            "src/order.ts",
            &[resolution(18, "save", "src/user.ts", 17)],
        );
        assert_eq!(
            out,
            LiveOutcome {
                emitted: 0,
                pending: 1
            }
        );
        assert!(store
            .iter_edges_from(caller.id)
            .expect("iter_edges_from")
            .is_empty());
    }

    /// A symbol referencing itself carries no traversal value and would surface
    /// as a spurious self-loop in `get_callers`.
    #[test]
    fn a_self_reference_emits_no_edge() {
        let mut store = store_with(&[("src/rec.ts", "fn:walk", "function", 4, 12)]);
        let out = apply_live_resolutions(
            &mut store,
            CORPUS,
            "src/rec.ts",
            &[resolution(8, "walk", "src/rec.ts", 5)],
        );
        assert_eq!(
            out,
            LiveOutcome {
                emitted: 0,
                pending: 1
            }
        );
    }

    /// Plan R5: `reindex_replace` deletes every outbound edge of a file on each
    /// save, so the engine re-emits whole-file. Re-emitting must be idempotent
    /// rather than accumulating duplicates or failing.
    #[test]
    fn re_emitting_after_a_save_is_idempotent() {
        let mut store = store_with(&[
            ("src/order.ts", "fn:placeOrder", "function", 10, 30),
            ("src/user.ts", "method:User.save", "method", 15, 20),
        ]);
        let batch = [resolution(18, "save", "src/user.ts", 17)];
        for _ in 0..3 {
            let out = apply_live_resolutions(&mut store, CORPUS, "src/order.ts", &batch);
            assert_eq!(
                out,
                LiveOutcome {
                    emitted: 1,
                    pending: 0
                }
            );
        }
        assert_eq!(
            store
                .iter_edges_from(node_id("src/order.ts", "fn:placeOrder"))
                .expect("iter_edges_from")
                .len(),
            1,
            "re-emitting must not accumulate duplicate edges"
        );
    }

    // ── Phase 4: Rust call-site encoding (RFC-027 §14) ──────────────────────
    //
    // `candidate_signatures` was written against TypeScript's encoding; Rust's
    // differs in three ways (see the fn doc). These tests pin each, so a change
    // to the extractor or the signature builder cannot silently regress Rust
    // recall or, worse, start double-qualifying an already-qualified sig.

    fn rust_store_with(nodes: &[(&str, &str, &str, u32, u32)]) -> SqliteStore {
        let mut store = SqliteStore::open_in_memory().expect("in-memory store");
        for (path, sig, kind, line, end_line) in nodes {
            let vname = VName::new(CORPUS, "", *path, "rust", *sig);
            let mut node = Node::new(vname, *kind);
            node.line = Some(*line);
            node.end_line = Some(*end_line);
            store.put_node(&node).expect("put_node");
        }
        store
    }

    fn rust_node_id(path: &str, sig: &str) -> NodeId {
        VName::new(CORPUS, "", path, "rust", sig).id()
    }

    /// `Type::method()` (an associated call like `Zoo::new()`) is emitted by the
    /// Rust extractor ALREADY QUALIFIED — `callee_sig = "method:Zoo.new"`, with
    /// no receiver type and `is_method_call = false` (phase_b_rust.rs, the
    /// `call.scoped` uppercase-qualifier arm). It must reach `unique_definition`
    /// verbatim: rebuilding `method:{recv}.{leaf}` would double-qualify it. A
    /// `None` recv_type and `!is_method_call` route it to the free-function
    /// branch, which uses the sig as-is. The plan flagged this as "correct by
    /// accident"; this pins it as correct on purpose.
    #[test]
    fn rust_associated_call_signature_is_used_verbatim_not_rebuilt() {
        let src = rust_node_id("src/main.rs", "fn:main");
        let c = call(src, "method:Zoo.new", 3);
        assert_eq!(candidate_signatures(&c), vec!["method:Zoo.new".to_string()]);
    }

    /// `recv.method()` (e.g. `zoo.add()`) is emitted as `callee_sig = "fn:add"`
    /// with `is_method_call = true` and the receiver type recovered into
    /// `recv_type`. The lane must rebuild `method:{recv}.{leaf}` to match the
    /// definition node `method:Zoo.add`; `fn:add` alone names no method.
    #[test]
    fn rust_method_call_with_receiver_type_rebuilds_the_qualified_sig() {
        let src = rust_node_id("src/zoo.rs", "method:Zoo.announce");
        let mut c = call(src, "fn:add", 5);
        c.is_method_call = true;
        c.recv_type = Some("Zoo".to_string());
        assert_eq!(candidate_signatures(&c), vec!["method:Zoo.add".to_string()]);
    }

    /// A method call with no recovered receiver type is the ambiguity §7.3a
    /// cannot settle, so it yields no candidate and abstains — the LSP lane
    /// (§7.3b) is what exists for it. Guessing by leaf name here is the
    /// recall-biased Phase B policy, wrong for an un-ratified edge.
    #[test]
    fn rust_method_call_without_receiver_type_yields_no_candidate() {
        let src = rust_node_id("src/zoo.rs", "method:Zoo.announce");
        let mut c = call(src, "fn:add", 5);
        c.is_method_call = true;
        c.recv_type = None;
        assert!(candidate_signatures(&c).is_empty());
    }

    /// A bare free-function call `helper()` names its definition directly.
    #[test]
    fn rust_bare_function_call_names_its_definition_directly() {
        let src = rust_node_id("src/main.rs", "fn:main");
        let c = call(src, "fn:helper", 2);
        assert_eq!(candidate_signatures(&c), vec!["fn:helper".to_string()]);
    }

    /// End to end on a Rust-labeled store: `Zoo::new()` resolves to the
    /// associated-function definition with a `live` edge and no double-qualify.
    #[test]
    fn rust_associated_call_resolves_to_a_live_edge() {
        let mut store = rust_store_with(&[
            ("src/main.rs", "fn:main", "function", 1, 6),
            ("src/zoo.rs", "method:Zoo.new", "method", 4, 9),
        ]);
        let src = rust_node_id("src/main.rs", "fn:main");
        let out = resolve_unambiguous_lexical(
            &mut store,
            CORPUS,
            "src/main.rs",
            &[call(src, "method:Zoo.new", 3)],
        );
        assert_eq!(
            out,
            LiveOutcome {
                emitted: 1,
                pending: 0
            }
        );
        assert_eq!(
            provenance_of(&store, src, rust_node_id("src/zoo.rs", "method:Zoo.new")).as_deref(),
            Some("live"),
        );
    }

    /// Rust field access `x.count` is emitted with `callee_sig = "field:count"`
    /// AND a recovered `recv_type` — the extractor only emits field refs when
    /// the receiver type is recoverable (phase_b_rust.rs guards
    /// `recv_type.is_none()`). So `candidate_signatures` DOES build
    /// `field:Zoo.count`, and the field definition node is present. It still
    /// abstains, because `is_definition` does not admit the `"field"` kind and
    /// `unique_definition` filters the node out. The abstention is by
    /// definition-kind, NOT (as an earlier note assumed) a missing receiver
    /// type. Recorded, not closed: a field read is not a call and must not mint
    /// a `RefCall` edge. Both facts are asserted here so neither can drift.
    #[test]
    fn rust_field_access_abstains_on_definition_kind_not_missing_receiver() {
        let mut c = call(
            rust_node_id("src/zoo.rs", "method:Zoo.tally"),
            "field:count",
            5,
        );
        c.recv_type = Some("Zoo".to_string());
        assert_eq!(
            candidate_signatures(&c),
            vec!["field:Zoo.count".to_string()],
            "the owner-qualified field sig is built; recv_type is present",
        );

        let mut store = rust_store_with(&[
            ("src/zoo.rs", "method:Zoo.tally", "method", 4, 9),
            ("src/zoo.rs", "field:Zoo.count", "field", 2, 2),
        ]);
        let out = resolve_unambiguous_lexical(&mut store, CORPUS, "src/zoo.rs", &[c]);
        assert_eq!(
            out,
            LiveOutcome {
                emitted: 0,
                pending: 1
            },
            "the field node exists and its sig matches; abstention is the is_definition filter",
        );
    }
}
