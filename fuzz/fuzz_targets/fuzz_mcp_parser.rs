//! Fuzz target: JSON-RPC 2.0 request deserialisation.
//!
//! Feeds arbitrary bytes through `serde_json::from_str::<RpcRequest>`.
//! A panic here means the parser is not panic-safe on malformed input.
#![no_main]

use libfuzzer_sys::fuzz_target;
use travsr_mcp::RpcRequest;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let _: Result<RpcRequest, _> = serde_json::from_str(s);
    }
});
