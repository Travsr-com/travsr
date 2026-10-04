//! Fuzz target: Tree-sitter Ruby parse.
//!
//! Writes arbitrary bytes to a temp `.rb` file and parses it through the
//! in-process Ruby grammar. Tree-sitter must not panic on any byte sequence.
#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "common/mod.rs"]
mod common;

fuzz_target!(|data: &[u8]| {
    common::parse_bytes_as(data, "rb");
});
