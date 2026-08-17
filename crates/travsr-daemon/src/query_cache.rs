//! In-daemon LRU result cache for read-only CLI queries (#318 O2).
//!
//! Keyed by `(tool, args, last_commit, phase_b_commit, data_version,
//! embed_data_version)`. The git commit hook advances `last_commit` and the
//! background Phase B pass advances `phase_b_commit` (#318 O3); `data_version`
//! is SQLite's `PRAGMA data_version` as seen by the daemon's read connection,
//! which increments on *any* write to `graph.db` from another connection —
//! including out-of-band writers such as `travsr fsck --fix` that never touch
//! the commit markers (#464). `embed_data_version` is the same pragma on the
//! sibling `embed.db`: `ask` results depend on stored embeddings (KNN +
//! RFC-019 cosine oracle), and an embed reindex rewrites embed.db without any
//! graph.db write, so graph state alone cannot see it. The daemon passes
//! `None` both while no embed.db exists and for tools that never read
//! embed.db (`graph`, `status`), so the embed sidecar's batched writes cannot
//! thrash entries that don't depend on them.
//! Together, any graph or embed mutation changes the key. That makes
//! invalidation structural: stale entries simply stop matching and age out via
//! LRU eviction — there is no explicit `invalidate()` to forget to call on a
//! mutation path.
//!
//! The cache is not thread-safe on its own; the daemon wraps it in a `Mutex`.

use std::collections::HashMap;

/// The two `PRAGMA data_version` readings that, together with the commit
/// markers, bound result freshness (#464 and follow-up).
#[derive(Clone, Copy)]
pub struct DataVersions {
    /// graph.db's `data_version` as seen by the daemon's read connection.
    pub graph: u64,
    /// embed.db's `data_version`. `None` while no embed.db exists (FTS-only
    /// mode) and for tools that never read embed.db; `Some(version)` for
    /// embed-dependent tools once the sidecar has created it. The two states
    /// must not collide, so this stays an `Option` rather than a sentinel.
    pub embed: Option<u64>,
}

/// Cache key: the query identity plus the markers that bound graph freshness —
/// the two commit markers and the SQLite `data_version` of graph.db and
/// embed.db as seen by the daemon's persistent read connections.
///
/// `args` is the canonical JSON serialization of the query args. A
/// `serde_json::Value` object serializes its keys in sorted (`BTreeMap`) order,
/// so two equal queries always produce an equal key string regardless of how
/// the caller built the argument object.
#[derive(Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    tool: String,
    args: String,
    last_commit: String,
    phase_b_commit: String,
    data_version: u64,
    embed_data_version: Option<u64>,
}

/// Byte budget across all cached values (issue #736 B1).
///
/// The entry-count cap alone does not bound memory: values are whole query
/// results, and a single legitimate `graph <hub> --budget 0` response can
/// reach 256 MiB (see `travsr-ipc`'s `MAX_RESPONSE_BYTES`), so 256 entries
/// could resident multiple GB. The byte budget makes the worst case a known
/// constant; typical entries are small, so the effective capacity for normal
/// workloads is still the entry cap.
const MAX_CACHE_BYTES: usize = 64 * 1024 * 1024;

/// Bounded LRU over serialized query results.
///
/// Eviction is exact LRU: every access stamps the entry with a monotonically
/// increasing tick, and the entry with the smallest tick is dropped when
/// either the entry cap or the byte budget is exceeded. Eviction is `O(n)`
/// but `n` is the (small) capacity and `put` is rare relative to `get`, so
/// the linear scan is not a hot path.
pub struct QueryCache {
    cap: usize,
    tick: u64,
    /// Value, LRU tick, and the value's approximate resident bytes (computed
    /// once at insert so eviction never has to re-measure).
    entries: HashMap<CacheKey, (serde_json::Value, u64, usize)>,
    /// Sum of the per-entry byte estimates, kept under `MAX_CACHE_BYTES`.
    total_bytes: usize,
}

impl QueryCache {
    /// Create a cache holding at most `cap` results (minimum 1).
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            tick: 0,
            entries: HashMap::new(),
            total_bytes: 0,
        }
    }

    fn key(
        tool: &str,
        args: &serde_json::Value,
        last_commit: &str,
        phase_b_commit: &str,
        versions: DataVersions,
    ) -> CacheKey {
        CacheKey {
            tool: tool.to_string(),
            args: serde_json::to_string(args).unwrap_or_default(),
            last_commit: last_commit.to_string(),
            phase_b_commit: phase_b_commit.to_string(),
            data_version: versions.graph,
            embed_data_version: versions.embed,
        }
    }

    /// Look up a cached result, marking it most-recently-used on a hit.
    pub fn get(
        &mut self,
        tool: &str,
        args: &serde_json::Value,
        last_commit: &str,
        phase_b_commit: &str,
        versions: DataVersions,
    ) -> Option<serde_json::Value> {
        let k = Self::key(tool, args, last_commit, phase_b_commit, versions);
        self.tick = self.tick.wrapping_add(1);
        let tick = self.tick;
        let entry = self.entries.get_mut(&k)?;
        entry.1 = tick;
        Some(entry.0.clone())
    }

    /// Insert (or refresh) a result, evicting least-recently-used entries
    /// while either the entry cap or the byte budget is exceeded.
    pub fn put(
        &mut self,
        tool: &str,
        args: &serde_json::Value,
        last_commit: &str,
        phase_b_commit: &str,
        versions: DataVersions,
        value: serde_json::Value,
    ) {
        let cost = approx_value_bytes(&value);
        // A value bigger than the whole budget would evict everything and
        // still not fit — refuse it; the query simply stays uncached.
        if cost > MAX_CACHE_BYTES {
            return;
        }
        let k = Self::key(tool, args, last_commit, phase_b_commit, versions);
        self.tick = self.tick.wrapping_add(1);
        let tick = self.tick;
        if let Some((_, _, prev_cost)) = self.entries.insert(k, (value, tick, cost)) {
            self.total_bytes = self.total_bytes.saturating_sub(prev_cost);
        }
        self.total_bytes += cost;
        while self.entries.len() > self.cap || self.total_bytes > MAX_CACHE_BYTES {
            if !self.evict_lru() {
                break;
            }
        }
    }

    /// Evict the least-recently-used entry. Returns false when empty.
    fn evict_lru(&mut self) -> bool {
        let Some(victim) = self
            .entries
            .iter()
            .min_by_key(|(_, (_, t, _))| *t)
            .map(|(k, _)| k.clone())
        else {
            return false;
        };
        if let Some((_, _, cost)) = self.entries.remove(&victim) {
            self.total_bytes = self.total_bytes.saturating_sub(cost);
        }
        true
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Approximate resident bytes of a JSON value, without serializing it.
///
/// Proportionality is what matters (the budget bounds growth); exactness is
/// not. String content dominates real payloads; containers get a fixed
/// per-element overhead for their allocation and enum tags.
fn approx_value_bytes(v: &serde_json::Value) -> usize {
    const ELEM_OVERHEAD: usize = 32;
    match v {
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            ELEM_OVERHEAD
        }
        serde_json::Value::String(s) => s.len() + ELEM_OVERHEAD,
        serde_json::Value::Array(items) => {
            items.iter().map(approx_value_bytes).sum::<usize>() + ELEM_OVERHEAD
        }
        serde_json::Value::Object(map) => {
            map.iter()
                .map(|(k, val)| k.len() + approx_value_bytes(val))
                .sum::<usize>()
                + ELEM_OVERHEAD
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Graph-only data versions (no embed.db), the common case in these tests.
    fn dv(graph: u64) -> DataVersions {
        DataVersions { graph, embed: None }
    }

    #[test]
    fn hit_after_put_same_key() {
        let mut c = QueryCache::new(8);
        let args = json!({"seed": "Foo", "direction": "both"});
        assert!(c.get("graph", &args, "abc", "abc", dv(1)).is_none());
        c.put("graph", &args, "abc", "abc", dv(1), json!({"nodes": 3}));
        assert_eq!(
            c.get("graph", &args, "abc", "abc", dv(1)),
            Some(json!({"nodes": 3}))
        );
    }

    #[test]
    fn arg_order_independent_key() {
        // Same logical args built in different key order must collide — the
        // canonical (sorted) JSON serialization guarantees this.
        let mut c = QueryCache::new(8);
        c.put(
            "graph",
            &json!({"a": 1, "b": 2}),
            "c1",
            "c1",
            dv(1),
            json!("hit"),
        );
        assert_eq!(
            c.get("graph", &json!({"b": 2, "a": 1}), "c1", "c1", dv(1)),
            Some(json!("hit"))
        );
    }

    #[test]
    fn commit_advance_misses() {
        // A new last_commit (Phase A reindex) or phase_b_commit (background
        // Phase B) must miss — structural invalidation.
        let mut c = QueryCache::new(8);
        let args = json!({"seed": "Foo"});
        c.put("graph", &args, "c1", "p1", dv(1), json!("old"));
        assert!(
            c.get("graph", &args, "c2", "p1", dv(1)).is_none(),
            "last_commit moved"
        );
        assert!(
            c.get("graph", &args, "c1", "p2", dv(1)).is_none(),
            "phase_b moved"
        );
        assert_eq!(c.get("graph", &args, "c1", "p1", dv(1)), Some(json!("old")));
    }

    #[test]
    fn data_version_advance_misses() {
        // #464: an out-of-band write to graph.db (fsck --fix, manual sqlite3)
        // bumps the read connection's data_version without touching either
        // commit marker — the cached entry must stop matching.
        let mut c = QueryCache::new(8);
        let args = json!({"query": "isPrime"});
        c.put("ask", &args, "c1", "p1", dv(1), json!("pre-delete"));
        assert!(
            c.get("ask", &args, "c1", "p1", dv(2)).is_none(),
            "data_version moved"
        );
        assert_eq!(
            c.get("ask", &args, "c1", "p1", dv(1)),
            Some(json!("pre-delete"))
        );
    }

    #[test]
    fn embed_data_version_advance_misses() {
        // #464 follow-up: an embed reindex rewrites embed.db without any
        // graph.db write — commit markers and graph data_version are all
        // unchanged, so only the embed component can invalidate the entry.
        let mut c = QueryCache::new(8);
        let args = json!({"query": "isPrime"});
        let embedded = |embed: u64| DataVersions {
            graph: 1,
            embed: Some(embed),
        };
        c.put(
            "ask",
            &args,
            "c1",
            "p1",
            embedded(7),
            json!("old embeddings"),
        );
        assert!(
            c.get("ask", &args, "c1", "p1", embedded(8)).is_none(),
            "embed data_version moved"
        );
        // embed.db appearing where there was none is also a state change.
        assert!(
            c.get("ask", &args, "c1", "p1", dv(1)).is_none(),
            "Some(v) vs None must not collide"
        );
        assert_eq!(
            c.get("ask", &args, "c1", "p1", embedded(7)),
            Some(json!("old embeddings"))
        );
    }

    /// #736 B1: the entry cap alone does not bound memory — large values must
    /// trigger eviction well before 256 entries accumulate.
    #[test]
    fn byte_budget_evicts_before_entry_cap() {
        let mut c = QueryCache::new(256);
        // ~8 MB per value → the 64 MB budget holds at most 8 of them.
        let big = "x".repeat(8 * 1024 * 1024);
        for i in 0..20 {
            let args = json!({ "k": i });
            c.put(
                "graph",
                &args,
                "c",
                "c",
                dv(1),
                json!({ "blob": big.clone() }),
            );
        }
        assert!(
            c.total_bytes <= MAX_CACHE_BYTES,
            "total_bytes {} exceeds budget {}",
            c.total_bytes,
            MAX_CACHE_BYTES
        );
        assert!(c.len() < 20, "old large entries must have been evicted");
        // Newest entry is still served.
        assert!(c.get("graph", &json!({"k": 19}), "c", "c", dv(1)).is_some());
        // Oldest was evicted by the byte budget.
        assert!(c.get("graph", &json!({"k": 0}), "c", "c", dv(1)).is_none());
    }

    /// A single value larger than the whole budget must be refused outright.
    #[test]
    fn oversized_value_is_not_cached() {
        let mut c = QueryCache::new(8);
        let args = json!({"k": "big"});
        let huge = "x".repeat(MAX_CACHE_BYTES + 1);
        c.put("graph", &args, "c", "c", dv(1), json!(huge));
        assert!(c.get("graph", &args, "c", "c", dv(1)).is_none());
        assert_eq!(c.total_bytes, 0);
    }

    #[test]
    fn evicts_least_recently_used() {
        let mut c = QueryCache::new(2);
        let a = json!({"k": "a"});
        let b = json!({"k": "b"});
        let d = json!({"k": "d"});
        c.put("graph", &a, "c", "c", dv(1), json!(1));
        c.put("graph", &b, "c", "c", dv(1), json!(2));
        // Touch `a` so `b` becomes the LRU victim.
        assert!(c.get("graph", &a, "c", "c", dv(1)).is_some());
        c.put("graph", &d, "c", "c", dv(1), json!(3));
        assert_eq!(c.len(), 2);
        assert!(
            c.get("graph", &b, "c", "c", dv(1)).is_none(),
            "b should be evicted"
        );
        assert!(
            c.get("graph", &a, "c", "c", dv(1)).is_some(),
            "a was kept (recent)"
        );
        assert!(c.get("graph", &d, "c", "c", dv(1)).is_some(), "d is newest");
    }
}
