//! Fuzz target: Tree-sitter Go parse.
//!
//! Writes arbitrary bytes to a temp `.go` file and passes it to
//! `Indexer::parse_file`. Tree-sitter must not panic on any byte sequence.
#![no_main]

use libfuzzer_sys::fuzz_target;

use travsr_indexer::Indexer;

fuzz_target!(|data: &[u8]| {
    // Unwrap is intentional: tempfile creation failing here is a system-level
    // failure unrelated to the fuzz input, not a bug in the indexer.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("input.go");
    std::fs::write(&path, data).unwrap();
    let _ = Indexer::new().parse_file(&path);
});
