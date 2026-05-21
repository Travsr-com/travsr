//! Phase 2 exit criterion: LSIF ingest of a 1k-symbol dump in < 30 s (S7-2).
//!
//! Generates a synthetic LSIF JSON-Lines dump with 1 000 resultSet vertices
//! (one per TypeScript symbol) and 500 item/references edges connecting them.
//! The ingest must complete in under 30 seconds on any CI runner.

use std::fmt::Write as _;
use std::time::Instant;

use travsr_indexer::ingest_lsif;

/// Generate a synthetic LSIF dump with `n_symbols` resultSet vertices and
/// `n_refs` item/references edges.  The format matches the subset that
/// `travsr_indexer::lsif::ingest` actually parses.
fn generate_lsif_dump(n_symbols: usize, n_refs: usize) -> String {
    let mut out = String::with_capacity(n_symbols * 200);

    // metaData vertex
    writeln!(
        out,
        r#"{{"id":0,"type":"vertex","label":"metaData","projectRoot":"file:///project","version":"0.4.3"}}"#
    )
    .unwrap();

    // document vertex — one source file
    writeln!(
        out,
        r#"{{"id":1,"type":"vertex","label":"document","uri":"file:///project/src/index.ts","languageId":"typescript"}}"#
    )
    .unwrap();

    // resultSet vertices with travsr_vname extensions
    let rs_start = 2u64;
    for i in 0..n_symbols {
        let id = rs_start + i as u64;
        writeln!(
            out,
            r#"{{"id":{id},"type":"vertex","label":"resultSet","travsr_vname":{{"path":"src/index.ts","signature":"fn:symbol_{i}"}}}}"#
        )
        .unwrap();
    }

    // referenceResult vertices
    let rr_start = rs_start + n_symbols as u64;
    for i in 0..n_refs {
        let rr_id = rr_start + i as u64;
        writeln!(
            out,
            r#"{{"id":{rr_id},"type":"vertex","label":"referenceResult"}}"#
        )
        .unwrap();
    }

    // textDocument/references edges: each resultSet → its referenceResult
    let edge_start = rr_start + n_refs as u64;
    for i in 0..n_refs {
        let eid = edge_start + i as u64;
        let rs_id = rs_start + (i % n_symbols) as u64;
        let rr_id = rr_start + i as u64;
        writeln!(
            out,
            r#"{{"id":{eid},"type":"edge","label":"textDocument/references","outV":{rs_id},"inV":{rr_id}}}"#
        )
        .unwrap();
    }

    // item/references edges: inV = referenceResult, outVs = [some resultSet]
    let item_start = edge_start + n_refs as u64;
    for i in 0..n_refs {
        let eid = item_start + i as u64;
        let rr_id = rr_start + i as u64;
        // Reference from the next symbol in the ring
        let ref_rs_id = rs_start + ((i + 1) % n_symbols) as u64;
        let doc_id = 1u64;
        writeln!(
            out,
            r#"{{"id":{eid},"type":"edge","label":"item","property":"references","outV":{rr_id},"inVs":[{ref_rs_id}],"document":{doc_id}}}"#
        )
        .unwrap();
    }

    out
}

#[test]
fn lsif_ingest_1k_symbols_under_30s() {
    let dump = generate_lsif_dump(1_000, 500);

    let start = Instant::now();
    let out = ingest_lsif(&dump).expect("LSIF ingest must succeed");
    let elapsed = start.elapsed();

    // Correctness: edges were emitted
    assert!(
        !out.edges.is_empty(),
        "ingest must produce at least one edge from 500 item/references entries"
    );

    // Performance gate: must complete in < 30 s on any CI runner
    assert!(
        elapsed.as_secs() < 30,
        "LSIF ingest of 1k symbols took {}ms — exceeds 30s budget",
        elapsed.as_millis()
    );
}
