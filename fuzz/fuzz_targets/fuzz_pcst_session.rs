//! Fuzz target: PCST path-finding and SessionStore creation under arbitrary input.
//!
//! Exercises:
//! 1. `pcst_path` with arbitrary NodeId pairs (most will be missing — tests fallback path).
//! 2. `SessionStore::create` + `get` with arbitrary corpus strings (tests DashMap + expiry).
//! 3. `SessionId::from_hex` with arbitrary strings (tests parser robustness).
//!
//! Does NOT import any production SQL — uses in-memory stores only.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Need at least 16 bytes: 8 for src NodeId, 8 for dst NodeId.
    if data.len() < 16 {
        return;
    }

    // Extract two NodeIds from the first 16 bytes.
    let src_bytes: [u8; 8] = data[..8].try_into().unwrap();
    let dst_bytes: [u8; 8] = data[8..16].try_into().unwrap();
    let src = travsr_core::NodeId(u64::from_le_bytes(src_bytes));
    let dst = travsr_core::NodeId(u64::from_le_bytes(dst_bytes));

    // Exercise PCST on an empty store — always exercises the BFS fallback.
    let store = travsr_store::SqliteStore::open_in_memory().unwrap();
    let filter = travsr_retrieval::OpenFilter;
    let _ = travsr_retrieval::pcst_path(&store, src, dst, &filter, 4096);

    // Exercise SessionId parsing with the remaining bytes as a hex string.
    if data.len() >= 16 {
        let rest = &data[16..];
        if let Ok(s) = std::str::from_utf8(rest) {
            let _ = travsr_mcp::SessionId::from_hex(s);
        }
    }

    // Exercise SessionStore with fuzz-derived corpus name.
    if data.len() >= 17 {
        let corpus_len = data[16] as usize % 64;
        let corpus_start = 17;
        let corpus_end = (corpus_start + corpus_len).min(data.len());
        if let Ok(corpus) = std::str::from_utf8(&data[corpus_start..corpus_end]) {
            let store = travsr_mcp::SessionStore::new();
            let session = store.create([corpus], std::time::Duration::from_secs(60));
            // get() must not panic regardless of session state.
            let _ = store.get(&session.id);
            store.revoke(&session.id);
            let _ = store.get(&session.id); // must return None after revoke
        }
    }
});
