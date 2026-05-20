//! Criterion benchmarks for the travsr-retrieval algorithms.
//!
//! Two graph shapes exercise different traversal characteristics:
//!
//!   chain  — linear N-node path; stresses depth-first iteration and the
//!             token-budget accumulator. BFS time is O(N).
//!
//!   fan    — single hub connected to N-1 leaves; stresses breadth-first
//!             queue expansion. BFS time is O(N) but with larger frontier.
//!
//! PPR benchmarks use the chain shape only (it is computationally heavier than
//! BFS so smaller N values are sufficient to produce stable timings).

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use travsr_core::{Edge, EdgeKind, Node, NodeId, VName};
use travsr_retrieval::{bfs, ppr};
use travsr_store::{SqliteStore, Store};

// ---------------------------------------------------------------------------
// Graph fixtures
// ---------------------------------------------------------------------------

fn make_node(i: usize) -> Node {
    Node::new(
        VName::new("", "", &format!("file_{i}.ts"), "typescript", &format!("fn:{i}")),
        "function",
    )
}

/// Linear chain: node[0] → node[1] → … → node[n-1].
/// Returns the store and the seed (node[0].id).
fn chain(n: usize) -> (SqliteStore, NodeId) {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let nodes: Vec<Node> = (0..n).map(make_node).collect();
    for node in &nodes {
        store.put_node(node).unwrap();
    }
    for i in 0..n - 1 {
        store
            .put_edge(&Edge::new(nodes[i].id, nodes[i + 1].id, EdgeKind::RefCall))
            .unwrap();
    }
    (store, nodes[0].id)
}

/// Star: hub connected to n-1 leaves via DefinesBinding edges.
/// Returns the store and the hub's NodeId.
fn fan(n: usize) -> (SqliteStore, NodeId) {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let nodes: Vec<Node> = (0..n).map(make_node).collect();
    for node in &nodes {
        store.put_node(node).unwrap();
    }
    for leaf in &nodes[1..] {
        store
            .put_edge(&Edge::new(nodes[0].id, leaf.id, EdgeKind::DefinesBinding))
            .unwrap();
    }
    (store, nodes[0].id)
}

// ---------------------------------------------------------------------------
// BFS benchmarks
// ---------------------------------------------------------------------------

fn bench_bfs_chain(c: &mut Criterion) {
    let mut group = c.benchmark_group("bfs/chain");
    for n in [100usize, 1_000, 10_000] {
        let (store, seed) = chain(n);
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| bfs(&store, seed, u8::MAX, usize::MAX).unwrap());
        });
    }
    group.finish();
}

fn bench_bfs_fan(c: &mut Criterion) {
    let mut group = c.benchmark_group("bfs/fan");
    for n in [100usize, 1_000, 10_000] {
        let (store, seed) = fan(n);
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| bfs(&store, seed, 1, usize::MAX).unwrap());
        });
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// PPR benchmarks
// ---------------------------------------------------------------------------

fn bench_ppr_chain(c: &mut Criterion) {
    let mut group = c.benchmark_group("ppr/chain");
    for n in [100usize, 1_000] {
        let (store, seed) = chain(n);
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| ppr(&store, &[seed], 20).unwrap());
        });
    }
    group.finish();
}

// ---------------------------------------------------------------------------

criterion_group!(benches, bench_bfs_chain, bench_bfs_fan, bench_ppr_chain);
criterion_main!(benches);
