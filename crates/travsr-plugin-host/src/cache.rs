use std::collections::{HashMap, VecDeque};
use travsr_plugin_protocol::ParseResponse;

/// Daemon-computed parse cache key. plugin_version prevents stale cached
/// output after a plugin update; file_hash prevents serving old output for
/// changed files. Neither component is ever supplied by the plugin itself.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub plugin_version: String,
    pub file_hash: [u8; 32],
}

/// Byte budget for one `ParseCache` (issue #736 A1).
///
/// The cache exists to dedupe re-parses of identical file content within one
/// indexer's lifetime. Before this bound it held every file's full
/// `ParseResponse` for the whole run — on a monorepo that is a second
/// in-memory copy of the parsed graph, multiplied by the init worker count,
/// and it was the single largest init-time memory consumer.
///
/// A byte budget rather than an entry count because entries vary by orders of
/// magnitude (a 10-line script vs a 20k-line generated file). 64 MB keeps the
/// hit rate for the workloads the cache was built for (repeated hashes within
/// a batch, unchanged files across incremental reindexes) while bounding the
/// worst case at a known constant per indexer.
const MAX_CACHE_BYTES: usize = 64 * 1024 * 1024;

pub struct ParseCache {
    store: HashMap<CacheKey, ParseResponse>,
    /// Approximate resident bytes of all cached values (see `approx_bytes`).
    approx_bytes: usize,
    /// Insertion order for eviction, oldest first. A `VecDeque` because the
    /// byte budget admits tens of thousands of typical-size entries, so once
    /// the cache is full eviction runs on nearly every insert — `pop_front`
    /// must be O(1), not a Vec::remove(0) memmove of the whole queue. FIFO is
    /// the right policy for this cache: within one run a file is parsed once
    /// and hit shortly after, so the oldest entries are the least likely to
    /// be needed again.
    insertion_order: VecDeque<CacheKey>,
}

impl ParseCache {
    pub fn new() -> Self {
        Self {
            store: HashMap::new(),
            approx_bytes: 0,
            insertion_order: VecDeque::new(),
        }
    }

    pub fn get(&self, plugin_version: &str, file_hash: [u8; 32]) -> Option<&ParseResponse> {
        self.store.get(&CacheKey {
            plugin_version: plugin_version.to_string(),
            file_hash,
        })
    }

    pub fn insert(&mut self, key: CacheKey, resp: ParseResponse) {
        let cost = approx_bytes(&resp);
        // An entry bigger than the whole budget is never worth caching: it
        // would evict everything else and then be evicted by the next insert.
        if cost > MAX_CACHE_BYTES {
            return;
        }
        // Credit an existing entry's cost BEFORE the budget check, so a
        // refresh is charged as a delta rather than a wholly new entry —
        // otherwise re-inserting a large value evicts neighbours to make room
        // for bytes that are about to be released anyway.
        let existed = match self.store.remove(&key) {
            Some(prev) => {
                self.approx_bytes = self.approx_bytes.saturating_sub(approx_bytes(&prev));
                true
            }
            None => false,
        };
        // Evict oldest-first until the new entry fits. A refreshed key's own
        // (now dangling) order slot may be popped here — track that so the key
        // still ends up with exactly one slot.
        let mut key_slot_popped = false;
        while self.approx_bytes + cost > MAX_CACHE_BYTES {
            let Some(oldest) = self.insertion_order.pop_front() else {
                break;
            };
            if oldest == key {
                key_slot_popped = true;
                continue;
            }
            if let Some(evicted) = self.store.remove(&oldest) {
                self.approx_bytes = self.approx_bytes.saturating_sub(approx_bytes(&evicted));
            }
        }
        self.store.insert(key.clone(), resp);
        if !existed || key_slot_popped {
            self.insertion_order.push_back(key);
        }
        self.approx_bytes += cost;
    }
}

impl Default for ParseCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Approximate resident size of a cached response.
///
/// Counts the string payloads (which dominate) plus a fixed struct overhead
/// per element. Deliberately an estimate: it only has to be proportional to
/// real memory use for the budget to bound growth, and computing it must stay
/// cheap enough to run on every insert.
fn approx_bytes(resp: &ParseResponse) -> usize {
    const NODE_OVERHEAD: usize = 128; // NodeId + Options + Vec/HashMap slack
    const EDGE_OVERHEAD: usize = 48; // two NodeIds + kind + Option<u8>
    const FFI_OVERHEAD: usize = 160;

    let node_bytes: usize = resp
        .nodes
        .iter()
        .map(|n| {
            n.vname.corpus.len()
                + n.vname.root.len()
                + n.vname.path.len()
                + n.vname.language.len()
                + n.vname.signature.len()
                + n.kind.len()
                + n.package.len()
                + NODE_OVERHEAD
        })
        .sum();
    let edge_bytes = resp.edges.len() * EDGE_OVERHEAD;
    let ffi_bytes = resp.ffi_markers.len() * FFI_OVERHEAD;
    node_bytes + edge_bytes + ffi_bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use travsr_core::{Node, VName};

    fn response_with_nodes(n: usize, sig_len: usize) -> ParseResponse {
        let nodes = (0..n)
            .map(|i| {
                Node::new(
                    VName::new(
                        "github.com/a/b",
                        "",
                        "src/lib.rs",
                        "rust",
                        format!("{i}:{}", "x".repeat(sig_len)),
                    ),
                    "function",
                )
            })
            .collect();
        ParseResponse {
            nodes,
            edges: vec![],
            ffi_markers: vec![],
        }
    }

    fn key(i: u8) -> CacheKey {
        CacheKey {
            plugin_version: "1.0.0".into(),
            file_hash: [i; 32],
        }
    }

    #[test]
    fn hit_and_miss_still_work() {
        let mut cache = ParseCache::new();
        cache.insert(key(1), response_with_nodes(3, 8));
        assert!(cache.get("1.0.0", [1; 32]).is_some());
        assert!(cache.get("1.0.0", [2; 32]).is_none());
        assert!(cache.get("2.0.0", [1; 32]).is_none());
    }

    /// The #736 A1 regression guard: total resident bytes stay under the
    /// budget no matter how many entries are inserted.
    #[test]
    fn cache_never_exceeds_its_byte_budget() {
        let mut cache = ParseCache::new();
        // Each response ≈ 1 MB of signature strings; insert far more than fits.
        let per_entry = 1024;
        for i in 0..200u8 {
            cache.insert(key(i), response_with_nodes(per_entry, 1024));
        }
        assert!(
            cache.approx_bytes <= MAX_CACHE_BYTES,
            "approx_bytes {} exceeds budget {}",
            cache.approx_bytes,
            MAX_CACHE_BYTES
        );
        assert!(
            cache.store.len() < 200,
            "eviction must have removed old entries"
        );
        // Newest entry survives; the very first was evicted.
        assert!(cache.get("1.0.0", [199; 32]).is_some());
        assert!(cache.get("1.0.0", [0; 32]).is_none());
    }

    /// An entry larger than the whole budget must be refused, not allowed to
    /// flush the cache and then thrash.
    #[test]
    fn oversized_entry_is_not_cached() {
        let mut cache = ParseCache::new();
        cache.insert(key(1), response_with_nodes(10, 8));
        let bytes_before = cache.approx_bytes;
        // ~70 MB of signatures > 64 MB budget.
        cache.insert(key(2), response_with_nodes(70 * 1024, 1024));
        assert!(cache.get("1.0.0", [2; 32]).is_none());
        assert!(cache.get("1.0.0", [1; 32]).is_some());
        assert_eq!(cache.approx_bytes, bytes_before);
    }

    /// Re-inserting the same key must replace, not double-count.
    #[test]
    fn reinsert_same_key_does_not_double_count() {
        let mut cache = ParseCache::new();
        cache.insert(key(1), response_with_nodes(10, 64));
        let first = cache.approx_bytes;
        cache.insert(key(1), response_with_nodes(10, 64));
        assert_eq!(cache.approx_bytes, first);
        assert_eq!(cache.insertion_order.len(), 1);
    }

    /// Review follow-up on #736: a refresh must be charged as a delta, not as
    /// a wholly new entry. Two entries that together fill most of the budget,
    /// then a same-size refresh of the newer one — the older entry must
    /// survive, because the refresh releases exactly the bytes it adds.
    #[test]
    fn refresh_does_not_evict_neighbours() {
        let mut cache = ParseCache::new();
        // Two ~29 MB entries (~1.2 KB estimated per node): ~59 MB total,
        // inside the 64 MB budget with room to spare.
        let big = 24 * 1024;
        cache.insert(key(1), response_with_nodes(big, 1024));
        cache.insert(key(2), response_with_nodes(big, 1024));
        assert!(cache.get("1.0.0", [1; 32]).is_some());
        // Refresh key 2 with an equal-cost value.
        cache.insert(key(2), response_with_nodes(big, 1024));
        assert!(
            cache.get("1.0.0", [1; 32]).is_some(),
            "refreshing key 2 must not evict key 1 — the delta is zero"
        );
        assert!(cache.get("1.0.0", [2; 32]).is_some());
        assert!(cache.approx_bytes <= MAX_CACHE_BYTES);
        assert_eq!(cache.insertion_order.len(), 2, "one order slot per key");
    }
}
