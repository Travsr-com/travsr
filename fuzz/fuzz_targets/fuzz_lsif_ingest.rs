//! Fuzz target: LSIF dump ingest.
//!
//! Feeds arbitrary UTF-8 bytes through `travsr_indexer::ingest_lsif`.
//! The function must not panic on any input; `Err` is an acceptable outcome.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = travsr_indexer::ingest_lsif(s, "");
    }
});
