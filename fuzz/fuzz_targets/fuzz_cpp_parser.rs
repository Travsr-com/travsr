//! Fuzz target: Tree-sitter C++ parse.
//!
//! Writes arbitrary bytes to a temp `.cpp` file and passes it to
//! `Indexer::parse_file`. Tree-sitter must not panic on any byte sequence.
#![no_main]

use libfuzzer_sys::fuzz_target;
use travsr_indexer::Indexer;

fuzz_target!(|data: &[u8]| {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("input.cpp");
    std::fs::write(&path, data).unwrap();
    let _ = Indexer::new().parse_file(&path);
});
