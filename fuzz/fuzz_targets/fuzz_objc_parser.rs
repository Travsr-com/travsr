//! Fuzz target: Tree-sitter Objective-C parse.
//!
//! Writes arbitrary bytes to a temp `.m` file and parses it through the
//! in-process Objective-C grammar. Tree-sitter must not panic on any byte sequence.
#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "common/mod.rs"]
mod common;

fuzz_target!(|data: &[u8]| {
    common::parse_bytes_as(data, "m");
});
