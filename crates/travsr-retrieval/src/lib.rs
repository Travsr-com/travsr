//! travsr-retrieval — graph traversal and ranking algorithms.
//!
//! MVP ships BFS depth-3. Production adds Personalized PageRank,
//! Prize-Collecting Steiner Tree, k-core decomposition, and 0-1 knapsack
//! for token-budgeted context assembly. See CLAUDE.md "Algorithmic Stack".

// O(V + E) where V = visited nodes, E = traversed edges.

#![forbid(unsafe_code)]

pub mod ppr;
pub use ppr::ppr;

use std::collections::{HashSet, VecDeque};

use travsr_core::{Node, NodeId};
use travsr_error::TravsrError;
use travsr_store::{SqliteStore, Store};

/// Bounded BFS from `seed`, returning reachable nodes in discovery order.
///
/// Terminates when `depth > max_depth` or the cumulative token cost
/// (signature.len + kind.len per node) exceeds `token_budget`.
/// Cycles are handled via a visited set — the graph may contain them.
pub fn bfs(
    store: &SqliteStore,
    seed: NodeId,
    max_depth: u8,
    token_budget: usize,
) -> Result<Vec<Node>, TravsrError> {
    let _span = tracing::debug_span!(
        "bfs.expand",
        seed = seed.0,
        max_depth = max_depth,
        token_budget = token_budget
    )
    .entered();

    let mut visited: HashSet<NodeId> = HashSet::new();
    let mut queue: VecDeque<(NodeId, u8)> = VecDeque::new();
    let mut result: Vec<Node> = Vec::new();
    let mut tokens_used: usize = 0;
    let mut edges_traversed: usize = 0;

    queue.push_back((seed, 0));
    visited.insert(seed);

    while let Some((current_id, depth)) = queue.pop_front() {
        if depth > max_depth {
            break;
        }

        if let Some(node) = store.get_node(current_id)? {
            let cost = node.vname.signature.len() + node.kind.len();
            if tokens_used + cost > token_budget {
                break;
            }
            tokens_used += cost;
            result.push(node);
        }

        if depth < max_depth {
            for edge in store.iter_edges_from(current_id)? {
                // edges_traversed counts all edges examined, including back-edges to
                // already-visited nodes — it measures store query load, not unique new nodes.
                edges_traversed += 1;
                if !visited.contains(&edge.dst) {
                    visited.insert(edge.dst);
                    queue.push_back((edge.dst, depth + 1));
                }
            }
        }
    }

    tracing::debug!(
        nodes_visited = result.len(),
        edges_traversed = edges_traversed,
        tokens_used = tokens_used,
        "bfs.expand complete"
    );
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use travsr_core::{Edge, EdgeKind, VName};
    use travsr_store::Store;

    fn node(path: &str, sig: &str) -> travsr_core::Node {
        travsr_core::Node::new(VName::new("", "", path, "typescript", sig), "function")
    }

    fn store_with_nodes_and_edges(
        nodes: &[travsr_core::Node],
        edges: &[(NodeId, NodeId, EdgeKind)],
    ) -> SqliteStore {
        let mut store = SqliteStore::open_in_memory().unwrap();
        for n in nodes {
            store.put_node(n).unwrap();
        }
        for &(src, dst, kind) in edges {
            store.put_edge(&Edge::new(src, dst, kind)).unwrap();
        }
        store
    }

    #[test]
    fn bfs_empty_graph_returns_seed_only() {
        let a = node("a.ts", "fn:a");
        let store = store_with_nodes_and_edges(std::slice::from_ref(&a), &[]);
        let result = bfs(&store, a.id, 3, 4096).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, a.id);
    }

    #[test]
    fn bfs_follows_edges() {
        let a = node("a.ts", "fn:a");
        let b = node("b.ts", "fn:b");
        let c = node("c.ts", "fn:c");
        let store = store_with_nodes_and_edges(
            &[a.clone(), b.clone(), c.clone()],
            &[
                (a.id, b.id, EdgeKind::DefinesBinding),
                (b.id, c.id, EdgeKind::DefinesBinding),
            ],
        );
        let result = bfs(&store, a.id, 2, 4096).unwrap();
        let ids: Vec<NodeId> = result.iter().map(|n| n.id).collect();
        assert!(ids.contains(&a.id), "seed must be in result");
        assert!(ids.contains(&b.id), "depth-1 node must be in result");
        assert!(ids.contains(&c.id), "depth-2 node must be in result");
    }

    #[test]
    fn bfs_respects_depth_limit() {
        let a = node("a.ts", "fn:a");
        let b = node("b.ts", "fn:b");
        let c = node("c.ts", "fn:c");
        let store = store_with_nodes_and_edges(
            &[a.clone(), b.clone(), c.clone()],
            &[
                (a.id, b.id, EdgeKind::DefinesBinding),
                (b.id, c.id, EdgeKind::DefinesBinding),
            ],
        );
        let result = bfs(&store, a.id, 1, 4096).unwrap();
        let ids: Vec<NodeId> = result.iter().map(|n| n.id).collect();
        assert!(ids.contains(&a.id));
        assert!(ids.contains(&b.id));
        assert!(
            !ids.contains(&c.id),
            "depth-2 node must be excluded at max_depth=1"
        );
    }

    #[test]
    fn bfs_respects_token_budget() {
        let a = node("a.ts", "fn:a");
        let b = node("b.ts", "fn:b");
        let a_cost = a.vname.signature.len() + a.kind.len();
        let store = store_with_nodes_and_edges(
            &[a.clone(), b.clone()],
            &[(a.id, b.id, EdgeKind::DefinesBinding)],
        );
        // Budget only covers A — B must be excluded.
        let result = bfs(&store, a.id, 3, a_cost).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, a.id);
    }

    #[test]
    fn bfs_handles_cycles() {
        let a = node("a.ts", "fn:a");
        let b = node("b.ts", "fn:b");
        let store = store_with_nodes_and_edges(
            &[a.clone(), b.clone()],
            &[
                (a.id, b.id, EdgeKind::DefinesBinding),
                (b.id, a.id, EdgeKind::DefinesBinding), // cycle
            ],
        );
        // Must terminate — no infinite loop.
        let result = bfs(&store, a.id, 10, 4096).unwrap();
        assert_eq!(result.len(), 2, "both nodes should be visited exactly once");
    }
}
