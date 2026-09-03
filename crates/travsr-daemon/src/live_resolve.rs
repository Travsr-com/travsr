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

use travsr_core::{Edge, EdgeKind, InheritanceRef, NodeId, UnresolvedCall};
use travsr_ipc::message::{LiveResolution, LiveResolutionTarget};
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
    // Section 12: what this lane claimed, so the precision meter can score it.
    // Without a claim row an editor-resolved edge is invisible to the meter, and
    // a language that runs this lane alone (every non-native one, §8.3) could
    // never earn its per-language gate a reading.
    let mut states: Vec<travsr_store::RefResolution> = Vec::with_capacity(resolutions.len());
    for r in resolutions {
        // The claim is keyed on the reference, so the edge's own source is the
        // key's `src`. A resolution whose line maps to no enclosing definition
        // has no reference to record and abstains below anyway.
        let src = store
            .enclosing_definition_at(corpus, file, r.ref_line)
            .ok()
            .flatten();
        let claimed = match resolve_one(store, corpus, file, r) {
            Some(edge) => match store.put_edge_live(&edge) {
                Ok(()) => Some(edge.dst),
                Err(e) => {
                    // A write failure is a freshness loss, never a correctness
                    // one: the commit-gated path still ratifies this region.
                    tracing::debug!(error = %e, "live edge write failed");
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
        if let Some(src) = src {
            // `ref_col` is 0, not the column the editor sent, so this row
            // collides with the save-path pass's row for the same reference and
            // upgrades it in place. Keying on the real column would fork one
            // reference into a stale `pending` row and a `resolved` one.
            states.push(travsr_store::RefResolution {
                src,
                ref_line: r.ref_line,
                ref_col: 0,
                name: r.name.clone(),
                state: if claimed.is_some() {
                    "resolved"
                } else {
                    "pending"
                },
                resolved_dst: claimed,
            });
        }
    }
    if let Err(e) = store.upsert_ref_resolution_states(&states) {
        // Losing the claim costs the meter its evidence, never the graph its
        // correctness.
        tracing::debug!(error = %e, "recording editor-lane ref_resolution_state failed");
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

/// RFC-027 section 11: keep only the resolutions that answer a question the
/// daemon is still asking.
///
/// The editor reports `(ref_line, name, edge_kind)` triples it resolved from a
/// target list the daemon produced earlier. `targets` is that list recomputed
/// against the file **as the daemon now parses it**, so a reference that has
/// moved, changed name, or stopped existing is simply absent and its answer is
/// dropped.
///
/// This is what consumes the intent behind the protocol's `buffer_version`.
/// The version number itself cannot be the guard: it is the editor's counter,
/// minted and checked by the editor, and the window it cannot see is the one
/// where the *daemon* re-parsed the file (a change on disk under an unchanged
/// buffer) between handing out targets and receiving answers. In that window
/// `enclosing_definition_at` and `enclosing_node_at` read spans from the newer
/// parse and a resolution computed against the older text can land on a node
/// that has moved. Comparing against the daemon's own current reference set
/// answers exactly that question with evidence the daemon owns.
///
/// It also narrows the trust surface: an editor can no longer volunteer a
/// position the daemon never asked about.
pub fn retain_current_targets(
    resolutions: &[LiveResolution],
    targets: &[LiveResolutionTarget],
) -> Vec<LiveResolution> {
    let asked: std::collections::HashSet<(u32, &str, &str)> = targets
        .iter()
        .map(|t| (t.ref_line, t.name.as_str(), t.edge_kind.as_str()))
        .collect();
    resolutions
        .iter()
        .filter(|r| asked.contains(&(r.ref_line, r.name.as_str(), r.edge_kind.as_str())))
        .cloned()
        .collect()
}

/// Map one editor resolution to an edge, or `None` to abstain.
fn resolve_one(store: &SqliteStore, corpus: &str, file: &str, r: &LiveResolution) -> Option<Edge> {
    // The editor names the edge kind it resolved (RFC-027 live edge-kind scope).
    // Restricted to the Bucket-B kinds the lane may emit, so a malformed or
    // hostile report cannot make it write a kind it was never scoped to.
    let edge = live_edge_kind(&r.edge_kind)?;
    // The reference's enclosing definition is the edge's source. A reference at
    // top level (no enclosing function) has no caller node to attach to.
    let src = store
        .enclosing_definition_at(corpus, file, r.ref_line)
        .ok()
        .flatten()?;
    // Section 7.5: the node the editor pointed at, restricted to the kinds valid
    // for this edge kind (a field ref lands on a `field` node, an implements
    // clause on an interface/trait, a call on a definition). The kind set is the
    // gate: a target of the wrong kind, or a position in no matching span (a
    // node_modules file, a generated stub, an unindexed file), maps to nothing
    // and abstains (§8.1).
    let dst = match store
        .enclosing_node_at(corpus, &r.target_path, r.target_line, target_kinds(edge))
        .ok()
        .flatten()
    {
        Some(dst) => dst,
        // Issue #816 defect 2 backstop: a provider that reports the item's full
        // range (rust-analyzer's `targetRange`) or a bare `Location` puts the
        // definition line on a leading doc comment, attribute, or decorator,
        // above the node's declaration, so exact span containment misses. Map
        // such a position to the definition it heads. Language-server-agnostic:
        // it needs no `targetSelectionRange`, so a server that supplies only a
        // range still resolves. The name gate below still guards precision, so a
        // wrong header match abstains.
        None => store
            .node_starting_at_or_below(corpus, &r.target_path, r.target_line, target_kinds(edge))
            .ok()
            .flatten()?,
    };
    // Does the node the editor landed on actually carry the name it said it was
    // resolving? Nothing above checks this: the gates are edge kind, target node
    // kind, span containment and corpus equality, none of which notice a
    // provider that jumped to an unrelated symbol, a resolution computed against
    // a stale buffer, or a column recovered from the wrong occurrence on the
    // line. One signature-leaf comparison on a node already fetched catches all
    // three, so it is the cheapest gate in the lane.
    if !names_match(store, dst, &r.name) {
        return None;
    }
    edge_if_sound(store, src, dst, edge)
}

/// True when `dst`'s signature plausibly names `name`.
///
/// Signatures are `kind:Leaf` or `kind:Qual.Leaf`, and `leaf_of` returns the
/// last segment of either, which is normally the identifier the editor was asked
/// about. A node that cannot be read fails the check rather than passing it: an
/// unreadable target is exactly the case to abstain on.
///
/// One shape legitimately differs: a **constructor call** names its *type*
/// (`new Thing()` in Java/C#, `Thing()` in Swift/Dart, all captured by the
/// detector as a `ref/call` to `Thing`), while a provider may jump to the
/// initialiser rather than the type declaration — `method:Thing.init`, whose
/// leaf is `init`. So a constructor leaf also matches on the qualifier. This is
/// the only relaxation: the target must still carry the reported name somewhere
/// in its own signature.
fn names_match(store: &SqliteStore, dst: NodeId, name: &str) -> bool {
    /// Leaves a provider may land on for a call written as the type's name.
    const CONSTRUCTOR_LEAVES: &[&str] = &["init", "new", "constructor"];

    let Some(node) = store.get_node(dst).ok().flatten() else {
        return false;
    };
    let sig = &node.vname.signature;
    let leaf = travsr_core::ident::leaf_of(sig);
    if leaf == name {
        return true;
    }
    if !CONSTRUCTOR_LEAVES.contains(&leaf) {
        return false;
    }
    // The qualifier: `Thing` in `method:Thing.init`.
    sig.split_once(':')
        .map(|(_, body)| body)
        .unwrap_or(sig)
        .rsplit_once('.')
        .is_some_and(|(qual, _)| qual.rsplit('.').next().unwrap_or(qual) == name)
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
///
/// `locally_bound` is section 7.3's **step 1**, and step 3a needs both halves:
/// `|candidates| == 1` **and** local-clean. It holds the names this file binds
/// to a local variable or a parameter
/// ([`travsr_analysis::live_scope::local_binding_names`]); a bare-identifier
/// reference to one of them is not a free reference and must not be resolved
/// repo-wide, however unique the name happens to be elsewhere. Uniqueness alone
/// was the reported defect: a local `const save = () => 1` is not indexed by
/// Phase A, so `fn:save` had exactly one definition in the graph and the floor
/// pointed the call at an unrelated function in another file.
pub fn resolve_unambiguous_lexical(
    store: &mut SqliteStore,
    corpus: &str,
    path: &str,
    unresolved: &[UnresolvedCall],
    inheritance: &[InheritanceRef],
    locally_bound: &std::collections::HashSet<String>,
) -> LiveOutcome {
    let mut outcome = LiveOutcome::default();
    // Section 9.2: every reference this pass saw, and what became of it. An
    // abstention is recorded, not dropped — "there is a call here and its
    // target is not yet known" is true and useful, and saying it is what makes
    // fail-closed honest rather than merely quiet.
    //
    // Calls and inheritance clauses share one `states` vector because
    // `replace_ref_resolution_states` clears the whole file's rows before
    // inserting: recording them in two passes would make the second wipe the
    // first. One pass, one replace.
    let mut states: Vec<travsr_store::RefResolution> =
        Vec::with_capacity(unresolved.len() + inheritance.len());

    for call in unresolved {
        let name = travsr_core::ident::leaf_of(&call.callee_sig).to_string();
        // The target this lane claimed, kept whatever later happens to the edge.
        // Section 12's meter needs the claim itself: by ratification a re-derived
        // live edge has been relabelled and an unratified one is about to be
        // swept, so the edges table can no longer say what the lane decided.
        let claimed = match lexical_one(store, call, locally_bound) {
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

    // RFC-027 live edge-kind scope: the IsImplementation floor. An `extends` /
    // `implements` clause whose base type has exactly one definition repo-wide
    // resolves with no language server, exactly as the call floor above does. An
    // ambiguous or cross-file-only base abstains and becomes an editor target.
    for r in inheritance {
        // The clause sits on the implementing class's own declaration, so the
        // edge source is the definition enclosing that line.
        let src = store
            .enclosing_definition_at(corpus, path, r.line)
            .ok()
            .flatten();
        let claimed =
            match src.and_then(|src| inheritance_edge(store, src, &r.base_name, locally_bound)) {
                Some(edge) => match store.put_edge_live(&edge) {
                    Ok(()) => Some(edge.dst),
                    Err(e) => {
                        tracing::debug!(error = %e, "live inheritance edge write failed");
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
        if let Some(src) = src {
            states.push(travsr_store::RefResolution {
                src,
                ref_line: r.line,
                ref_col: 0,
                name: r.base_name.clone(),
                state: if claimed.is_some() {
                    "resolved"
                } else {
                    "pending"
                },
                resolved_dst: claimed,
            });
        }
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

fn lexical_one(
    store: &SqliteStore,
    call: &UnresolvedCall,
    locally_bound: &std::collections::HashSet<String>,
) -> Option<Edge> {
    let edge = lexical_edge_kind(call);
    // Section 7.3 step 1: a bare identifier that this file also binds to a local
    // or a parameter is not a free reference, so no repo-wide lookup is entitled
    // to answer it. Only the bare-identifier shape is gated: a method or field
    // reference resolves through a recovered receiver *type*, which a local
    // binding of the member name cannot shadow.
    if !call.is_method_call
        && !call.callee_sig.starts_with("field:")
        && locally_bound.contains(travsr_core::ident::leaf_of(&call.callee_sig))
    {
        return None;
    }
    let dst = candidate_signatures(call)
        .into_iter()
        .find_map(|sig| unique_definition(store, &sig, edge))?;
    edge_if_sound(store, call.src, dst, edge)
}

/// The edge kind a native call-site record resolves to. The extractor encodes a
/// field access as `field:...` (with a recovered receiver type); everything else
/// it emits is a call. A field read must become `ref/field`, never `ref/call`,
/// so it never surfaces as a caller in `get_callers` / `get_blast_radius` (#757)
/// while still appearing as a use site in `find_references`.
fn lexical_edge_kind(call: &UnresolvedCall) -> EdgeKind {
    if call.callee_sig.starts_with("field:") {
        EdgeKind::RefField
    } else {
        EdgeKind::RefCall
    }
}

/// RFC-027 daemon-driven positions: the references the editor should resolve via
/// a language provider, from the native extractor's reference set.
///
/// Fail-closed and surgical, the two properties §7.6 asks of the LSP lane:
///
/// - **A reference the lexical lane can settle is skipped.** `lexical_one`
///   already resolves it repo-wide with no server, so sending it to the editor
///   would waste a provider round trip on an answer the save-path lane produces
///   deterministically. The two lanes therefore partition the reference set with
///   no overlap: lexical takes the unambiguous ones, the editor takes the rest.
/// - **Only method/field references go to the editor.** A bare free-function or
///   constructor call the lexical lane could not resolve has no unique definition
///   in the graph; the editor's provider would resolve it to that same
///   (missing-or-ambiguous) target and the daemon would abstain anyway. Method
///   and field references on a receiver whose type the extractor could not
///   recover are exactly the disambiguation §7.3b exists for.
///
/// The editor is handed the line and the name, not a column: the native
/// extractor records the reference's line but not its column, and the editor can
/// recover the column by finding `name` on the line far more cheaply than the
/// extractor could be taught to carry UTF-16 offsets.
pub fn targets_needing_editor(
    store: &SqliteStore,
    lines: &[&str],
    unresolved: &[UnresolvedCall],
    locally_bound: &std::collections::HashSet<String>,
) -> Vec<LiveResolutionTarget> {
    let targets: Vec<LiveResolutionTarget> = unresolved
        .iter()
        .filter_map(|call| {
            // The lexical lane owns anything it can resolve without a server.
            if lexical_one(store, call, locally_bound).is_some() {
                return None;
            }
            // Only a method or field reference benefits from LSP disambiguation.
            let is_field = call.callee_sig.starts_with("field:");
            if !(call.is_method_call || is_field) {
                return None;
            }
            // No line means no position for the editor to query (older
            // extractors record `0`); skip rather than send an unusable target.
            if call.caller_line == 0 {
                return None;
            }
            // RFC-027 #813 P2: prefer the extractor's exact occurrence column,
            // converted from a byte offset to the editor's UTF-16 column. It
            // points at the precise identifier the extractor captured, so it is
            // right even when the name repeats on the line, where the
            // `fill_target_columns` name search would pick the first match and
            // could resolve the wrong occurrence. `None` leaves it for the name
            // search fallback.
            let ref_col = call.caller_col.and_then(|bc| {
                lines
                    .get((call.caller_line - 1) as usize)
                    .map(|line| byte_to_utf16_col(line, bc))
            });
            Some(LiveResolutionTarget {
                ref_line: call.caller_line,
                ref_col,
                name: travsr_core::ident::leaf_of(&call.callee_sig).to_string(),
                edge_kind: lexical_edge_kind(call).as_str().to_string(),
                // Calls and fields are both answered by the definition provider;
                // the implementation provider is for implements/override targets,
                // which arrive through a different detector.
                provider: "definition".to_string(),
            })
        })
        .collect();
    drop_same_position_collisions(targets)
}

/// RFC-027 daemon-driven positions, IsImplementation half: the `extends` /
/// `implements` clauses the lexical floor could not settle, as editor targets.
///
/// A clause whose base type is unique repo-wide is the lexical floor's job
/// (`resolve_unambiguous_lexical`), so it is skipped here for the same
/// no-overlap reason the call path skips resolvable calls. What remains — an
/// ambiguous base, or one defined only in another file — is what the editor's
/// **definition** provider resolves: the reference names the base type, and we
/// want where that type is *defined* (the class/interface node), not its
/// implementors, so the provider is `definition`, not `implementation`.
pub fn inheritance_targets_needing_editor(
    store: &SqliteStore,
    corpus: &str,
    path: &str,
    inheritance: &[InheritanceRef],
    locally_bound: &std::collections::HashSet<String>,
) -> Vec<LiveResolutionTarget> {
    let targets: Vec<LiveResolutionTarget> = inheritance
        .iter()
        .filter_map(|r| {
            let src = store.enclosing_definition_at(corpus, path, r.line).ok()??;
            // The lexical floor owns any base with a unique repo-wide definition.
            if inheritance_edge(store, src, &r.base_name, locally_bound).is_some() {
                return None;
            }
            Some(LiveResolutionTarget {
                ref_line: r.line,
                ref_col: None,
                name: r.base_name.clone(),
                edge_kind: EdgeKind::IsImplementation.as_str().to_string(),
                provider: "definition".to_string(),
            })
        })
        .collect();
    drop_same_position_collisions(targets)
}

/// RFC-027 section 8.2/8.3: editor targets for a language with no native
/// Phase B extractor (Go, Java, C#, C/C++, and the rest of the sixteen).
///
/// Where the native path partitions its reference set between two lanes — the
/// lexical floor takes what it can settle repo-wide, the editor takes the rest —
/// there is nothing to partition here. The generic detector deliberately
/// recovers no receiver type and builds no signature key, so
/// [`resolve_unambiguous_lexical`] has nothing to match on and never runs for
/// these languages. Every detected reference is therefore an editor target:
/// filtering one out on a guess about what the floor "might" have resolved would
/// drop the reference outright rather than hand it to the other lane.
///
/// With no language server installed nothing is answered and every reference
/// abstains, which is exactly section 7.3c — today's commit-gated behavior, zero
/// regression.
///
/// References that collide on `(line, name, kind)` are **dropped**, not
/// collapsed — see [`drop_same_position_collisions`].
pub fn generic_targets_needing_editor(
    refs: &travsr_analysis::live_detect::LiveRefs,
) -> Vec<LiveResolutionTarget> {
    let mut out = Vec::with_capacity(refs.calls.len() + refs.fields.len());
    let mut push = |line: u32, name: &str, kind: EdgeKind| {
        // A reference with no line has no position for the editor to query.
        if line == 0 {
            return;
        }
        out.push(LiveResolutionTarget {
            ref_line: line,
            ref_col: None,
            name: name.to_string(),
            edge_kind: kind.as_str().to_string(),
            // All three kinds resolve to where the target is *defined*, which is
            // the definition provider. `implementation` answers the opposite
            // question (who implements this) and is not what any of these want.
            provider: "definition".to_string(),
        });
    };
    for c in &refs.calls {
        push(c.line, &c.name, EdgeKind::RefCall);
    }
    for f in &refs.fields {
        push(f.line, &f.name, EdgeKind::RefField);
    }
    for i in &refs.inheritance {
        push(i.line, &i.base_name, EdgeKind::IsImplementation);
    }
    drop_same_position_collisions(out)
}

/// Drop every target the editor could not tell apart from another.
///
/// A target names a line and a name, not a column, and the editor recovers the
/// column by finding the first whole-word match of that name on that line. Two
/// references sharing `(line, name, edge_kind)` therefore both resolve to the
/// *first* occurrence's position, so a provider that answers about one of them
/// is answering about the other as well.
///
/// The earlier version collapsed such a group to its first member on the
/// reasoning that "two references to the same name on one line resolve to the
/// same position". That holds only when they are the same reference. For
/// `a.save(); b.save();` with different receiver types there are two references,
/// two positions and two correct targets, and keeping one silently attributes
/// the first occurrence's answer to both — a wrong edge, which is the one
/// outcome section 8.1 does not permit. The same happens when the name appears
/// earlier on the line inside a string or a comment.
///
/// So the whole group is dropped rather than resolved. The references stay
/// `pending`, which is honest, and the commit-gated path resolves them. Carrying
/// the real column through the protocol would recover this recall — the
/// detectors have it at capture time — but it is a protocol change, and until
/// then abstaining is the only fail-closed option.
/// RFC-027 #813 P1: pin each target's 0-based column against the file text, so
/// the editor resolves at the exact position instead of searching the line for
/// `name`.
///
/// Mirrors the extension's `\bname\b` search and its column unit: `vscode.Position`
/// counts UTF-16 code units, and JS `\b` (no `u` flag) is ASCII, so a
/// daemon-pinned column and the editor's own fallback search agree byte for byte.
/// A name the daemon cannot pin as a whole word on its line is left `None`, and
/// the editor falls back to its own search, which abstains the same way. `lines`
/// is the current file text split by line; a target whose line is out of range
/// is left as is.
pub fn fill_target_columns(lines: &[&str], targets: &mut [LiveResolutionTarget]) {
    for t in targets.iter_mut() {
        // A target the extractor already pinned to its exact occurrence column
        // keeps it; the name search is only the fallback for the rest.
        if t.ref_col.is_some() || t.ref_line == 0 {
            continue;
        }
        if let Some(line) = lines.get((t.ref_line - 1) as usize) {
            t.ref_col = word_boundary_col_utf16(line, &t.name);
        }
    }
}

/// Convert a 0-based byte offset within `line` to the 0-based UTF-16 column the
/// editor's `vscode.Position` expects (RFC-027 #813 P2). Equal for ASCII, which
/// identifiers are; a byte offset landing inside a multibyte char is clamped
/// back to the previous char boundary, and one past the end clamps to the line
/// length, so the result is always a valid column.
fn byte_to_utf16_col(line: &str, byte_col: u32) -> u32 {
    let mut b = (byte_col as usize).min(line.len());
    while b > 0 && !line.is_char_boundary(b) {
        b -= 1;
    }
    line[..b].encode_utf16().count() as u32
}

/// The 0-based UTF-16 column of the first whole-word (`\bname\b`, ASCII word
/// boundary) occurrence of `name` in `line`, or `None`.
fn word_boundary_col_utf16(line: &str, name: &str) -> Option<u32> {
    if name.is_empty() {
        return None;
    }
    let bytes = line.as_bytes();
    let nb = name.as_bytes();
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut i = 0usize;
    while i + nb.len() <= bytes.len() {
        if &bytes[i..i + nb.len()] == nb && line.is_char_boundary(i) {
            let before_ok = i == 0 || !is_word(bytes[i - 1]);
            let after = i + nb.len();
            let after_ok = after >= bytes.len() || !is_word(bytes[after]);
            if before_ok && after_ok {
                return Some(line[..i].encode_utf16().count() as u32);
            }
        }
        i += 1;
    }
    None
}

fn drop_same_position_collisions(targets: Vec<LiveResolutionTarget>) -> Vec<LiveResolutionTarget> {
    // `ref_col` is part of the key: two occurrences of one name on one line
    // (`a.save(); b.save()`) each carry a distinct column now, so they are
    // distinct positions and both are kept. Two with the same column, or both
    // with no column (`None`), are a genuine collision the editor could not
    // disambiguate and still collapse, exactly as before.
    let mut counts: std::collections::HashMap<(u32, Option<u32>, &str, &str), usize> =
        std::collections::HashMap::new();
    for t in &targets {
        *counts
            .entry((t.ref_line, t.ref_col, t.name.as_str(), t.edge_kind.as_str()))
            .or_default() += 1;
    }
    let unique: std::collections::HashSet<(u32, Option<u32>, String, String)> = counts
        .into_iter()
        .filter(|(_, n)| *n == 1)
        .map(|((line, col, name, kind), _)| (line, col, name.to_string(), kind.to_string()))
        .collect();
    targets
        .into_iter()
        .filter(|t| unique.contains(&(t.ref_line, t.ref_col, t.name.clone(), t.edge_kind.clone())))
        .collect()
}

/// RFC-027 #813 P2: merge a saved file's changed-definition committed
/// occurrences into its native/generic editor targets.
///
/// These reach references the tree-sitter live extractor never detects (macro,
/// desugared, trait-dispatched) that the committed SCIP occurrence set did
/// capture, resolved at their exact stored column. Precision is safe by
/// construction: the editor resolves the CURRENT buffer position and the daemon
/// maps the result to a SCIP-owned node (section 8.2 fencing), so a stale
/// position is fail-closed (it resolves current code or nothing), never a
/// fabricated target.
///
/// Each occurrence is bounded to its changed definition's CURRENT span (the body
/// changed, so an occurrence whose remapped line drifted outside it is no longer
/// trusted), given the editor's UTF-16 column from its stored byte column (or
/// left for the editor's name search when it has none), dropped when a native
/// target already covers its `(line, name, kind)` (the native lane owns it), and
/// finally deduped within the enumerated set the same way the native lane is.
pub fn merge_changed_occurrence_targets(
    store: &SqliteStore,
    lines: &[&str],
    native: Vec<LiveResolutionTarget>,
    stashed: &[travsr_core::ChangedOccurrence],
) -> Vec<LiveResolutionTarget> {
    if stashed.is_empty() {
        return native;
    }
    let src_ids: Vec<travsr_core::NodeId> = {
        let mut v: Vec<travsr_core::NodeId> = stashed.iter().map(|o| o.src).collect();
        v.sort_unstable_by_key(|id| id.0);
        v.dedup();
        v
    };
    let spans = store.current_spans(&src_ids).unwrap_or_default();
    let native_positions: std::collections::HashSet<(u32, String, String)> = native
        .iter()
        .map(|t| (t.ref_line, t.name.clone(), t.edge_kind.clone()))
        .collect();
    let enumerated: Vec<LiveResolutionTarget> = stashed
        .iter()
        .filter_map(|occ| {
            // Bound to the definition's current span; drop an occurrence that
            // drifted outside it (an unknown end is a start-only lower bound).
            let &(start, end) = spans.get(&occ.src)?;
            if occ.line < start || end.is_some_and(|e| occ.line > e) {
                return None;
            }
            // The native lane already owns this position; do not double-count it.
            if native_positions.contains(&(occ.line, occ.name.clone(), occ.kind.clone())) {
                return None;
            }
            let ref_col = occ.col.and_then(|bc| {
                lines
                    .get((occ.line - 1) as usize)
                    .map(|l| byte_to_utf16_col(l, bc))
            });
            Some(LiveResolutionTarget {
                ref_line: occ.line,
                ref_col,
                name: occ.name.clone(),
                // Calls and fields are both answered by the definition provider.
                edge_kind: occ.kind.clone(),
                provider: "definition".to_string(),
            })
        })
        .collect();
    // Enumerated occurrences first: the editor drains this list in order up to
    // its per-save budget, and the P2 enumeration is the whole point of the pass
    // (references tree-sitter never detected). A native target that lands inside
    // a preserved definition is a no-op for the graph (`put_edge_live` only
    // touches `live` rows, never the preserved committed edge), so it must never
    // crowd the enumerated set out of the budget on a large file.
    let mut out = drop_same_position_collisions(enumerated);
    out.extend(native);
    out
}

/// The unique `IsImplementation` edge from `src` (the implementing class) to the
/// base type named `base_name`, or `None` to abstain.
fn inheritance_edge(
    store: &SqliteStore,
    src: NodeId,
    base_name: &str,
    locally_bound: &std::collections::HashSet<String>,
) -> Option<Edge> {
    // Same step-1 gate as the call floor: `class Foo extends Bar` where this
    // file also binds `Bar` to a local is not a free reference to the repo-wide
    // `Bar`.
    if locally_bound.contains(base_name) {
        return None;
    }
    let dst = unique_base_definition(store, base_name)?;
    edge_if_sound(store, src, dst, EdgeKind::IsImplementation)
}

/// The single class / interface / trait / … named `base_name` repo-wide, or
/// `None` when there are zero or more than one.
///
/// Tries the exact signature under each kind valid for an `IsImplementation`
/// target (`class:Foo`, `interface:Foo`, …), which is how Phase A keys these
/// nodes, and requires exactly one match across all of them. Two definitions of
/// the same name — the cross-file ambiguity the editor's provider exists for —
/// abstain rather than guess, the same precision guarantee as the call floor.
fn unique_base_definition(store: &SqliteStore, base_name: &str) -> Option<NodeId> {
    let kinds = target_kinds(EdgeKind::IsImplementation);
    let mut found: Option<NodeId> = None;
    for kind in kinds {
        let sig = format!("{kind}:{base_name}");
        let Ok(nodes) = store.lookup_nodes_exact(&sig, None) else {
            continue;
        };
        for n in nodes {
            if !kinds.contains(&n.kind.as_str()) {
                continue;
            }
            match found {
                Some(id) if id != n.id => return None,
                Some(_) => {}
                None => found = Some(n.id),
            }
        }
    }
    found
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

/// The single node named by `signature` and valid as a `edge` target, or `None`
/// when there are zero or more than one.
///
/// `lookup_nodes_exact(sig, None)` documents exactly this contract: one row is
/// an unambiguous match, more than one is "genuinely ambiguous; caller must
/// disambiguate". Candidates whose kind is not valid for `edge` are filtered out
/// first, so the count is over real targets. Ambiguity abstains, which is the
/// whole precision guarantee of section 7.3a.
fn unique_definition(store: &SqliteStore, signature: &str, edge: EdgeKind) -> Option<NodeId> {
    let candidates = store.lookup_nodes_exact(signature, None).ok()?;
    let kinds = target_kinds(edge);
    let defs: Vec<&travsr_core::Node> = candidates
        .iter()
        .filter(|n| kinds.contains(&n.kind.as_str()))
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
///
/// `kind` is the edge kind to emit (RFC-027 live edge-kind scope): the lane is
/// no longer call-only, so the caller names it. The target-kind gate lives in
/// [`target_kinds`], applied before this by whichever lane found `dst`.
fn edge_if_sound(store: &SqliteStore, src: NodeId, dst: NodeId, kind: EdgeKind) -> Option<Edge> {
    if src == dst {
        return None;
    }
    let src_node = store.get_node(src).ok().flatten()?;
    let dst_node = store.get_node(dst).ok().flatten()?;
    if src_node.vname.corpus != dst_node.vname.corpus {
        return None;
    }
    Some(Edge::new(src, dst, kind))
}

/// The edge kinds the live lane is permitted to emit. Anything else — a
/// structural kind, a cross-language `ffi/call`, an unknown string — is refused
/// so a malformed or hostile editor report cannot make the lane write an edge
/// kind it was never scoped to.
///
/// Exactly the three kinds the lane actually ships. `Overrides` is **ruled out**
/// (no single fail-closed LSP call resolves it) and `RefImports` is **deferred**,
/// so admitting them here would have let an editor report get an edge written
/// for a kind the RFC says cannot be resolved fail-closed — the precise thing
/// this accept-list exists to stop. They earn a place here when they are scoped,
/// not before.
fn live_edge_kind(s: &str) -> Option<EdgeKind> {
    let kind = EdgeKind::from_str(s)?;
    matches!(
        kind,
        EdgeKind::RefCall | EdgeKind::RefField | EdgeKind::IsImplementation
    )
    .then_some(kind)
}

/// The node kinds a live edge of `edge` may point at. This is the whole
/// precision gate on the *target*: a candidate of any other kind is filtered out
/// and the reference abstains rather than mint an edge to the wrong node kind
/// (§8.1). The *source* is always a code body, gated separately by
/// `enclosing_definition_at`.
fn target_kinds(edge: EdgeKind) -> &'static [&'static str] {
    match edge {
        EdgeKind::RefCall => DEFINITION_KINDS,
        EdgeKind::RefField => &["field"],
        // A named import specifier resolves to the exported definition it binds.
        EdgeKind::RefImports => DEFINITION_KINDS,
        // `class C implements I` / `impl Trait for T` points at the contract.
        EdgeKind::IsImplementation => &["interface", "trait", "class", "struct", "protocol"],
        // A subclass method overriding a base method points at the base method.
        EdgeKind::Overrides => &["method", "fn", "function"],
        // The live lane emits no other kind; `live_edge_kind` already refused it.
        _ => &[],
    }
}

/// Kinds a call / import reference can resolve to. Mirrors the store's
/// `ENCLOSING_DEFINITION_KINDS` and `definition_node_ids_in_file`, so all three
/// agree on what counts as a definition. Deliberately excludes `field`, which is
/// a `ref/field` target, not a `ref/call` one.
const DEFINITION_KINDS: &[&str] = &[
    "function",
    "method",
    "fn",
    "class",
    "interface",
    "struct",
    "trait",
    "enum",
    "type",
    "typedef",
    "union",
    "object",
    "protocol",
    "mixin",
    "extension",
    "namespace",
    "init",
];

#[cfg(test)]
mod tests {
    use super::*;
    use travsr_core::{Node, VName};

    const CORPUS: &str = "testrepo";

    #[test]
    fn fill_target_columns_pins_word_boundary_positions() {
        let content = "fn a() {\n    self.save(x);\n    let save = 1;\n}\n";
        let mut targets = vec![
            LiveResolutionTarget {
                ref_line: 2,
                ref_col: None,
                name: "save".to_string(),
                edge_kind: "ref/call".to_string(),
                provider: "definition".to_string(),
            },
            // A name that is not present on its line stays None; the editor
            // falls back to its own search, which abstains the same way.
            LiveResolutionTarget {
                ref_line: 2,
                ref_col: None,
                name: "missing".to_string(),
                edge_kind: "ref/call".to_string(),
                provider: "definition".to_string(),
            },
        ];
        let lines: Vec<&str> = content.lines().collect();
        fill_target_columns(&lines, &mut targets);
        // "    self.save(x);" - `save` starts at column 9 (0-based), after the
        // word boundary at the dot, not inside `self`.
        assert_eq!(targets[0].ref_col, Some(9));
        assert_eq!(targets[1].ref_col, None);
    }

    #[test]
    fn merge_changed_occurrence_targets_enumerates_bounds_and_dedups() {
        use travsr_core::ChangedOccurrence;
        // A caller def spanning lines 5-9. Its committed occurrences are what the
        // live lane enumerates as editor targets after a body edit.
        let store = store_with(&[("a.ts", "fn:caller", "function", 5, 9)]);
        let src = node_id("a.ts", "fn:caller");
        // Line 6 has a non-ASCII prefix, so the stored byte column and the
        // editor's UTF-16 column genuinely differ: `bar` is at byte col 8 but
        // UTF-16 col 7 (the accented `é` is two bytes, one UTF-16 unit). A wrong
        // conversion here is a wrong editor position.
        // Lines 5-9 are the function body; line 6 carries the accented call.
        let content = "\n\n\n\nfn caller() {\n  café.bar();\n  qux();\n}\n\n";
        let stashed = vec![
            // In-span, with a column: enumerated at the exact UTF-16 position.
            ChangedOccurrence {
                src,
                line: 6,
                col: Some(8),
                kind: "ref/call".into(),
                name: "bar".into(),
            },
            // Out of the def's current span (line 20 > end 9): dropped.
            ChangedOccurrence {
                src,
                line: 20,
                col: Some(2),
                kind: "ref/call".into(),
                name: "gone".into(),
            },
            // In-span but a position the native lane already owns: dropped so it
            // is not double-counted (native target retained below).
            ChangedOccurrence {
                src,
                line: 7,
                col: Some(2),
                kind: "ref/call".into(),
                name: "qux".into(),
            },
        ];
        let native = vec![LiveResolutionTarget {
            ref_line: 7,
            ref_col: Some(2),
            name: "qux".to_string(),
            edge_kind: "ref/call".to_string(),
            provider: "definition".to_string(),
        }];

        let lines: Vec<&str> = content.lines().collect();
        let out = merge_changed_occurrence_targets(&store, &lines, native, &stashed);

        // The native target is kept; the enumerated `bar` is added at its UTF-16
        // column; the out-of-span and native-owned occurrences are dropped.
        assert_eq!(out.len(), 2, "native target plus one enumerated occurrence");
        assert!(out
            .iter()
            .any(|t| t.name == "qux" && t.ref_line == 7 && t.provider == "definition"));
        let bar = out
            .iter()
            .find(|t| t.name == "bar")
            .expect("bar occurrence enumerated");
        assert_eq!(bar.ref_line, 6);
        assert_eq!(
            bar.ref_col,
            Some(7),
            "byte col 8 must convert to UTF-16 col 7 across the accented prefix"
        );
        assert_eq!(bar.edge_kind, "ref/call");
    }

    #[test]
    fn word_boundary_col_is_utf16_and_whole_word_only() {
        // `saved` must not match the target `save` (no trailing boundary).
        assert_eq!(
            word_boundary_col_utf16("let saved = save()", "save"),
            Some(12)
        );
        // A multibyte prefix: `é` is one UTF-16 code unit, so `x` is at col 2.
        assert_eq!(word_boundary_col_utf16("é x", "x"), Some(2));
        // Substring inside a larger identifier never matches.
        assert_eq!(word_boundary_col_utf16("reservation", "server"), None);
    }

    /// Most tests exercise the uniqueness gate, not the section 7.3 step-1
    /// local-scope gate, so they declare no local bindings. The tests that do
    /// exercise it build their own set.
    fn no_locals() -> std::collections::HashSet<String> {
        std::collections::HashSet::new()
    }

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
            edge_kind: "ref/call".to_string(),
        }
    }

    /// Like [`resolution`] but naming a specific edge kind, for the Bucket-B
    /// kinds beyond `ref/call` (RFC-027 live edge-kind scope).
    fn resolution_kind(
        ref_line: u32,
        name: &str,
        target_path: &str,
        target_line: u32,
        edge_kind: &str,
    ) -> LiveResolution {
        LiveResolution {
            edge_kind: edge_kind.to_string(),
            ..resolution(ref_line, name, target_path, target_line)
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

    /// Issue #816 defect 2 backstop: a provider that reports the item's full
    /// range (or a bare `Location`) puts the definition line on a leading doc
    /// comment or attribute, above the node's declaration. The daemon maps such
    /// a position to the definition it heads and emits, so a server that does not
    /// supply `targetSelectionRange` still resolves.
    #[test]
    fn a_definition_line_just_above_the_node_maps_to_the_node() {
        let mut store = store_with(&[
            ("src/order.ts", "fn:placeOrder", "function", 10, 30),
            ("src/user.ts", "method:User.save", "method", 15, 20),
        ]);
        // The provider reports line 13, a doc comment / attribute two lines above
        // User.save's declaration at 15, which is inside no node span.
        let out = apply_live_resolutions(
            &mut store,
            CORPUS,
            "src/order.ts",
            &[resolution(18, "save", "src/user.ts", 13)],
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
            "a header position above the node must still resolve to the definition",
        );
    }

    /// The backstop is name-gated: a header position above a node whose name does
    /// not match the reported reference abstains rather than attaching.
    #[test]
    fn a_header_position_above_a_mismatched_node_still_abstains() {
        let mut store = store_with(&[
            ("src/order.ts", "fn:placeOrder", "function", 10, 30),
            ("src/user.ts", "method:User.save", "method", 15, 20),
        ]);
        // Line 13 heads User.save, but the reference names `load`, not `save`.
        let out = apply_live_resolutions(
            &mut store,
            CORPUS,
            "src/order.ts",
            &[resolution(18, "load", "src/user.ts", 13)],
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
            &[],
            &no_locals(),
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
            &[],
            &no_locals(),
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

        let out = resolve_unambiguous_lexical(
            &mut store,
            CORPUS,
            "src/order.ts",
            &[c],
            &[],
            &no_locals(),
        );
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
            &[],
            &no_locals(),
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
    /// `recv_type.is_none()`). So `candidate_signatures` builds `field:Zoo.count`
    /// and the field definition node is present.
    ///
    /// RFC-027 live edge-kind scope: a field read now resolves to a `ref/field`
    /// edge, not abstention. The edge kind matters — it must be `ref/field`, never
    /// `ref/call`, so a field read never surfaces as a caller in `get_callers` /
    /// `get_blast_radius` (#757) while still appearing as a use site in
    /// `find_references`. Both the emission and the kind are pinned here.
    #[test]
    fn rust_field_access_resolves_to_a_ref_field_edge() {
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
        assert_eq!(
            lexical_edge_kind(&c),
            EdgeKind::RefField,
            "a field:… call-site record resolves to ref/field, not ref/call",
        );

        let mut store = rust_store_with(&[
            ("src/zoo.rs", "method:Zoo.tally", "method", 4, 9),
            ("src/zoo.rs", "field:Zoo.count", "field", 2, 2),
        ]);
        let out =
            resolve_unambiguous_lexical(&mut store, CORPUS, "src/zoo.rs", &[c], &[], &no_locals());
        assert_eq!(
            out,
            LiveOutcome {
                emitted: 1,
                pending: 0
            },
        );
        let edge = store
            .iter_edges_from(rust_node_id("src/zoo.rs", "method:Zoo.tally"))
            .expect("iter_edges_from")
            .into_iter()
            .find(|e| e.dst == rust_node_id("src/zoo.rs", "field:Zoo.count"))
            .expect("a live edge to the field must exist");
        assert_eq!(
            edge.kind,
            EdgeKind::RefField,
            "the edge kind must be ref/field"
        );
        assert_eq!(
            edge.provenance.as_deref(),
            Some("live"),
            "and it must be tagged live",
        );
    }

    /// RFC-027 section 7.3 **step 1**, the reported defect.
    ///
    /// `export function process() { const save = () => 1; return save(); }`
    /// makes the extractor emit a bare `fn:save` with no receiver, and Phase A
    /// does not index the local `const`. So `fn:save` has exactly one definition
    /// repo-wide — an unrelated function in another file — and uniqueness alone
    /// resolved the local call to it, writing a fabricated `provenance='live'`
    /// `ref/call` edge that `get_callers` / `find_references` /
    /// `get_blast_radius` would show until the next commit swept it.
    #[test]
    fn a_locally_bound_name_is_not_resolved_repo_wide() {
        let mut store = store_with(&[
            ("src/order.ts", "fn:process", "function", 1, 4),
            ("src/unrelated.ts", "fn:save", "function", 1, 3),
        ]);
        let src = node_id("src/order.ts", "fn:process");
        let mut locals = std::collections::HashSet::new();
        locals.insert("save".to_string());

        let out = resolve_unambiguous_lexical(
            &mut store,
            CORPUS,
            "src/order.ts",
            &[call(src, "fn:save", 3)],
            &[],
            &locals,
        );

        assert_eq!(
            out,
            LiveOutcome {
                emitted: 0,
                pending: 1
            },
            "a call to a locally bound name must abstain, not resolve repo-wide"
        );
        assert!(
            store
                .iter_edges_from(src)
                .expect("iter_edges_from")
                .is_empty(),
            "no edge at all may be written for a local reference"
        );
    }

    /// The gate is the *local-clean* half of step 3a, not a replacement for the
    /// uniqueness half: the same call with nothing bound locally still resolves,
    /// so the fix costs no recall on genuinely free references.
    #[test]
    fn a_free_reference_still_resolves_under_the_scope_gate() {
        let mut store = store_with(&[
            ("src/order.ts", "fn:process", "function", 1, 4),
            ("src/unrelated.ts", "fn:save", "function", 1, 3),
        ]);
        let src = node_id("src/order.ts", "fn:process");
        let out = resolve_unambiguous_lexical(
            &mut store,
            CORPUS,
            "src/order.ts",
            &[call(src, "fn:save", 3)],
            &[],
            &no_locals(),
        );
        assert_eq!(
            out,
            LiveOutcome {
                emitted: 1,
                pending: 0
            },
        );
    }

    /// A locally shadowed base type gets the same treatment as a call.
    #[test]
    fn a_locally_bound_base_type_abstains() {
        let mut store = store_with(&[
            ("src/order.ts", "class:Order", "class", 3, 8),
            ("src/base.ts", "class:Base", "class", 1, 5),
        ]);
        let mut locals = std::collections::HashSet::new();
        locals.insert("Base".to_string());
        let out = resolve_unambiguous_lexical(
            &mut store,
            CORPUS,
            "src/order.ts",
            &[],
            &[InheritanceRef {
                base_name: "Base".to_string(),
                line: 3,
            }],
            &locals,
        );
        assert_eq!(
            out,
            LiveOutcome {
                emitted: 0,
                pending: 1
            },
        );
    }

    /// Section 7.3b: the node the editor landed on must actually carry the name
    /// it said it was resolving. Without this a provider that jumped to an
    /// unrelated symbol, a stale-buffer answer, or a column recovered from the
    /// wrong occurrence all pass every other gate.
    #[test]
    fn a_target_whose_name_does_not_match_the_reference_abstains() {
        let mut store = store_with(&[
            ("src/order.ts", "fn:placeOrder", "function", 10, 30),
            ("src/user.ts", "method:User.save", "method", 15, 20),
        ]);
        let out = apply_live_resolutions(
            &mut store,
            CORPUS,
            "src/order.ts",
            // The editor says it resolved `load`, but the position it points at
            // is `User.save`.
            &[resolution(18, "load", "src/user.ts", 17)],
        );
        assert_eq!(
            out,
            LiveOutcome {
                emitted: 0,
                pending: 1
            },
        );
    }

    /// A constructor call names its *type*, and a provider may jump to the
    /// initialiser instead of the type declaration. The name check must not
    /// abstain on that.
    #[test]
    fn a_constructor_target_matches_on_the_type_name() {
        let mut store = store_with(&[
            ("src/order.ts", "fn:placeOrder", "function", 10, 30),
            ("src/thing.ts", "method:Thing.init", "method", 15, 20),
        ]);
        let out = apply_live_resolutions(
            &mut store,
            CORPUS,
            "src/order.ts",
            &[resolution(18, "Thing", "src/thing.ts", 17)],
        );
        assert_eq!(
            out,
            LiveOutcome {
                emitted: 1,
                pending: 0
            },
        );
    }

    /// Section 11: an answer to a question the daemon is no longer asking is
    /// dropped, which is what the protocol's `buffer_version` cannot do on its
    /// own (it is the editor's counter, so it cannot see a daemon re-parse).
    #[test]
    fn a_resolution_the_current_parse_no_longer_asks_for_is_dropped() {
        let targets = vec![LiveResolutionTarget {
            ref_line: 18,
            ref_col: None,
            name: "save".to_string(),
            edge_kind: "ref/call".to_string(),
            provider: "definition".to_string(),
        }];
        let reported = vec![
            resolution(18, "save", "src/user.ts", 17),
            // The reference moved a line since the targets were handed out.
            resolution(21, "save", "src/user.ts", 17),
        ];
        let kept = retain_current_targets(&reported, &targets);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].ref_line, 18);
    }

    /// Section 7.3b for a field target: an editor resolves `zoo.count` to the
    /// field's declaration line, and the daemon maps that position to the
    /// `field` node via `enclosing_node_at` (which the call-target kind set of
    /// `enclosing_definition_at` would have excluded) and emits `ref/field`.
    #[test]
    fn an_editor_field_resolution_becomes_a_ref_field_edge() {
        let mut store = rust_store_with(&[
            ("src/zoo.rs", "method:Zoo.tally", "method", 4, 9),
            ("src/zoo.rs", "field:Zoo.count", "field", 2, 2),
        ]);
        let out = apply_live_resolutions(
            &mut store,
            CORPUS,
            "src/zoo.rs",
            // The reference is on line 5 (inside tally); the field decl is line 2.
            &[resolution_kind(5, "count", "src/zoo.rs", 2, "ref/field")],
        );
        assert_eq!(
            out,
            LiveOutcome {
                emitted: 1,
                pending: 0
            },
        );
        let edge = store
            .iter_edges_from(rust_node_id("src/zoo.rs", "method:Zoo.tally"))
            .expect("iter_edges_from")
            .into_iter()
            .find(|e| e.dst == rust_node_id("src/zoo.rs", "field:Zoo.count"))
            .expect("a live edge to the field must exist");
        assert_eq!(edge.kind, EdgeKind::RefField);
    }

    /// RFC-027 daemon-driven positions: the editor is handed exactly the
    /// references the lexical lane could not settle, and no others. A resolvable
    /// call is the lexical lane's job; a method call with no recovered receiver
    /// type is the disambiguation §7.3b exists for, so only the latter becomes a
    /// target — carrying the line, the leaf name, its edge kind, and the
    /// definition provider.
    #[test]
    fn targets_are_only_the_references_the_lexical_lane_cannot_settle() {
        let store = store_with(&[
            ("src/order.ts", "fn:placeOrder", "function", 10, 30),
            ("src/user.ts", "method:User.save", "method", 15, 20),
        ]);
        let src = node_id("src/order.ts", "fn:placeOrder");

        // Resolvable associated call — lexical owns it, so no editor target.
        let resolvable = call(src, "method:User.save", 18);
        // Method call with no recovered receiver type — lexical abstains.
        let mut ambiguous = call(src, "fn:save", 19);
        ambiguous.is_method_call = true;

        let targets = targets_needing_editor(&store, &[], &[resolvable, ambiguous], &no_locals());
        assert_eq!(
            targets.len(),
            1,
            "only the unsettleable reference is a target"
        );
        assert_eq!(targets[0].ref_line, 19);
        assert_eq!(targets[0].name, "save");
        assert_eq!(targets[0].edge_kind, "ref/call");
        assert_eq!(targets[0].provider, "definition");
    }

    /// A field access with no recovered receiver type becomes a `ref/field`
    /// target for the definition provider — the daemon names the kind, the editor
    /// runs the provider.
    #[test]
    fn a_field_reference_becomes_a_ref_field_target() {
        let store = store_with(&[("src/order.ts", "fn:placeOrder", "function", 10, 30)]);
        let src = node_id("src/order.ts", "fn:placeOrder");
        let field = call(src, "field:count", 20);
        let targets = targets_needing_editor(&store, &[], &[field], &no_locals());
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].edge_kind, "ref/field");
        assert_eq!(targets[0].name, "count");
        assert_eq!(targets[0].provider, "definition");
    }

    /// RFC-027 #813 P2: the extractor's exact occurrence column wins over the
    /// name search when the name repeats on the line. The name search finds the
    /// first `save`, which is the wrong occurrence and would resolve the wrong
    /// call; the captured column points at the one the extractor actually meant.
    #[test]
    fn the_extractor_column_beats_the_name_search_when_a_name_repeats() {
        let store = store_with(&[("src/a.ts", "fn:f", "function", 1, 3)]);
        let src = node_id("src/a.ts", "fn:f");
        let content = "  a.save(); b.save()\n";
        let mut c = call(src, "fn:save", 1);
        c.is_method_call = true;
        c.caller_col = Some(14); // byte column of the second `save`
        let lines: Vec<&str> = content.lines().collect();
        let targets = targets_needing_editor(&store, &lines, &[c], &no_locals());
        assert_eq!(targets.len(), 1);
        assert_eq!(
            targets[0].ref_col,
            Some(14),
            "the exact occurrence column must win over the first name match"
        );
    }

    /// A bare free-function call the lexical lane could not resolve has no unique
    /// definition in the graph; the editor's provider would resolve it to that
    /// same missing target and abstain, so it is not sent (stay surgical, §7.6).
    #[test]
    fn an_unresolvable_free_call_is_not_sent_to_the_editor() {
        let store = store_with(&[("src/order.ts", "fn:placeOrder", "function", 10, 30)]);
        let src = node_id("src/order.ts", "fn:placeOrder");
        let bare = call(src, "fn:nowhere", 21);
        assert!(targets_needing_editor(&store, &[], &[bare], &no_locals()).is_empty());
    }

    /// RFC-027 section 12: the editor lane records what it claimed, so the
    /// precision meter can score it. Without a claim row an editor-resolved edge
    /// is invisible to the meter, and a language that runs this lane alone
    /// (every non-native one, §8.3) could never earn its gate a reading.
    #[test]
    fn an_editor_resolution_is_recorded_as_a_scorable_claim() {
        let mut store = store_with(&[
            ("src/order.ts", "fn:placeOrder", "function", 10, 30),
            ("src/user.ts", "method:User.save", "method", 15, 20),
        ]);
        apply_live_resolutions(
            &mut store,
            CORPUS,
            "src/order.ts",
            &[resolution(18, "save", "src/user.ts", 17)],
        );
        let sample = store
            .live_precision_sample_by_language()
            .expect("precision sample");
        let bucket = sample
            .get("typescript")
            .expect("the claim must be attributed to the source node's language");
        assert_eq!(
            bucket.claims(),
            1,
            "the editor lane's resolution must be a claim the meter can see"
        );
    }

    /// An editor answer the daemon could not map abstains, and the abstention is
    /// recorded rather than dropped: "there is a reference here and its target is
    /// not yet known" is true and useful (§9.2).
    #[test]
    fn an_editor_abstention_is_recorded_as_pending() {
        let mut store = store_with(&[("src/order.ts", "fn:placeOrder", "function", 10, 30)]);
        let out = apply_live_resolutions(
            &mut store,
            CORPUS,
            "src/order.ts",
            &[resolution(18, "save", "src/nowhere.ts", 17)],
        );
        assert_eq!(
            out,
            LiveOutcome {
                emitted: 0,
                pending: 1
            },
        );
        let pending = store
            .pending_refs_in_file(CORPUS, "src/order.ts")
            .expect("pending_refs_in_file");
        assert_eq!(pending, vec![("save".to_string(), 18)]);
    }

    /// The editor lane must upgrade the save-path pass's row for the same
    /// reference, not fork it into a stale `pending` beside a `resolved`. Both
    /// lanes key on `ref_col = 0` for exactly this reason.
    #[test]
    fn an_editor_answer_upgrades_the_save_paths_pending_row() {
        let mut store = store_with(&[
            ("src/order.ts", "fn:placeOrder", "function", 10, 30),
            ("src/user.ts", "method:User.save", "method", 15, 20),
        ]);
        let src = node_id("src/order.ts", "fn:placeOrder");
        // The save-path pass abstains on a method call with no receiver type.
        let mut ambiguous = call(src, "fn:save", 18);
        ambiguous.is_method_call = true;
        resolve_unambiguous_lexical(
            &mut store,
            CORPUS,
            "src/order.ts",
            &[ambiguous],
            &[],
            &no_locals(),
        );
        assert_eq!(
            store
                .pending_refs_in_file(CORPUS, "src/order.ts")
                .expect("pending_refs_in_file")
                .len(),
            1,
        );

        apply_live_resolutions(
            &mut store,
            CORPUS,
            "src/order.ts",
            &[resolution(18, "save", "src/user.ts", 17)],
        );
        assert!(
            store
                .pending_refs_in_file(CORPUS, "src/order.ts")
                .expect("pending_refs_in_file")
                .is_empty(),
            "the resolved reference must leave no stale pending row behind"
        );
        assert_eq!(
            store
                .live_precision_sample_by_language()
                .expect("precision sample")
                .get("typescript")
                .map(|b| b.claims()),
            Some(1),
            "one reference must produce one claim, not two rows",
        );
    }

    /// RFC-027 section 8.3: a language with no native extractor has no lexical
    /// floor, so every detected reference becomes an editor target — a call as
    /// `ref/call`, a field read as `ref/field`, each for the definition
    /// provider. Nothing is filtered: dropping one would lose the reference
    /// outright rather than hand it to another lane.
    #[test]
    fn every_generic_reference_becomes_an_editor_target() {
        let refs = travsr_analysis::live_detect::LiveRefs {
            calls: vec![
                travsr_analysis::live_detect::LiveRef {
                    line: 12,
                    name: "Start".to_string(),
                },
                travsr_analysis::live_detect::LiveRef {
                    line: 13,
                    name: "helper".to_string(),
                },
            ],
            fields: vec![travsr_analysis::live_detect::LiveRef {
                line: 14,
                name: "count".to_string(),
            }],
            inheritance: Vec::new(),
        };
        let targets = generic_targets_needing_editor(&refs);
        assert_eq!(targets.len(), 3);
        assert_eq!(
            targets
                .iter()
                .map(|t| (t.ref_line, t.name.as_str(), t.edge_kind.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (12, "Start", "ref/call"),
                (13, "helper", "ref/call"),
                (14, "count", "ref/field"),
            ],
        );
        assert!(targets.iter().all(|t| t.provider == "definition"));
    }

    /// The editor recovers a target's column by finding the *first* whole-word
    /// match of the name on its line, so two references sharing a line, a name
    /// and a kind both resolve to the first occurrence's position. Collapsing
    /// them to one would attribute that answer to both — for `a.save();
    /// b.save();` with different receiver types, a wrong edge. Both abstain.
    #[test]
    fn generic_targets_colliding_on_a_position_are_dropped() {
        let refs = travsr_analysis::live_detect::LiveRefs {
            calls: vec![
                travsr_analysis::live_detect::LiveRef {
                    line: 7,
                    name: "Get".to_string(),
                },
                travsr_analysis::live_detect::LiveRef {
                    line: 7,
                    name: "Get".to_string(),
                },
            ],
            fields: Vec::new(),
            inheritance: Vec::new(),
        };
        assert!(
            generic_targets_needing_editor(&refs).is_empty(),
            "two references the editor cannot tell apart must both abstain, \
             not collapse onto the first occurrence's answer"
        );
    }

    /// A live edge for a non-native language rides exactly the same emit path as
    /// a native one: the daemon maps the reference line to its enclosing
    /// definition and the editor's answer to a node, and neither endpoint is
    /// minted here (§8.2). This is a Go file with no native extractor at all.
    #[test]
    fn a_generic_language_edge_emits_through_the_shared_path() {
        let mut store = store_with(&[
            ("cmd/run.go", "fn:Run", "function", 10, 20),
            ("svc/session.go", "method:Session.Start", "method", 30, 40),
        ]);
        let out = apply_live_resolutions(
            &mut store,
            CORPUS,
            "cmd/run.go",
            &[resolution_kind(
                12,
                "Start",
                "svc/session.go",
                31,
                "ref/call",
            )],
        );
        assert_eq!(
            out,
            LiveOutcome {
                emitted: 1,
                pending: 0
            },
        );
        let edge = store
            .iter_edges_from(node_id("cmd/run.go", "fn:Run"))
            .expect("iter_edges_from")
            .into_iter()
            .find(|e| e.dst == node_id("svc/session.go", "method:Session.Start"))
            .expect("a live edge to the Go method must exist");
        assert_eq!(edge.kind, EdgeKind::RefCall);
    }

    /// An editor report naming an edge kind outside Bucket B (a structural kind,
    /// a cross-language `ffi/call`, or an unknown string) is refused: the lane
    /// emits only the kinds it was scoped to, so a malformed report abstains
    /// rather than writing an out-of-scope edge.
    #[test]
    fn an_out_of_scope_edge_kind_is_refused() {
        assert!(live_edge_kind("ffi/call").is_none());
        assert!(live_edge_kind("depends").is_none());
        assert!(live_edge_kind("not-a-kind").is_none());
        // Ruled out and deferred respectively: admitting them made the
        // accept-list pass exactly the kinds it exists to stop.
        assert!(live_edge_kind("overrides").is_none());
        assert!(live_edge_kind("ref/imports").is_none());
        assert_eq!(live_edge_kind("ref/field"), Some(EdgeKind::RefField));

        let mut store = store_with(&[
            ("src/order.ts", "fn:placeOrder", "function", 10, 30),
            ("src/user.ts", "method:User.save", "method", 15, 20),
        ]);
        let out = apply_live_resolutions(
            &mut store,
            CORPUS,
            "src/order.ts",
            &[resolution_kind(18, "save", "src/user.ts", 17, "ffi/call")],
        );
        assert_eq!(
            out,
            LiveOutcome {
                emitted: 0,
                pending: 1
            },
        );
    }

    // ── IsImplementation lane (RFC-027 live edge-kind scope, TypeScript) ────────

    fn inherit(base_name: &str, line: u32) -> InheritanceRef {
        InheritanceRef {
            base_name: base_name.to_string(),
            line,
        }
    }

    /// Lexical floor: `class Order extends Base` where `Base` has exactly one
    /// definition repo-wide resolves to an `is-implementation` edge with no
    /// language server. The clause line is the class declaration line, which maps
    /// to the implementing class as the edge source.
    #[test]
    fn an_unambiguous_base_resolves_to_a_live_is_implementation_edge() {
        let mut store = store_with(&[
            ("src/order.ts", "class:Order", "class", 3, 20),
            ("src/base.ts", "class:Base", "class", 1, 10),
        ]);
        let out = resolve_unambiguous_lexical(
            &mut store,
            CORPUS,
            "src/order.ts",
            &[],
            &[inherit("Base", 3)],
            &no_locals(),
        );
        assert_eq!(
            out,
            LiveOutcome {
                emitted: 1,
                pending: 0
            },
        );
        let edge = store
            .iter_edges_from(node_id("src/order.ts", "class:Order"))
            .expect("iter_edges_from")
            .into_iter()
            .find(|e| e.dst == node_id("src/base.ts", "class:Base"))
            .expect("a live is-implementation edge to the base must exist");
        assert_eq!(edge.kind, EdgeKind::IsImplementation);
        assert_eq!(edge.provenance.as_deref(), Some("live"));
    }

    /// An interface `implements` target resolves the same way, keyed on the
    /// `interface:` signature the target-kind set admits.
    #[test]
    fn an_unambiguous_interface_resolves_to_a_live_edge() {
        let mut store = store_with(&[
            ("src/order.ts", "class:Order", "class", 3, 20),
            ("src/shape.ts", "interface:Shape", "interface", 1, 5),
        ]);
        let out = resolve_unambiguous_lexical(
            &mut store,
            CORPUS,
            "src/order.ts",
            &[],
            &[inherit("Shape", 3)],
            &no_locals(),
        );
        assert_eq!(
            out,
            LiveOutcome {
                emitted: 1,
                pending: 0
            },
        );
        assert_eq!(
            provenance_of(
                &store,
                node_id("src/order.ts", "class:Order"),
                node_id("src/shape.ts", "interface:Shape"),
            )
            .as_deref(),
            Some("live"),
        );
    }

    /// A base with two definitions repo-wide is the cross-file ambiguity the
    /// editor's provider exists for: the lexical floor abstains and it becomes a
    /// `definition`-provider target, never a guessed edge.
    #[test]
    fn an_ambiguous_base_abstains_and_becomes_an_editor_target() {
        let store = store_with(&[
            ("src/order.ts", "class:Order", "class", 3, 20),
            ("src/a.ts", "class:Base", "class", 1, 10),
            ("src/b.ts", "class:Base", "class", 1, 10),
        ]);
        let targets = inheritance_targets_needing_editor(
            &store,
            CORPUS,
            "src/order.ts",
            &[inherit("Base", 3)],
            &no_locals(),
        );
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].edge_kind, "is-implementation");
        assert_eq!(targets[0].name, "Base");
        assert_eq!(targets[0].ref_line, 3);
        assert_eq!(
            targets[0].provider, "definition",
            "the base type is resolved to its definition, not its implementors",
        );
    }

    /// A base the lexical floor could settle is not also sent to the editor: the
    /// two lanes partition inheritance clauses with no overlap, exactly as they
    /// do calls.
    #[test]
    fn a_resolvable_base_is_not_sent_to_the_editor() {
        let store = store_with(&[
            ("src/order.ts", "class:Order", "class", 3, 20),
            ("src/base.ts", "class:Base", "class", 1, 10),
        ]);
        let targets = inheritance_targets_needing_editor(
            &store,
            CORPUS,
            "src/order.ts",
            &[inherit("Base", 3)],
            &no_locals(),
        );
        assert!(
            targets.is_empty(),
            "a unique base is the lexical floor's job"
        );
    }

    /// Section 7.3b for an implements target: an editor resolves the base name to
    /// its declaration and the daemon emits an `is-implementation` edge from the
    /// implementing class to the interface node the position mapped to.
    #[test]
    fn an_editor_resolved_base_becomes_an_is_implementation_edge() {
        let mut store = store_with(&[
            ("src/order.ts", "class:Order", "class", 3, 20),
            ("src/shape.ts", "interface:Shape", "interface", 1, 5),
        ]);
        let out = apply_live_resolutions(
            &mut store,
            CORPUS,
            "src/order.ts",
            &[resolution_kind(
                3,
                "Shape",
                "src/shape.ts",
                1,
                "is-implementation",
            )],
        );
        assert_eq!(
            out,
            LiveOutcome {
                emitted: 1,
                pending: 0
            },
        );
        let edge = store
            .iter_edges_from(node_id("src/order.ts", "class:Order"))
            .expect("iter_edges_from")
            .into_iter()
            .find(|e| e.dst == node_id("src/shape.ts", "interface:Shape"))
            .expect("an is-implementation edge must exist");
        assert_eq!(edge.kind, EdgeKind::IsImplementation);
    }
}
