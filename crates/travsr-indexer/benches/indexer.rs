//! Criterion benchmarks for the travsr-indexer parsing pipeline.
//!
//! Benchmark groups (added in order):
//!
//!   rust/cold   — parse `simple.rs` fixture from scratch on every iteration.
//!                 Measures the full Tree-sitter query + node-emit path for a
//!                 typical small Rust file (57 lines, 19 nodes).
//!
//!   rust/warm   — parse `simple.rs` **twice** in the same `b.iter()` call so
//!                 the second parse sees a hot instruction cache. Measures the
//!                 incremental re-index path where the file was just parsed.
//!
//!   rust/travsr_core — parse `crates/travsr-core/src/lib.rs` from the real
//!                      Travsr workspace.  This is the "Travsr indexes itself"
//!                      smoke benchmark and exercises a larger, denser file.
//!                      Skipped gracefully when the workspace root is absent
//!                      (vendor-only CI).
//!
//!   python/cold — parse `simple.py` fixture from scratch each iteration.
//!                 Exercises the Python Tree-sitter grammar + node-emit path
//!                 (QA-221 / Sprint 10).
//!
//!   python/warm — parse `simple.py` **twice** per `b.iter()` call so the
//!                 second parse sees a warm instruction cache.
//!
//!   go/cold         — parse `simple.go` fixture from scratch each iteration.
//!                     Exercises the Go Tree-sitter grammar + node-emit path
//!                     (INDEX-170 / Sprint 12).
//!
//!   go/warm         — parse `simple.go` **twice** per `b.iter()` call so the
//!                     second parse sees a warm instruction cache.
//!
//!   go/kubectl_sample — parse `kubectl_sample.go` (~380-line controller fixture
//!                     representative of real-world Go). Exit criterion: < 30s
//!                     total wall time for 10k-LOC equivalent (Issue #170).
//!
//! Phase 3 exit criterion (Issue #121): query p95 < 50 ms.
//! The 1k-node query benchmark lives in `travsr-retrieval`; this file covers
//! the indexer (parse) side of the latency budget.

use std::path::{Path, PathBuf};

use criterion::{criterion_group, criterion_main, Criterion};
use travsr_indexer::Indexer;

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

fn simple_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust/simple.rs")
}

fn python_simple_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/python/simple.py")
}

fn go_simple_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/go/simple.go")
}

fn go_kubectl_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/go/kubectl_sample.go")
}

fn travsr_core_lib() -> Option<PathBuf> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent() // crates/
        .and_then(|p| p.parent()) // workspace root
        .map(|root| root.join("crates/travsr-core/src/lib.rs"))?;
    p.exists().then_some(p)
}

// ---------------------------------------------------------------------------
// Benchmarks
// ---------------------------------------------------------------------------

/// Cold parse of the simple.rs fixture — full Tree-sitter pipeline each iter.
fn bench_rust_cold(c: &mut Criterion) {
    let fixture = simple_fixture();
    let indexer = Indexer::new();
    let mut group = c.benchmark_group("rust/cold");
    group.bench_function("simple_rs", |b| {
        b.iter(|| {
            indexer
                .parse_file_with_vname(&fixture, "src/simple.rs")
                .unwrap()
        });
    });
    group.finish();
}

/// Warm re-index of the simple.rs fixture — **two** parses per `b.iter()` call
/// so the second parse sees a hot instruction cache. The first parse primes the
/// dcache and icache; the second parse is the timed "warm re-index" path.
fn bench_rust_warm(c: &mut Criterion) {
    let fixture = simple_fixture();
    let indexer = Indexer::new();
    let mut group = c.benchmark_group("rust/warm");
    group.bench_function("simple_rs", |b| {
        b.iter(|| {
            let _ = indexer
                .parse_file_with_vname(&fixture, "src/simple.rs")
                .unwrap();
            indexer
                .parse_file_with_vname(&fixture, "src/simple.rs")
                .unwrap()
        });
    });
    group.finish();
}

/// Parse the real travsr-core/src/lib.rs from the workspace.
/// Skipped silently when not running from a full workspace checkout.
fn bench_rust_travsr_core(c: &mut Criterion) {
    let Some(lib_path) = travsr_core_lib() else {
        eprintln!("bench_rust_travsr_core: skipped — travsr-core/src/lib.rs not found");
        return;
    };
    let indexer = Indexer::new();
    let mut group = c.benchmark_group("rust/travsr_core");
    group.bench_function("lib_rs", |b| {
        b.iter(|| {
            indexer
                .parse_file_with_vname(&lib_path, "crates/travsr-core/src/lib.rs")
                .unwrap()
        });
    });
    group.finish();
}

/// Cold parse of simple.py — full Python Tree-sitter grammar + node-emit path (QA-221).
fn bench_python_cold(c: &mut Criterion) {
    let fixture = python_simple_fixture();
    let indexer = Indexer::new();
    let mut group = c.benchmark_group("python/cold");
    group.bench_function("simple_py", |b| {
        b.iter(|| {
            indexer
                .parse_file_with_vname(&fixture, "src/simple.py")
                .unwrap()
        });
    });
    group.finish();
}

/// Warm re-index of simple.py — **two** parses per `b.iter()` call so the
/// second parse sees a hot instruction cache (QA-221).
fn bench_python_warm(c: &mut Criterion) {
    let fixture = python_simple_fixture();
    let indexer = Indexer::new();
    let mut group = c.benchmark_group("python/warm");
    group.bench_function("simple_py", |b| {
        b.iter(|| {
            let _ = indexer
                .parse_file_with_vname(&fixture, "src/simple.py")
                .unwrap();
            indexer
                .parse_file_with_vname(&fixture, "src/simple.py")
                .unwrap()
        });
    });
    group.finish();
}

/// Cold parse of simple.go — full Go Tree-sitter grammar + node-emit path (INDEX-170).
fn bench_go_cold(c: &mut Criterion) {
    let fixture = go_simple_fixture();
    let indexer = Indexer::new();
    let mut group = c.benchmark_group("go/cold");
    group.bench_function("simple_go", |b| {
        b.iter(|| {
            indexer
                .parse_file_with_vname(&fixture, "src/simple.go")
                .unwrap()
        });
    });
    group.finish();
}

/// Warm re-index of simple.go — **two** parses per `b.iter()` call so the
/// second parse sees a hot instruction cache (INDEX-170).
fn bench_go_warm(c: &mut Criterion) {
    let fixture = go_simple_fixture();
    let indexer = Indexer::new();
    let mut group = c.benchmark_group("go/warm");
    group.bench_function("simple_go", |b| {
        b.iter(|| {
            let _ = indexer
                .parse_file_with_vname(&fixture, "src/simple.go")
                .unwrap();
            indexer
                .parse_file_with_vname(&fixture, "src/simple.go")
                .unwrap()
        });
    });
    group.finish();
}

/// Parse kubectl_sample.go — a ~380-line controller fixture representative of
/// real-world Go (structs, interfaces, methods, generics, type aliases).
/// Exit criterion: < 30 s total for 10k-LOC equivalent (Issue #170).
fn bench_go_kubectl_sample(c: &mut Criterion) {
    let fixture = go_kubectl_fixture();
    let indexer = Indexer::new();
    let mut group = c.benchmark_group("go/kubectl_sample");
    group.bench_function("kubectl_sample_go", |b| {
        b.iter(|| {
            indexer
                .parse_file_with_vname(&fixture, "controller/kubectl_sample.go")
                .unwrap()
        });
    });
    group.finish();
}

// ---------------------------------------------------------------------------

criterion_group!(
    benches,
    bench_rust_cold,
    bench_rust_warm,
    bench_rust_travsr_core,
    bench_python_cold,
    bench_python_warm,
    bench_go_cold,
    bench_go_warm,
    bench_go_kubectl_sample,
);
criterion_main!(benches);
