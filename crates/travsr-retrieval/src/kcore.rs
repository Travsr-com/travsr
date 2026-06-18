//! K-core decomposition (Batagelyi & Zaversnik, 2003) with shell propagation.
//!
//! Assigns every node a "shell number" (coreness) — the largest k such that
//! the node survives in the k-core subgraph where every node has at least k
//! neighbours.  High-shell nodes are the structural backbone of the codebase.
//!
//! ## Two-phase design
//!
//! Phase 1 — **Peeling**: Computes raw shell numbers from the full undirected
//! graph.  Go-package nodes naturally receive high shells when they sit in dense
//! import clusters; individual symbols (function, method, class, …) receive low
//! shells (≤ 1) because the only edges they participate in are containment edges
//! to their parent file.
//!
//! Phase 2 — **Propagation**: Propagates the go-package shell number down to
//! each contained symbol so that `get_context` boosting is meaningful at the
//! symbol level — a function in a highly-central package inherits that
//! centrality score.
//!
//! Edge direction of `defines/binding` (as emitted by the Go parser):
//! ```text
//! file   → function / var / class / interface / type   (file contains symbol)
//! file   → go-pkg                                       (file belongs to package)
//! class  → method                                       (class contains method)
//! ```
//!
//! Propagation path for symbols:
//! - `function/var/class/interface/type`: `symbol ← file → go-pkg`
//! - `method`:                            `method ← class ← file → go-pkg`
//!
//! # Complexity
//! O(V + E) time and space via bucket-sort peeling + one propagation pass.

use std::collections::{HashMap, HashSet};

use travsr_core::NodeId;
use travsr_error::TravsrError;
use travsr_store::SqliteStore;

/// Compute k-core shell numbers for all nodes in `store`, with package-level
/// propagation so symbol nodes inherit their containing package's shell.
///
/// Returns a map from `NodeId` to coreness (shell number ≥ 0).
pub fn compute_kcore(store: &SqliteStore) -> Result<HashMap<NodeId, u32>, TravsrError> {
    let _span = tracing::debug_span!("kcore.compute").entered();

    // ── Load node kinds + all edges in one pass ───────────────────────────────
    let id_kind_pairs = store
        .all_node_ids_and_kinds()
        .map_err(|e| TravsrError::Internal(e.to_string()))?;

    if id_kind_pairs.is_empty() {
        tracing::debug!(kcore.nodes = 0, "kcore.compute: empty graph");
        return Ok(HashMap::new());
    }

    let n = id_kind_pairs.len();
    let kind_map: HashMap<NodeId, &str> = id_kind_pairs
        .iter()
        .map(|(id, k)| (*id, k.as_str()))
        .collect();

    // Dense index for O(1) array access during peeling.
    let node_ids: Vec<NodeId> = id_kind_pairs.iter().map(|(id, _)| *id).collect();
    let mut id_to_idx: HashMap<NodeId, usize> = HashMap::with_capacity(n);
    for (i, &id) in node_ids.iter().enumerate() {
        id_to_idx.insert(id, i);
    }

    // ── Build undirected neighbour sets + propagation maps ────────────────────
    let mut neighbours: Vec<HashSet<usize>> = vec![HashSet::new(); n];

    // Propagation maps (built during the same edge scan):
    //   file_id   → go-pkg_id          (file→go-pkg defines/binding)
    //   symbol_id → container_id       (file→symbol or class→method defines/binding)
    let mut file_to_pkg: HashMap<NodeId, NodeId> = HashMap::new();
    let mut symbol_to_container: HashMap<NodeId, NodeId> = HashMap::new();

    let all_edges = store
        .all_edges()
        .map_err(|e| TravsrError::Internal(e.to_string()))?;

    for (src, dst, kind, _) in &all_edges {
        // Undirected neighbour sets for peeling (all edge kinds).
        if let (Some(&si), Some(&di)) = (id_to_idx.get(src), id_to_idx.get(dst)) {
            if si != di {
                neighbours[si].insert(di);
                neighbours[di].insert(si);
            }
        }

        // Propagation maps (defines/binding only).
        if kind == "defines/binding" {
            let src_kind = kind_map.get(src).copied().unwrap_or("");
            let dst_kind = kind_map.get(dst).copied().unwrap_or("");
            match (src_kind, dst_kind) {
                // file → go-pkg: the file belongs to this package.
                ("file", "go-pkg") => {
                    file_to_pkg.insert(*src, *dst);
                }
                // file → symbol: file contains symbol; record container.
                ("file", "function" | "var" | "class" | "interface" | "type") => {
                    symbol_to_container.insert(*dst, *src);
                }
                // class → method: class contains method; record container.
                ("class", "method") => {
                    symbol_to_container.insert(*dst, *src);
                }
                _ => {}
            }
        }
    }

    let total_edges = all_edges.len();

    // ── Phase 1: O(V + E) bucket-sort peeling ────────────────────────────────
    let mut degree: Vec<u32> = neighbours.iter().map(|nb| nb.len() as u32).collect();
    let max_degree = degree.iter().copied().max().unwrap_or(0) as usize;

    let mut bin: Vec<usize> = vec![0usize; max_degree + 1];
    for &d in &degree {
        bin[d as usize] += 1;
    }
    let mut start = 0usize;
    for b in &mut bin {
        let cnt = *b;
        *b = start;
        start += cnt;
    }

    let mut order: Vec<usize> = vec![0; n];
    let mut pos: Vec<usize> = vec![0; n];
    let mut bin_cur = bin.clone();
    for v in 0..n {
        let d = degree[v] as usize;
        pos[v] = bin_cur[d];
        order[bin_cur[d]] = v;
        bin_cur[d] += 1;
    }

    let mut shell: Vec<u32> = vec![0; n];
    for i in 0..n {
        let v = order[i];
        shell[v] = degree[v];
        for &u in neighbours[v].iter() {
            if pos[u] <= i {
                continue;
            }
            if degree[u] > degree[v] {
                let du = degree[u] as usize;
                let pu = pos[u];
                let bs = bin[du];
                let w = order[bs];
                order[pu] = w;
                pos[w] = pu;
                order[bs] = u;
                pos[u] = bs;
                bin[du] += 1;
                degree[u] -= 1;
            }
        }
    }

    // Reconstruct NodeId → raw shell number.
    let mut shells: HashMap<NodeId, u32> = node_ids
        .iter()
        .enumerate()
        .map(|(i, &id)| (id, shell[i]))
        .collect();

    // ── Phase 2: Propagate go-pkg shell to contained symbols ─────────────────
    //
    // Without propagation, every function/method/class has shell ≤ 1 because
    // they only touch their containing file via defines/binding edges and have
    // no cross-file edges until Phase B (call edges) is complete.  Propagating
    // the package shell gives symbols a meaningful centrality score that reflects
    // how import-central their package is.
    //
    // Propagation paths:
    //   function/var/class/interface/type:  symbol ← file → go-pkg
    //   method:                             method ← class ← file → go-pkg

    // Build class → file map from symbol_to_container (classes are in that map
    // with their containing file as the value, so we invert only the class entries).
    let class_to_file: HashMap<NodeId, NodeId> = symbol_to_container
        .iter()
        .filter(|(&sym_id, _)| kind_map.get(&sym_id).copied() == Some("class"))
        .map(|(&class_id, &file_id)| (class_id, file_id))
        .collect();

    for (&sym_id, &container_id) in &symbol_to_container {
        let sym_kind = kind_map.get(&sym_id).copied().unwrap_or("");
        let pkg_id = match sym_kind {
            "method" => {
                // method → class → file → go-pkg
                class_to_file
                    .get(&container_id)
                    .and_then(|file_id| file_to_pkg.get(file_id))
            }
            _ => {
                // symbol → file → go-pkg
                file_to_pkg.get(&container_id)
            }
        };
        if let Some(&pkg_id) = pkg_id {
            let pkg_shell = shells.get(&pkg_id).copied().unwrap_or(0);
            let entry = shells.entry(sym_id).or_insert(0);
            *entry = (*entry).max(pkg_shell);
        }
    }

    let max_shell = shells.values().copied().max().unwrap_or(0);
    let max_sym_shell = shells
        .iter()
        .filter(|(id, _)| {
            matches!(
                kind_map.get(id).copied(),
                Some("function" | "method" | "class" | "interface" | "type" | "var")
            )
        })
        .map(|(_, &s)| s)
        .max()
        .unwrap_or(0);

    tracing::debug!(
        kcore.nodes = n,
        kcore.edges = total_edges,
        kcore.max_shell = max_shell,
        kcore.max_symbol_shell = max_sym_shell,
        "kcore.compute complete"
    );

    Ok(shells)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use travsr_core::{Edge, EdgeKind, VName};
    use travsr_store::{SqliteStore, Store};

    fn make_node(sig: &str, kind: &str) -> travsr_core::Node {
        travsr_core::Node::new(VName::new("", "", "test.ts", "typescript", sig), kind)
    }

    fn store_with(
        nodes: &[travsr_core::Node],
        edges: &[(NodeId, NodeId, EdgeKind)],
    ) -> SqliteStore {
        let mut s = SqliteStore::open_in_memory().unwrap();
        for n in nodes {
            s.put_node(n).unwrap();
        }
        for &(src, dst, kind) in edges {
            s.put_edge(&Edge::new(src, dst, kind)).unwrap();
        }
        s
    }

    fn shell(store: &SqliteStore, id: NodeId) -> u32 {
        *compute_kcore(store).unwrap().get(&id).unwrap_or(&0)
    }

    #[test]
    fn empty_graph_returns_empty_map() {
        let s = SqliteStore::open_in_memory().unwrap();
        assert!(compute_kcore(&s).unwrap().is_empty());
    }

    #[test]
    fn isolated_node_has_shell_zero() {
        let a = make_node("fn:a", "function");
        let s = store_with(std::slice::from_ref(&a), &[]);
        assert_eq!(shell(&s, a.id), 0);
    }

    #[test]
    fn chain_a_b_c_gives_shell_one() {
        let a = make_node("fn:a", "function");
        let b = make_node("fn:b", "function");
        let c = make_node("fn:c", "function");
        let s = store_with(
            &[a.clone(), b.clone(), c.clone()],
            &[
                (a.id, b.id, EdgeKind::RefCall),
                (b.id, c.id, EdgeKind::RefCall),
            ],
        );
        let shells = compute_kcore(&s).unwrap();
        assert_eq!(shells[&a.id], 1);
        assert_eq!(shells[&b.id], 1);
        assert_eq!(shells[&c.id], 1);
    }

    #[test]
    fn triangle_gives_shell_two() {
        let a = make_node("fn:a", "function");
        let b = make_node("fn:b", "function");
        let c = make_node("fn:c", "function");
        let s = store_with(
            &[a.clone(), b.clone(), c.clone()],
            &[
                (a.id, b.id, EdgeKind::RefCall),
                (b.id, c.id, EdgeKind::RefCall),
                (c.id, a.id, EdgeKind::RefCall),
            ],
        );
        let shells = compute_kcore(&s).unwrap();
        assert_eq!(shells[&a.id], 2);
        assert_eq!(shells[&b.id], 2);
        assert_eq!(shells[&c.id], 2);
    }

    #[test]
    fn star_graph_all_shell_one() {
        let centre = make_node("fn:centre", "function");
        let leaves: Vec<_> = (0..4)
            .map(|i| make_node(&format!("fn:leaf{i}"), "function"))
            .collect();
        let mut all_nodes = vec![centre.clone()];
        all_nodes.extend(leaves.iter().cloned());
        let edges: Vec<(NodeId, NodeId, EdgeKind)> = leaves
            .iter()
            .map(|l| (centre.id, l.id, EdgeKind::RefCall))
            .collect();
        let s = store_with(&all_nodes, &edges);
        let shells = compute_kcore(&s).unwrap();
        assert_eq!(shells[&centre.id], 1);
        for leaf in &leaves {
            assert_eq!(shells[&leaf.id], 1);
        }
    }

    #[test]
    fn clique_of_four_gives_shell_three() {
        let nodes: Vec<_> = (0..4)
            .map(|i| make_node(&format!("fn:n{i}"), "function"))
            .collect();
        let mut edges = Vec::new();
        for i in 0..4 {
            for j in (i + 1)..4 {
                edges.push((nodes[i].id, nodes[j].id, EdgeKind::RefCall));
            }
        }
        let s = store_with(&nodes, &edges);
        let shells = compute_kcore(&s).unwrap();
        for n in &nodes {
            assert_eq!(shells[&n.id], 3);
        }
    }

    #[test]
    fn disconnected_components_scored_independently() {
        let a = make_node("fn:a", "function");
        let b = make_node("fn:b", "function");
        let c = make_node("fn:c", "function");
        let iso = make_node("fn:iso", "function");
        let s = store_with(
            &[a.clone(), b.clone(), c.clone(), iso.clone()],
            &[
                (a.id, b.id, EdgeKind::RefCall),
                (b.id, c.id, EdgeKind::RefCall),
                (c.id, a.id, EdgeKind::RefCall),
            ],
        );
        let shells = compute_kcore(&s).unwrap();
        assert_eq!(shells[&a.id], 2);
        assert_eq!(shells[&b.id], 2);
        assert_eq!(shells[&c.id], 2);
        assert_eq!(shells[&iso.id], 0);
    }

    #[test]
    fn multi_edges_counted_once() {
        let a = make_node("fn:a", "function");
        let b = make_node("fn:b", "function");
        let mut s = SqliteStore::open_in_memory().unwrap();
        s.put_node(&a).unwrap();
        s.put_node(&b).unwrap();
        s.put_edge(&Edge::new(a.id, b.id, EdgeKind::RefCall))
            .unwrap();
        s.put_edge(&Edge::new(a.id, b.id, EdgeKind::Depends))
            .unwrap();
        let shells = compute_kcore(&s).unwrap();
        assert_eq!(shells[&a.id], 1);
        assert_eq!(shells[&b.id], 1);
    }

    #[test]
    fn self_loop_ignored() {
        let a = make_node("fn:a", "function");
        let mut s = SqliteStore::open_in_memory().unwrap();
        s.put_node(&a).unwrap();
        s.put_edge(&Edge::new(a.id, a.id, EdgeKind::RefCall))
            .unwrap();
        let shells = compute_kcore(&s).unwrap();
        assert_eq!(shells[&a.id], 0);
    }

    #[test]
    fn directed_edges_treated_as_undirected() {
        let a = make_node("fn:a", "function");
        let b = make_node("fn:b", "function");
        let s = store_with(&[a.clone(), b.clone()], &[(a.id, b.id, EdgeKind::RefCall)]);
        let shells = compute_kcore(&s).unwrap();
        assert_eq!(shells[&a.id], 1);
        assert_eq!(shells[&b.id], 1);
    }

    #[test]
    fn propagation_gives_method_its_package_shell_via_class() {
        // method → class → file → go-pkg propagation chain.
        let pkg = make_node("go-pkg:mypkg/mypkg", "go-pkg");
        let file = make_node("file:mypkg/a.go", "file");
        let file_b = make_node("file:mypkg/b.go", "file");
        let file_c = make_node("file:mypkg/c.go", "file");
        let cls = make_node("class:MyStruct", "class");
        let method = make_node("method:MyStruct.DoWork", "method");

        let s = store_with(
            &[
                pkg.clone(),
                file.clone(),
                file_b.clone(),
                file_c.clone(),
                cls.clone(),
                method.clone(),
            ],
            &[
                // files → go-pkg
                (file.id, pkg.id, EdgeKind::DefinesBinding),
                (file_b.id, pkg.id, EdgeKind::DefinesBinding),
                (file_c.id, pkg.id, EdgeKind::DefinesBinding),
                // files form a triangle (→ shell 3 for files and pkg)
                (file.id, file_b.id, EdgeKind::Depends),
                (file_b.id, file_c.id, EdgeKind::Depends),
                (file_c.id, file.id, EdgeKind::Depends),
                // file contains class
                (file.id, cls.id, EdgeKind::DefinesBinding),
                // class contains method
                (cls.id, method.id, EdgeKind::DefinesBinding),
            ],
        );

        let shells = compute_kcore(&s).unwrap();
        let pkg_shell = shells[&pkg.id];
        let method_shell = shells[&method.id];

        assert!(
            method_shell >= pkg_shell,
            "method shell ({method_shell}) must be ≥ pkg shell ({pkg_shell}) after propagation"
        );
        assert!(
            method_shell > 0,
            "method shell must be > 0 after propagation"
        );
    }

    #[test]
    fn propagation_gives_function_its_package_shell() {
        // go-pkg node connected to 3 files (defines/binding), those files all
        // connected to each other via depends → go-pkg gets shell ≥ 1.
        // A function in one of those files should inherit the go-pkg shell.
        let pkg = make_node("go-pkg:mypkg/mypkg", "go-pkg");
        let file_a = make_node("file:mypkg/a.go", "file");
        let file_b = make_node("file:mypkg/b.go", "file");
        let file_c = make_node("file:mypkg/c.go", "file");
        let func = make_node("fn:MyFunc", "function");

        let s = store_with(
            &[
                pkg.clone(),
                file_a.clone(),
                file_b.clone(),
                file_c.clone(),
                func.clone(),
            ],
            &[
                // file → go-pkg containment
                (file_a.id, pkg.id, EdgeKind::DefinesBinding),
                (file_b.id, pkg.id, EdgeKind::DefinesBinding),
                (file_c.id, pkg.id, EdgeKind::DefinesBinding),
                // files depend on each other (creates 3-clique → shell 2 for files and pkg)
                (file_a.id, file_b.id, EdgeKind::Depends),
                (file_b.id, file_c.id, EdgeKind::Depends),
                (file_c.id, file_a.id, EdgeKind::Depends),
                // file_a contains func (defines/binding: file → function)
                (file_a.id, func.id, EdgeKind::DefinesBinding),
            ],
        );

        let shells = compute_kcore(&s).unwrap();

        // pkg is in the 3-clique formed with the 3 files → high shell
        let pkg_shell = shells[&pkg.id];
        let func_shell = shells[&func.id];

        // func must have been propagated at least the pkg shell
        assert!(
            func_shell >= pkg_shell,
            "func shell ({func_shell}) must be ≥ pkg shell ({pkg_shell}) after propagation"
        );
        assert!(func_shell > 0, "func shell must be > 0 after propagation");
    }
}
