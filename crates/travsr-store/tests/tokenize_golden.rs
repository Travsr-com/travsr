/// #478 WS-1 (blocking): `tokenize_identifier` must produce byte-identical
/// output after its segmentation logic moved into `travsr_core::ident`. A
/// diff here means silent index corruption on the next incremental reindex —
/// `nodes_fts`/`fts_vocab` would stop matching what `ident::contains_token`
/// (the anchor guard) expects.
///
/// The fixture (`tests/fixtures/tokenize_golden.json`) was generated from
/// every distinct `signature`/`path` value in this repo's own `.travsr/graph.db`
/// (7853 entries) against the pre-refactor implementation. Regenerate it with
/// `GOLDEN_INPUTS_JSON=<path> cargo test -p travsr-store --test
/// gen_tokenize_golden -- --ignored` only when `tokenize_identifier`'s
/// intended output is deliberately changing.
use travsr_store::fts_tokenize::tokenize_identifier;

#[test]
fn tokenize_identifier_output_is_byte_identical_to_golden_fixture() {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/tokenize_golden.json"
    ))
    .expect("read golden fixture");
    let pairs: Vec<(String, String)> = serde_json::from_str(&raw).expect("parse golden fixture");
    assert!(!pairs.is_empty(), "golden fixture must not be empty");

    let mut mismatches = Vec::new();
    for (input, expected) in &pairs {
        let actual = tokenize_identifier(input);
        if &actual != expected {
            mismatches.push(format!(
                "input={input:?} expected={expected:?} actual={actual:?}"
            ));
        }
    }
    assert!(
        mismatches.is_empty(),
        "tokenize_identifier output diverged from the golden fixture ({} of {} mismatched):\n{}",
        mismatches.len(),
        pairs.len(),
        mismatches.join("\n")
    );
}
