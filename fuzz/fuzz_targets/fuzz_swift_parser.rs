//! Fuzz target: Tree-sitter Swift parse.
//!
//! Writes arbitrary bytes to a temp `.swift` file and parses it through the
//! in-process Swift grammar. Tree-sitter must not panic on any byte sequence.
#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "common/mod.rs"]
mod common;

fuzz_target!(|data: &[u8]| {
    common::parse_bytes_as(data, "swift");
});
