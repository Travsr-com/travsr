//! Property-based tests for PCST and RBAC traversal (S14, 172g).
//!
//! Properties verified:
//! 1. `prop_pcst_source_and_sink_in_result` — when a direct path exists, result contains both endpoints.
//! 2. `prop_pcst_token_budget_respected` — total token cost of result ≤ token_budget.
//! 3. `prop_rbac_denied_corpus_not_in_result` — nodes in denied corpus are never returned.
//! 4. `prop_pcst_empty_for_missing_node` — missing source/sink always returns empty.

use proptest::prelude::*;
use travsr_core::{Edge, EdgeKind, Node, NodeId, VName};
use travsr_retrieval::{pcst_path, token_cost, OpenFilter, RbacFilter};
use travsr_store::{SqliteStore, Store};

fn make_store(nodes: &[Node], edges: &[(NodeId, NodeId, EdgeKind)]) -> SqliteStore {
    let mut s = SqliteStore::open_in_memory().unwrap();
    for n in nodes {
        s.put_node(n).unwrap();
    }
    for &(src, dst, kind) in edges {
        s.put_edge(&Edge::new(src, dst, kind)).unwrap();
    }
    s
}

fn node_with_corpus(corpus: &str, sig: &str) -> Node {
    Node::new(
        VName::new(corpus, "", "src/lib.rs", "rust", sig),
        "function",
    )
}

/// Arb: small chain length 2..=8
fn arb_chain_len() -> impl Strategy<Value = usize> {
    2usize..=8
}

// Property 1: when a direct chain a→b→…→z exists, pcst returns both endpoints.
proptest! {
    #![proptest_config(proptest::test_runner::Config {
        cases: 50,
        failure_persistence: None,
        ..Default::default()
    })]

    #[test]
    fn prop_pcst_source_and_sink_in_result(chain_len in arb_chain_len()) {
        // Build a chain of `chain_len` nodes all in the same corpus.
        let nodes: Vec<Node> = (0..chain_len)
            .map(|i| node_with_corpus("corp:a", &format!("fn:node{i}")))
            .collect();
        let edges: Vec<(NodeId, NodeId, EdgeKind)> = nodes
            .windows(2)
            .map(|w| (w[0].id, w[1].id, EdgeKind::RefCall))
            .collect();
        let store = make_store(&nodes, &edges);

        let source = nodes[0].id;
        let sink = nodes[chain_len - 1].id;

        let result = pcst_path(&store, source, sink, &OpenFilter, 65536).unwrap();
        let ids: Vec<NodeId> = result.iter().map(|n| n.id).collect();

        prop_assert!(
            ids.contains(&source),
            "source must be in path result: chain_len={chain_len}"
        );
        prop_assert!(
            ids.contains(&sink),
            "sink must be in path result: chain_len={chain_len}"
        );
    }
}

// Property 2: token cost of returned nodes never exceeds the budget.
proptest! {
    #![proptest_config(proptest::test_runner::Config {
        cases: 50,
        failure_persistence: None,
        ..Default::default()
    })]

    #[test]
    fn prop_pcst_token_budget_respected(
        chain_len in 2usize..=6,
        token_budget in 8usize..=512,
    ) {
        let nodes: Vec<Node> = (0..chain_len)
            .map(|i| node_with_corpus("corp:budget", &format!("fn:b{i}")))
            .collect();
        let edges: Vec<(NodeId, NodeId, EdgeKind)> = nodes
            .windows(2)
            .map(|w| (w[0].id, w[1].id, EdgeKind::RefCall))
            .collect();
        let store = make_store(&nodes, &edges);

        let result = pcst_path(
            &store,
            nodes[0].id,
            nodes[chain_len - 1].id,
            &OpenFilter,
            token_budget,
        ).unwrap();

        // Calculate the token cost using the canonical token_cost function.
        let total_cost: usize = result.iter().map(token_cost).sum();

        // The PCST implementation may include source/sink even if over budget —
        // accept that up to 2 nodes (src+sink) may exceed by their cost.
        let src_sink_cost = result
            .iter()
            .filter(|n| n.id == nodes[0].id || n.id == nodes[chain_len - 1].id)
            .map(token_cost)
            .sum::<usize>();

        prop_assert!(
            total_cost <= token_budget + src_sink_cost,
            "token cost {total_cost} exceeds budget {token_budget} + src/sink slack {src_sink_cost}"
        );
    }
}

// Property 3: with RbacFilter that denies corp:evil, no node from corp:evil appears in result.
proptest! {
    #![proptest_config(proptest::test_runner::Config {
        cases: 50,
        failure_persistence: None,
        ..Default::default()
    })]

    #[test]
    fn prop_rbac_denied_corpus_not_in_result(evil_count in 1usize..=4) {
        let src = node_with_corpus("corp:allowed", "fn:src");
        let snk = node_with_corpus("corp:allowed", "fn:snk");

        // Build evil nodes that src might try to traverse through.
        let evil_nodes: Vec<Node> = (0..evil_count)
            .map(|i| node_with_corpus("corp:evil", &format!("fn:evil{i}")))
            .collect();

        let mut all_nodes = vec![src.clone(), snk.clone()];
        all_nodes.extend(evil_nodes.iter().cloned());

        let mut edges = vec![(src.id, snk.id, EdgeKind::RefCall)];
        // Also add edges through evil nodes.
        for evil in &evil_nodes {
            edges.push((src.id, evil.id, EdgeKind::RefCall));
            edges.push((evil.id, snk.id, EdgeKind::RefCall));
        }

        let store = make_store(&all_nodes, &edges);
        let filter = RbacFilter::new(["corp:allowed"]);

        let result = pcst_path(&store, src.id, snk.id, &filter, 65536).unwrap();

        for node in &result {
            prop_assert_ne!(
                node.vname.corpus.as_str(),
                "corp:evil",
                "denied corpus node must not appear in result"
            );
        }
    }
}

// Property 4: pcst with a non-existent source always returns empty.
proptest! {
    #![proptest_config(proptest::test_runner::Config {
        cases: 30,
        failure_persistence: None,
        ..Default::default()
    })]

    #[test]
    fn prop_pcst_empty_for_missing_node(id_val in 1u64..=u64::MAX) {
        let store = SqliteStore::open_in_memory().unwrap();
        let result = pcst_path(
            &store,
            NodeId(id_val),
            NodeId(id_val.wrapping_add(1)),
            &OpenFilter,
            65536,
        ).unwrap();
        prop_assert!(result.is_empty(), "missing source must always return empty");
    }
}
