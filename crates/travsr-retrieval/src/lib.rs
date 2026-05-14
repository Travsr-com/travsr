//! travsr-retrieval — graph traversal and ranking algorithms.
//!
//! MVP ships BFS depth-3. Production adds Personalized PageRank,
//! Prize-Collecting Steiner Tree, k-core decomposition, and 0-1 knapsack
//! for token-budgeted context assembly. See CLAUDE.md "Algorithmic Stack".

#![forbid(unsafe_code)]

use travsr_core::NodeId;
use travsr_store::SqliteStore;

/// Bounded breadth-first traversal from `seed`, returning all node ids
/// reachable within `max_depth` hops.
///
/// Stub — returns the empty set. Implementation lands in Sprint 2.
pub fn bfs(_store: &SqliteStore, _seed: NodeId, _max_depth: u8) -> Vec<NodeId> {
    vec![]
}
