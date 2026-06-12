//! Shared CLI query execution (#318 O1).
//!
//! `travsr ask` / `travsr graph` / `travsr status` produce their data through
//! the functions in this module, on two routes:
//!
//! 1. **Daemon route** — the daemon's control socket receives a
//!    `ControlMessage::Query`, runs these functions against its warm store,
//!    and ships the payload back as JSON. No per-command store open.
//! 2. **Direct route** — no daemon running: the CLI opens the store
//!    (read-only fast path) and calls the same functions in-process.
//!
//! Because both routes execute identical code, CLI output is byte-identical
//! regardless of routing. The serialized payload shapes are versioned by
//! `travsr_ipc::QUERY_PROTOCOL_VERSION` — bump it on any breaking change here.

use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};
use travsr_core::{display_label, is_noise_node, NodeId};
use travsr_retrieval::{context_candidates, knapsack, token_cost};
use travsr_store::{SqliteStore, Store, StoreMigratable};

/// Token budget for `travsr ask` and the default for `travsr graph --budget`.
/// Matches the MCP `get_context` default.
pub const DEFAULT_TOKEN_BUDGET: usize = 4096;

// ── Payload types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QueryDirection {
    Deps,
    Callers,
    Both,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QueryEdgeMode {
    Semantic,
    All,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GraphQueryArgs {
    pub query: String,
    pub depth: u8,
    pub direction: QueryDirection,
    pub edge_mode: QueryEdgeMode,
    pub include_noise: bool,
}

/// One graph node, with everything the CLI renderers need.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeEntry {
    pub id: u64,
    pub signature: String,
    pub kind: String,
    pub path: String,
    pub language: String,
    pub depth: u8,
    pub label: String,
    /// Estimated token cost (same formula as `travsr ask` / `get_context`).
    pub tokens: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeEntry {
    pub src: u64,
    pub dst: u64,
    pub kind: String,
    pub provenance: String,
    /// Endpoint signatures resolved at build time so renderers never need
    /// store access (noise endpoints are not in `nodes` but still render).
    pub src_sig: String,
    pub dst_sig: String,
}

/// One BFS spanning-tree expansion step, in discovery order — drives the
/// `--format tree` renderer without store access.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeStep {
    pub parent: u64,
    pub edge_kind: String,
    pub child: u64,
}

/// Coverage / completeness metadata (#318 O5) — distinguishes "no callers"
/// from "cannot see callers".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Coverage {
    pub language: String,
    pub semantic: bool,
    pub phase_b_commit: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GraphPayload {
    /// `None` when no symbol matched the query.
    pub seed: Option<NodeEntry>,
    /// BFS discovery order — index 0 is the seed. Budget truncation keeps a
    /// prefix of this order, so ancestors always survive before descendants.
    pub nodes: Vec<NodeEntry>,
    pub edges: Vec<EdgeEntry>,
    pub tree: Vec<TreeStep>,
    /// True when caller traversal found no semantic edges and fell back to
    /// structural ones (the CLI prints the `travsr lang` hint for this).
    pub no_semantic_fallback: bool,
    pub coverage: Option<Coverage>,
    pub last_commit: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AskRow {
    pub kind: String,
    pub signature: String,
    pub path: String,
    pub score: f32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AskPayload {
    /// False when no symbol matched the query at all.
    pub matched: bool,
    /// True when a seed matched but PPR produced nothing.
    pub no_results: bool,
    pub rows: Vec<AskRow>,
    pub total_tokens: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StatusPayload {
    pub nodes: u64,
    pub edges: u64,
    pub schema: u32,
    pub journal: String,
    pub last_commit: Option<String>,
    pub signature_format_version: u8,
}

// ── status ────────────────────────────────────────────────────────────────────

pub fn status_query(store: &SqliteStore) -> anyhow::Result<StatusPayload> {
    Ok(StatusPayload {
        nodes: store.node_count()?,
        edges: store.edge_count()?,
        schema: store.schema_version()?,
        journal: store.journal_mode()?,
        last_commit: store.get_meta("last_commit")?,
        signature_format_version: store.get_signature_format_version()?,
    })
}

// ── ask ───────────────────────────────────────────────────────────────────────

pub fn ask_query(store: &SqliteStore, query: &str) -> anyhow::Result<AskPayload> {
    // Strip a leading `:` so VS Code graph-panel queries (which prefix with `:`
    // to bypass the colon-delimited command syntax) pass through cleanly.
    let query = query.strip_prefix(':').unwrap_or(query).trim();

    let matches = store.search_nodes_fuzzy(query)?;
    if matches.is_empty() {
        return Ok(AskPayload {
            matched: false,
            no_results: false,
            rows: Vec::new(),
            total_tokens: 0,
        });
    }

    // Prefer structural seeds over file nodes for richer PPR traversal.
    let preferred = ["class", "function", "method", "file"];
    let seed_node = preferred
        .iter()
        .find_map(|k| matches.iter().find(|n| n.kind == *k))
        .unwrap_or(&matches[0]);
    let seed = seed_node.id;

    let ppr_scores = travsr_retrieval::ppr(store, &[seed], context_candidates())?;
    if ppr_scores.is_empty() {
        return Ok(AskPayload {
            matched: true,
            no_results: true,
            rows: Vec::new(),
            total_tokens: 0,
        });
    }

    let node_ids: Vec<_> = ppr_scores.iter().map(|(id, _)| *id).collect();
    let score_map: HashMap<_, f32> = ppr_scores.into_iter().collect();
    let nodes = store.get_nodes(&node_ids)?;
    let items: Vec<_> = nodes
        .into_iter()
        .filter_map(|n| score_map.get(&n.id).map(|&s| (n, s)))
        .collect();

    let selected = knapsack(items, DEFAULT_TOKEN_BUDGET);
    let total_tokens: usize = selected.iter().map(token_cost).sum();
    let rows = selected
        .into_iter()
        .map(|n| AskRow {
            score: score_map.get(&n.id).copied().unwrap_or(0.0),
            kind: n.kind,
            signature: n.vname.signature,
            path: n.vname.path,
        })
        .collect();

    Ok(AskPayload {
        matched: true,
        no_results: false,
        rows,
        total_tokens,
    })
}

// ── graph ─────────────────────────────────────────────────────────────────────

fn node_entry(node: &travsr_core::Node, depth: u8) -> NodeEntry {
    NodeEntry {
        id: node.id.0,
        signature: node.vname.signature.clone(),
        kind: node.kind.clone(),
        path: node.vname.path.clone(),
        language: node.vname.language.clone(),
        depth,
        label: display_label(node).to_string(),
        tokens: token_cost(node),
    }
}

fn is_semantic_edge(kind: &str) -> bool {
    matches!(
        kind,
        "ref/call" | "ffi/call" | "overrides" | "is-implementation" | "ref/imports" | "resolves-to"
    )
}

/// Outgoing/incoming expansion for one node, mirroring `travsr graph`'s edge
/// semantics (RFC-014 file-node caller splice, semantic-preferred mode with
/// structural fallback). Sets `*no_semantic_fallback` when the fallback fires.
fn next_edges(
    store: &SqliteStore,
    node_id: NodeId,
    direction: QueryDirection,
    edge_mode: QueryEdgeMode,
    no_semantic_fallback: &mut bool,
) -> anyhow::Result<Vec<(String, NodeId)>> {
    let mut out = Vec::new();
    if matches!(direction, QueryDirection::Deps | QueryDirection::Both) {
        for e in store.iter_edges_from(node_id)? {
            out.push((e.kind.as_str().to_string(), e.dst));
        }
    }
    if matches!(direction, QueryDirection::Callers | QueryDirection::Both) {
        let mut incoming = store.iter_edges_to(node_id)?;
        // RFC-014 #317: a file's callers are the callers of the definitions it
        // contains — splice them in so function-level callers surface when a
        // query resolves to a file node. Edges sourced from the file itself
        // (defines/binding) are skipped to avoid self-loops.
        if let Some(node) = store.get_node(node_id)? {
            if node.kind == "file" {
                // Cap the splice at 200 definitions: pathological generated
                // files can hold thousands, each costing an iter_edges_to
                // round-trip. Full fidelity remains available via per-symbol
                // queries.
                for def_id in store
                    .definition_node_ids_in_file(&node.vname.corpus, &node.vname.path)?
                    .into_iter()
                    .take(200)
                {
                    incoming.extend(
                        store
                            .iter_edges_to(def_id)?
                            .into_iter()
                            .filter(|e| e.src != node_id),
                    );
                }
            }
        }
        if matches!(edge_mode, QueryEdgeMode::Semantic) {
            let has_semantic = incoming.iter().any(|e| is_semantic_edge(e.kind.as_str()));
            if has_semantic {
                for e in &incoming {
                    let s = e.kind.as_str();
                    if is_semantic_edge(s) || s == "defines/binding" {
                        out.push((s.to_string(), e.src));
                    }
                }
            } else {
                *no_semantic_fallback = true;
                for e in &incoming {
                    out.push((e.kind.as_str().to_string(), e.src));
                }
            }
        } else {
            for e in &incoming {
                out.push((e.kind.as_str().to_string(), e.src));
            }
        }
    }
    // Multiple call sites (and the file-node definition splice) can yield the
    // same (kind, src) pair — collapse them for display.
    let mut seen = HashSet::new();
    out.retain(|(kind, id)| seen.insert((kind.clone(), *id)));
    Ok(out)
}

fn coverage_for(store: &SqliteStore, language: &str) -> Coverage {
    Coverage {
        language: language.to_string(),
        semantic: store.has_refcall_edges_for_language(language),
        phase_b_commit: store.get_meta("phase_b_commit").ok().flatten(),
    }
}

/// BFS subgraph around the best seed for `args.query`.
///
/// `payload.seed == None` means no symbol matched. Nodes are emitted in BFS
/// discovery order; `tree` holds the spanning-tree expansion steps.
pub fn graph_query(store: &SqliteStore, args: &GraphQueryArgs) -> anyhow::Result<GraphPayload> {
    let matches = store.search_nodes_by_name(&args.query)?;
    let Some(seed) = matches
        .iter()
        .find(|n| n.kind == "file")
        .or_else(|| matches.first())
    else {
        return Ok(GraphPayload {
            seed: None,
            nodes: Vec::new(),
            edges: Vec::new(),
            tree: Vec::new(),
            no_semantic_fallback: false,
            coverage: None,
            last_commit: None,
        });
    };

    let mut nodes: Vec<NodeEntry> = Vec::new();
    let mut node_index: HashSet<NodeId> = HashSet::new();
    let mut edges_raw: Vec<(NodeId, NodeId, String, String)> = Vec::new();
    let mut tree: Vec<TreeStep> = Vec::new();
    let mut visited: HashSet<NodeId> = HashSet::new();
    let mut queue: VecDeque<(NodeId, u8)> = VecDeque::new();
    let mut no_semantic_fallback = false;

    visited.insert(seed.id);
    queue.push_back((seed.id, 0));

    while let Some((current_id, depth)) = queue.pop_front() {
        if let Some(node) = store.get_node(current_id)? {
            if node_index.insert(current_id) {
                nodes.push(node_entry(&node, depth));
            }
        }

        if depth >= args.depth {
            continue;
        }

        for (edge_kind, next_id) in next_edges(
            store,
            current_id,
            args.direction,
            args.edge_mode,
            &mut no_semantic_fallback,
        )? {
            let (src, dst) = match args.direction {
                QueryDirection::Callers => (next_id, current_id),
                _ => (current_id, next_id),
            };
            // DEBT(travsr-75): iter_edges_from/to do not return provenance, so
            // BFS-traversed edges always show "tree-sitter" in JSON output even
            // when the DB row is "lsif". Only --all mode (all_edges) is correct.
            edges_raw.push((src, dst, edge_kind.clone(), "tree-sitter".to_string()));

            if !visited.contains(&next_id) {
                if let Some(next_node) = store.get_node(next_id)? {
                    if !args.include_noise && is_noise_node(&next_node) {
                        visited.insert(next_id);
                        continue;
                    }
                }
                visited.insert(next_id);
                tree.push(TreeStep {
                    parent: current_id.0,
                    edge_kind,
                    child: next_id.0,
                });
                queue.push_back((next_id, depth + 1));
            }
        }
    }

    let edges = resolve_edge_sigs(store, &nodes, edges_raw)?;
    let coverage = coverage_for(store, &seed.vname.language);
    let seed_entry = nodes.first().cloned();

    Ok(GraphPayload {
        seed: seed_entry,
        nodes,
        edges,
        tree,
        no_semantic_fallback,
        coverage: Some(coverage),
        last_commit: store.get_meta("last_commit")?,
    })
}

/// Whole-graph payload for `travsr graph --all`. No traversal: every node at
/// depth 0, edges verbatim from the store (with true provenance).
pub fn graph_all_payload(store: &SqliteStore) -> anyhow::Result<GraphPayload> {
    let all = store.all_nodes()?;
    let nodes: Vec<NodeEntry> = all.iter().map(|n| node_entry(n, 0)).collect();
    let edges_raw = store.all_edges()?;
    let edges = resolve_edge_sigs(store, &nodes, edges_raw)?;
    Ok(GraphPayload {
        seed: None,
        nodes,
        edges,
        tree: Vec::new(),
        no_semantic_fallback: false,
        coverage: None,
        last_commit: store.get_meta("last_commit")?,
    })
}

fn resolve_edge_sigs(
    store: &SqliteStore,
    nodes: &[NodeEntry],
    edges_raw: Vec<(NodeId, NodeId, String, String)>,
) -> anyhow::Result<Vec<EdgeEntry>> {
    let mut sig_lookup: HashMap<u64, String> =
        nodes.iter().map(|n| (n.id, n.signature.clone())).collect();
    let mut edges = Vec::with_capacity(edges_raw.len());
    for (src, dst, kind, provenance) in edges_raw {
        for nid in [src, dst] {
            if let std::collections::hash_map::Entry::Vacant(e) = sig_lookup.entry(nid.0) {
                if let Some(node) = store.get_node(nid)? {
                    e.insert(node.vname.signature.clone());
                }
            }
        }
        edges.push(EdgeEntry {
            src: src.0,
            dst: dst.0,
            kind,
            provenance,
            src_sig: sig_lookup.get(&src.0).cloned().unwrap_or_default(),
            dst_sig: sig_lookup.get(&dst.0).cloned().unwrap_or_default(),
        });
    }
    Ok(edges)
}

// ── token budget (#318 O6) ────────────────────────────────────────────────────

/// Truncate `payload` to `budget` tokens. Returns the number of nodes cut.
///
/// Selection is a prefix of BFS discovery order — the seed always survives
/// (even if it alone exceeds the budget) and a node's parent is always kept
/// before the node, so tree rendering stays connected. `budget == 0` means
/// unlimited.
pub fn apply_token_budget(payload: &mut GraphPayload, budget: usize) -> usize {
    if budget == 0 || payload.nodes.is_empty() {
        return 0;
    }
    let mut cum = 0usize;
    let mut keep = 0usize;
    for (i, n) in payload.nodes.iter().enumerate() {
        if i > 0 && cum + n.tokens > budget {
            break;
        }
        cum += n.tokens;
        keep = i + 1;
    }
    let truncated = payload.nodes.len() - keep;
    if truncated == 0 {
        return 0;
    }
    let cut_ids: HashSet<u64> = payload.nodes[keep..].iter().map(|n| n.id).collect();
    payload.nodes.truncate(keep);
    payload
        .edges
        .retain(|e| !cut_ids.contains(&e.src) && !cut_ids.contains(&e.dst));
    payload
        .tree
        .retain(|t| !cut_ids.contains(&t.parent) && !cut_ids.contains(&t.child));
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use travsr_core::{Edge, EdgeKind, Node, VName};

    fn node(sig: &str, kind: &str, path: &str) -> Node {
        let vname = VName {
            signature: sig.to_string(),
            corpus: "test".to_string(),
            root: String::new(),
            path: path.to_string(),
            language: "typescript".to_string(),
        };
        let id = vname.id();
        Node {
            id,
            vname,
            kind: kind.to_string(),
            package: String::new(),
            line: None,
            end_line: None,
        }
    }

    fn seeded_store() -> (SqliteStore, Node, Node, Node) {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let file = node("file", "file", "src/service.ts");
        let class = node("class:PaymentService", "class", "src/service.ts");
        let caller = node("fn:processPayment", "function", "src/controller.ts");
        for n in [&file, &class, &caller] {
            store.put_node(n).unwrap();
        }
        store
            .put_edge(&Edge::new(file.id, class.id, EdgeKind::DefinesBinding))
            .unwrap();
        store
            .put_edge(&Edge::new(caller.id, class.id, EdgeKind::RefCall))
            .unwrap();
        (store, file, class, caller)
    }

    #[test]
    fn status_query_reports_counts_and_schema() {
        let (store, ..) = seeded_store();
        let s = status_query(&store).unwrap();
        assert_eq!(s.nodes, 3);
        assert_eq!(s.edges, 2);
        assert!(s.schema > 0);
    }

    #[test]
    fn graph_query_finds_callers_with_tree_steps() {
        let (store, _, class, caller) = seeded_store();
        let payload = graph_query(
            &store,
            &GraphQueryArgs {
                query: "PaymentService".to_string(),
                depth: 3,
                direction: QueryDirection::Callers,
                edge_mode: QueryEdgeMode::Semantic,
                include_noise: false,
            },
        )
        .unwrap();
        let seed = payload.seed.as_ref().expect("seed must match");
        assert_eq!(seed.signature, "class:PaymentService");
        assert!(payload.nodes.iter().any(|n| n.id == caller.id.0));
        assert!(payload
            .tree
            .iter()
            .any(|t| t.parent == class.id.0 && t.child == caller.id.0));
        let cov = payload.coverage.expect("coverage present");
        assert_eq!(cov.language, "typescript");
        assert!(cov.semantic, "ref/call edge exists for typescript");
    }

    #[test]
    fn graph_query_no_match_returns_empty_payload() {
        let (store, ..) = seeded_store();
        let payload = graph_query(
            &store,
            &GraphQueryArgs {
                query: "DoesNotExist".to_string(),
                depth: 3,
                direction: QueryDirection::Both,
                edge_mode: QueryEdgeMode::Semantic,
                include_noise: false,
            },
        )
        .unwrap();
        assert!(payload.seed.is_none());
        assert!(payload.nodes.is_empty());
    }

    #[test]
    fn token_budget_keeps_bfs_prefix_and_reports_cut() {
        let (store, ..) = seeded_store();
        let mut payload = graph_query(
            &store,
            &GraphQueryArgs {
                query: "PaymentService".to_string(),
                depth: 3,
                direction: QueryDirection::Both,
                edge_mode: QueryEdgeMode::All,
                include_noise: true,
            },
        )
        .unwrap();
        let total = payload.nodes.len();
        assert!(total >= 2, "need at least seed + one neighbour");
        let seed_tokens = payload.nodes[0].tokens;
        // Budget exactly the seed's cost: everything after it is cut.
        let cut = apply_token_budget(&mut payload, seed_tokens);
        assert_eq!(payload.nodes.len(), 1);
        assert_eq!(cut, total - 1);
        // All surviving edges/tree steps reference surviving nodes only.
        let kept: HashSet<u64> = payload.nodes.iter().map(|n| n.id).collect();
        assert!(payload.tree.iter().all(|t| kept.contains(&t.parent)));
    }

    #[test]
    fn zero_budget_means_unlimited() {
        let (store, ..) = seeded_store();
        let mut payload = graph_query(
            &store,
            &GraphQueryArgs {
                query: "PaymentService".to_string(),
                depth: 3,
                direction: QueryDirection::Both,
                edge_mode: QueryEdgeMode::All,
                include_noise: true,
            },
        )
        .unwrap();
        let before = payload.nodes.len();
        assert_eq!(apply_token_budget(&mut payload, 0), 0);
        assert_eq!(payload.nodes.len(), before);
    }

    #[test]
    fn ask_query_returns_rows_within_budget() {
        let (store, ..) = seeded_store();
        let payload = ask_query(&store, "PaymentService").unwrap();
        assert!(payload.matched);
        assert!(payload.total_tokens <= DEFAULT_TOKEN_BUDGET);
    }
}
