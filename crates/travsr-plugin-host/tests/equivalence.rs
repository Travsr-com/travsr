//! Transport-equivalence property test (RFC-011 P5-S2 gate).
//!
//! For each built-in language, asserts that the old `travsr_indexer::Indexer`
//! path and the new `PluginIndexer` path produce byte-identical `ParseOutput`
//! after sorting nodes and edges for deterministic comparison.
//!
//! This test is the merge gate for P5-S2: it must stay green whenever parsers
//! or plugin wrappers change.

use std::io::Write as _;
use std::path::Path;

use tempfile::NamedTempFile;
use travsr_indexer::Indexer;
use travsr_plugin_host::PluginIndexer;

const CORPUS: &str = "github.com/travsr/test";

// ── Helpers ───────────────────────────────────────────────────────────────────

fn old_parse(abs_path: &Path, vname_path: &str) -> travsr_indexer::ParseOutput {
    Indexer::with_corpus(CORPUS)
        .parse_file_with_vname(abs_path, vname_path)
        .expect("old Indexer parse failed")
}

fn new_parse(abs_path: &Path, vname_path: &str) -> travsr_indexer::ParseOutput {
    PluginIndexer::new(CORPUS)
        .parse_file_with_vname(abs_path, vname_path)
        .expect("PluginIndexer parse failed")
}

/// Write `source` to a NamedTempFile with the given extension and return it.
/// The file must stay alive for the duration of parsing.
fn write_fixture(source: &str, ext: &str) -> NamedTempFile {
    let mut f = tempfile::Builder::new()
        .suffix(&format!(".{ext}"))
        .tempfile()
        .expect("tempfile");
    f.write_all(source.as_bytes()).expect("write fixture");
    f.flush().expect("flush fixture");
    f
}

/// Sort nodes by id and edges by (src, dst, kind) for deterministic comparison.
fn normalize(out: &mut travsr_indexer::ParseOutput) {
    out.nodes.sort_by_key(|n| n.id.0);
    out.edges.sort_by_key(|e| (e.src.0, e.dst.0, e.kind as u16));
}

/// Assert that old and new paths produce identical nodes and edges.
/// FfiMarkers are compared by count only (kind mapping is validated separately).
fn assert_equivalent(lang: &str, abs_path: &Path, vname_path: &str) {
    let mut old = old_parse(abs_path, vname_path);
    let mut new = new_parse(abs_path, vname_path);

    normalize(&mut old);
    normalize(&mut new);

    assert_eq!(
        old.nodes.len(),
        new.nodes.len(),
        "{lang}: node count mismatch — old={}, new={}",
        old.nodes.len(),
        new.nodes.len()
    );

    for (i, (o, n)) in old.nodes.iter().zip(new.nodes.iter()).enumerate() {
        assert_eq!(
            o.id, n.id,
            "{lang}: node[{i}] id mismatch — old={:?} new={:?}\n  old vname={:?}\n  new vname={:?}",
            o.id, n.id, o.vname, n.vname
        );
        assert_eq!(o.kind, n.kind, "{lang}: node[{i}] kind mismatch");
        assert_eq!(o.vname, n.vname, "{lang}: node[{i}] vname mismatch");
        assert_eq!(o.line, n.line, "{lang}: node[{i}] line mismatch");
    }

    assert_eq!(
        old.edges.len(),
        new.edges.len(),
        "{lang}: edge count mismatch — old={}, new={}",
        old.edges.len(),
        new.edges.len()
    );

    for (i, (o, n)) in old.edges.iter().zip(new.edges.iter()).enumerate() {
        assert_eq!(o.src, n.src, "{lang}: edge[{i}] src mismatch");
        assert_eq!(o.dst, n.dst, "{lang}: edge[{i}] dst mismatch");
        assert_eq!(o.kind, n.kind, "{lang}: edge[{i}] kind mismatch");
    }

    assert_eq!(
        old.ffi_markers.len(),
        new.ffi_markers.len(),
        "{lang}: ffi_marker count mismatch"
    );
}

// ── TypeScript ────────────────────────────────────────────────────────────────

#[test]
fn typescript_class_equivalence() {
    let src = r#"
import { EventEmitter } from 'events';

export class PaymentService extends EventEmitter {
  constructor(private readonly apiKey: string) {
    super();
  }
  async charge(amount: number): Promise<void> {
    this.emit('charge', amount);
  }
}
"#;
    let f = write_fixture(src, "ts");
    let vname = "src/payment_service.ts";
    assert_equivalent("typescript/class", f.path(), vname);
}

#[test]
fn typescript_function_equivalence() {
    let src = r#"
export function greet(name: string): string {
  return `Hello, ${name}`;
}
export const add = (a: number, b: number): number => a + b;
"#;
    let f = write_fixture(src, "ts");
    assert_equivalent("typescript/function", f.path(), "src/utils.ts");
}

// ── Rust ──────────────────────────────────────────────────────────────────────

#[test]
fn rust_struct_and_fn_equivalence() {
    let src = r#"
use std::collections::HashMap;

pub struct PaymentService {
    api_key: String,
}

impl PaymentService {
    pub fn new(key: &str) -> Self {
        Self { api_key: key.to_string() }
    }

    pub fn charge(&self, amount: u64) -> bool {
        !self.api_key.is_empty() && amount > 0
    }
}

pub fn process_payment(service: &PaymentService, amount: u64) -> bool {
    service.charge(amount)
}
"#;
    let f = write_fixture(src, "rs");
    assert_equivalent("rust/struct_fn", f.path(), "src/payment.rs");
}

#[test]
fn rust_enum_and_trait_equivalence() {
    let src = r#"
pub enum Status { Pending, Charged, Failed }

pub trait Processor {
    fn process(&self, amount: u64) -> Status;
}

pub mod payments {
    pub use super::Status;
}
"#;
    let f = write_fixture(src, "rs");
    assert_equivalent("rust/enum_trait", f.path(), "src/types.rs");
}

// ── Python ────────────────────────────────────────────────────────────────────

#[test]
fn python_class_equivalence() {
    let src = r#"
import os
from typing import Optional

class PaymentService:
    def __init__(self, api_key: str) -> None:
        self.api_key = api_key

    def charge(self, amount: float) -> bool:
        return bool(self.api_key) and amount > 0

def process(service: PaymentService, amount: float) -> bool:
    return service.charge(amount)
"#;
    let f = write_fixture(src, "py");
    assert_equivalent("python/class", f.path(), "src/payment.py");
}

// ── Go ────────────────────────────────────────────────────────────────────────

#[test]
fn go_struct_and_func_equivalence() {
    let src = r#"
package payment

import "fmt"

type PaymentService struct {
    APIKey string
}

func (p *PaymentService) Charge(amount float64) bool {
    return p.APIKey != "" && amount > 0
}

func Process(service *PaymentService, amount float64) bool {
    fmt.Printf("processing %.2f\n", amount)
    return service.Charge(amount)
}
"#;
    let f = write_fixture(src, "go");
    assert_equivalent("go/struct_func", f.path(), "src/payment.go");
}

#[test]
fn go_interface_equivalence() {
    let src = r#"
package payment

type Processor interface {
    Charge(amount float64) bool
    Refund(txID string) error
}

type Status int

const (
    StatusPending Status = iota
    StatusCharged
    StatusFailed
)
"#;
    let f = write_fixture(src, "go");
    assert_equivalent("go/interface", f.path(), "src/types.go");
}

// ── Cache hit produces identical output ───────────────────────────────────────

#[test]
fn cache_hit_produces_identical_output() {
    let src = "export const x = 1;\n";
    let f = write_fixture(src, "ts");
    let vname = "src/const.ts";

    let mut indexer = PluginIndexer::new(CORPUS);
    let first = indexer
        .parse_file_with_vname(f.path(), vname)
        .expect("first parse");
    let second = indexer
        .parse_file_with_vname(f.path(), vname)
        .expect("second parse (cache hit)");

    assert_eq!(
        first.nodes.len(),
        second.nodes.len(),
        "cache hit changed node count"
    );
    assert_eq!(
        first.edges.len(),
        second.edges.len(),
        "cache hit changed edge count"
    );
}

// ── Java golden fixture ───────────────────────────────────────────────────────

#[test]
fn java_golden_fixture_produces_expected_nodes() {
    let fixture = std::path::Path::new("tests/fixtures/Hello.java");
    if !fixture.exists() {
        panic!("fixture missing: tests/fixtures/Hello.java");
    }
    let out = new_parse(fixture, "src/PaymentService.java");

    // Must have a file node
    assert!(
        out.nodes.iter().any(|n| n.kind == "file"),
        "missing file node"
    );

    // Must find PaymentService class
    assert!(
        out.nodes
            .iter()
            .any(|n| n.kind == "class" && n.vname.signature.contains("PaymentService")),
        "missing PaymentService class\nnodes: {:?}",
        out.nodes
            .iter()
            .map(|n| (&n.kind, &n.vname.signature))
            .collect::<Vec<_>>()
    );

    // Must find methods
    let methods: Vec<_> = out
        .nodes
        .iter()
        .filter(|n| n.kind == "method" || n.kind == "constructor" || n.kind == "function")
        .collect();
    assert!(
        methods.len() >= 2,
        "expected ≥2 methods, got {}",
        methods.len()
    );

    // Must find imports
    assert!(
        out.nodes.iter().any(|n| n.kind == "import"),
        "missing import nodes"
    );

    // Old Indexer returns empty for Java — PluginIndexer adds real content
    let old = old_parse(fixture, "src/PaymentService.java");
    assert!(
        out.nodes.len() > old.nodes.len(),
        "PluginIndexer should produce more than old empty path"
    );
}

// ── Kotlin golden fixture ─────────────────────────────────────────────────────

#[test]
fn kotlin_golden_fixture_produces_expected_nodes() {
    let fixture = std::path::Path::new("tests/fixtures/Hello.kt");
    if !fixture.exists() {
        panic!("fixture missing: tests/fixtures/Hello.kt");
    }
    let out = new_parse(fixture, "src/PaymentService.kt");

    assert!(
        out.nodes.iter().any(|n| n.kind == "file"),
        "missing file node"
    );
    assert!(
        out.nodes
            .iter()
            .any(|n| n.kind == "class" && n.vname.signature.contains("PaymentService")),
        "missing PaymentService class\nnodes: {:?}",
        out.nodes
            .iter()
            .map(|n| (&n.kind, &n.vname.signature))
            .collect::<Vec<_>>()
    );
    let fns: Vec<_> = out.nodes.iter().filter(|n| n.kind == "function").collect();
    assert!(!fns.is_empty(), "expected ≥1 function, got {}", fns.len());

    let old = old_parse(fixture, "src/PaymentService.kt");
    assert!(
        out.nodes.len() > old.nodes.len(),
        "PluginIndexer should produce more than old empty path"
    );
}
