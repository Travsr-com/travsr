/// #478 WS-1: one-time generator for the `tokenize_identifier` golden fixture.
///
/// Run manually with `cargo test -p travsr-store --test gen_tokenize_golden -- --ignored`
/// BEFORE the `ident` extraction refactor lands, to snapshot current output. Not part
/// of the regular suite; the permanent regression test lives in `tokenize_golden.rs`
/// and reads the fixture this produces.
use std::io::Write;

#[test]
#[ignore]
fn generate_golden_fixture() {
    let inputs_path = std::env::var("GOLDEN_INPUTS_JSON")
        .expect("set GOLDEN_INPUTS_JSON to the extracted signature/path corpus");
    let raw = std::fs::read_to_string(&inputs_path).expect("read golden inputs");
    let inputs: Vec<serde_json::Value> = serde_json::from_str(&raw).expect("parse golden inputs");

    let mut pairs: Vec<(String, String)> = inputs
        .into_iter()
        .filter_map(|row| row.get("v").and_then(|v| v.as_str()).map(str::to_string))
        .map(|input| {
            let output = travsr_store::fts_tokenize::tokenize_identifier(&input);
            (input, output)
        })
        .collect();
    pairs.sort();

    let out_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/tokenize_golden.json"
    );
    let mut f = std::fs::File::create(out_path).expect("create fixture file");
    let json = serde_json::to_string_pretty(&pairs).expect("serialize fixture");
    f.write_all(json.as_bytes()).expect("write fixture");
}
