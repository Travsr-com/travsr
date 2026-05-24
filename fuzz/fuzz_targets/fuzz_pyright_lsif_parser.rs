#![no_main]

use libfuzzer_sys::fuzz_target;
use std::path::Path;

fuzz_target!(|data: &[u8]| {
    // Fuzz the JSON-to-ParseOutput adapter (not the live pyright subprocess).
    // Invariant: must not panic; must return Ok or a recognised Err.
    let _ = travsr_indexer::python_lsif::json_to_parse_output(data, Path::new("/fuzz.py"));
});
