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
        VName::new(
            "",
            "",
            &format!("file_{i}.ts"),
            "typescript",
            &format!("fn:{i}"),
        ),
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
    // max_depth is u8 (max 255); chain depth == hop count, so inputs must stay
    // below 256 or the BFS depth-cap fires before all nodes are visited,
    // producing identical timing across larger inputs.
    for n in [50usize, 100, 200] {
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

// ---------------------------------------------------------------------------
// 1k-file fixture — simulates a 1 000-file TypeScript repository
// ---------------------------------------------------------------------------
//
// Topology: one root node (hub) connected to 999 leaf nodes via
// DefinesBinding edges (hub → leaf).  This mirrors a realistic monorepo
// entry-point that imports ~1 000 modules.
//
// BFS depth 1 from the hub visits all 1 000 nodes in a single frontier
// expansion; this is the worst-case breadth-first fan-out at the scale
// where p95 < 50 ms is required (Issue #32 / S7-1).
//
// PPR on the same graph runs power iteration over all 1 000 nodes.
// Both benchmarks are used by `scripts/check-p95.sh` to enforce the
// absolute 50 ms latency gate in CI.

const FIXTURE_1K: usize = 1_000;

fn bench_bfs_1k_fixture(c: &mut Criterion) {
    let mut group = c.benchmark_group("bfs/1k_fixture");
    let (store, seed) = fan(FIXTURE_1K);
    group.bench_function("fan-1000", |b| {
        b.iter(|| bfs(&store, seed, 1, usize::MAX).unwrap());
    });
    group.finish();
}

fn bench_ppr_1k_fixture(c: &mut Criterion) {
    let mut group = c.benchmark_group("ppr/1k_fixture");
    let (store, seed) = fan(FIXTURE_1K);
    group.bench_function("fan-1000", |b| {
        b.iter(|| ppr(&store, &[seed], 20).unwrap());
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_bfs_chain,
    bench_bfs_fan,
    bench_ppr_chain,
    bench_bfs_1k_fixture,
    bench_ppr_1k_fixture,
);
criterion_main!(benches);
