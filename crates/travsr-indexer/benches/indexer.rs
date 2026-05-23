//! Criterion benchmarks for the travsr-indexer parsing pipeline.
//!
//! Three benchmark groups:
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

// ---------------------------------------------------------------------------

criterion_group!(
    benches,
    bench_rust_cold,
    bench_rust_warm,
    bench_rust_travsr_core,
);
criterion_main!(benches);
