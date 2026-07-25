//! In-daemon LRU result cache for read-only CLI queries (#318 O2).
//!
//! Keyed by `(tool, args, last_commit, phase_b_commit, data_version)`. The git
//! commit hook advances `last_commit` and the background Phase B pass advances
//! `phase_b_commit` (#318 O3); `data_version` is SQLite's `PRAGMA data_version`
//! as seen by the daemon's read connection, which increments on *any* write to
//! `graph.db` from another connection — including out-of-band writers such as
//! `travsr fsck --fix` that never touch the commit markers (#464). Together,
//! any graph mutation changes the key. That makes invalidation structural:
//! stale entries simply stop matching and age out via LRU eviction — there is
//! no explicit `invalidate()` to forget to call on a mutation path.
//!
//! The cache is not thread-safe on its own; the daemon wraps it in a `Mutex`.

use std::collections::HashMap;

/// Cache key: the query identity plus the markers that bound graph freshness —
/// the two commit markers and the read connection's SQLite `data_version`.
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
}

/// Bounded LRU over serialized query results.
///
/// Eviction is exact LRU: every access stamps the entry with a monotonically
/// increasing tick, and the entry with the smallest tick is dropped when
/// capacity is exceeded. Eviction is `O(n)` but `n` is the (small) capacity and
/// `put` is rare relative to `get`, so the linear scan is not a hot path.
pub struct QueryCache {
    cap: usize,
    tick: u64,
    entries: HashMap<CacheKey, (serde_json::Value, u64)>,
}

impl QueryCache {
    /// Create a cache holding at most `cap` results (minimum 1).
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            tick: 0,
            entries: HashMap::new(),
        }
    }

    fn key(
        tool: &str,
        args: &serde_json::Value,
        last_commit: &str,
        phase_b_commit: &str,
        data_version: u64,
    ) -> CacheKey {
        CacheKey {
            tool: tool.to_string(),
            args: serde_json::to_string(args).unwrap_or_default(),
            last_commit: last_commit.to_string(),
            phase_b_commit: phase_b_commit.to_string(),
            data_version,
        }
    }

    /// Look up a cached result, marking it most-recently-used on a hit.
    pub fn get(
        &mut self,
        tool: &str,
        args: &serde_json::Value,
        last_commit: &str,
        phase_b_commit: &str,
        data_version: u64,
    ) -> Option<serde_json::Value> {
        let k = Self::key(tool, args, last_commit, phase_b_commit, data_version);
        self.tick = self.tick.wrapping_add(1);
        let tick = self.tick;
        let entry = self.entries.get_mut(&k)?;
        entry.1 = tick;
        Some(entry.0.clone())
    }

    /// Insert (or refresh) a result, evicting the least-recently-used entry if
    /// the cache is over capacity.
    pub fn put(
        &mut self,
        tool: &str,
        args: &serde_json::Value,
        last_commit: &str,
        phase_b_commit: &str,
        data_version: u64,
        value: serde_json::Value,
    ) {
        let k = Self::key(tool, args, last_commit, phase_b_commit, data_version);
        self.tick = self.tick.wrapping_add(1);
        let tick = self.tick;
        self.entries.insert(k, (value, tick));
        if self.entries.len() > self.cap {
            self.evict_lru();
        }
    }

    fn evict_lru(&mut self) {
        if let Some(victim) = self
            .entries
            .iter()
            .min_by_key(|(_, (_, t))| *t)
            .map(|(k, _)| k.clone())
        {
            self.entries.remove(&victim);
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hit_after_put_same_key() {
        let mut c = QueryCache::new(8);
        let args = json!({"seed": "Foo", "direction": "both"});
        assert!(c.get("graph", &args, "abc", "abc", 1).is_none());
        c.put("graph", &args, "abc", "abc", 1, json!({"nodes": 3}));
        assert_eq!(
            c.get("graph", &args, "abc", "abc", 1),
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
            1,
            json!("hit"),
        );
        assert_eq!(
            c.get("graph", &json!({"b": 2, "a": 1}), "c1", "c1", 1),
            Some(json!("hit"))
        );
    }

    #[test]
    fn commit_advance_misses() {
        // A new last_commit (Phase A reindex) or phase_b_commit (background
        // Phase B) must miss — structural invalidation.
        let mut c = QueryCache::new(8);
        let args = json!({"seed": "Foo"});
        c.put("graph", &args, "c1", "p1", 1, json!("old"));
        assert!(
            c.get("graph", &args, "c2", "p1", 1).is_none(),
            "last_commit moved"
        );
        assert!(
            c.get("graph", &args, "c1", "p2", 1).is_none(),
            "phase_b moved"
        );
        assert_eq!(c.get("graph", &args, "c1", "p1", 1), Some(json!("old")));
    }

    #[test]
    fn data_version_advance_misses() {
        // #464: an out-of-band write to graph.db (fsck --fix, manual sqlite3)
        // bumps the read connection's data_version without touching either
        // commit marker — the cached entry must stop matching.
        let mut c = QueryCache::new(8);
        let args = json!({"query": "isPrime"});
        c.put("ask", &args, "c1", "p1", 1, json!("pre-delete"));
        assert!(
            c.get("ask", &args, "c1", "p1", 2).is_none(),
            "data_version moved"
        );
        assert_eq!(
            c.get("ask", &args, "c1", "p1", 1),
            Some(json!("pre-delete"))
        );
    }

    #[test]
    fn evicts_least_recently_used() {
        let mut c = QueryCache::new(2);
        let a = json!({"k": "a"});
        let b = json!({"k": "b"});
        let d = json!({"k": "d"});
        c.put("graph", &a, "c", "c", 1, json!(1));
        c.put("graph", &b, "c", "c", 1, json!(2));
        // Touch `a` so `b` becomes the LRU victim.
        assert!(c.get("graph", &a, "c", "c", 1).is_some());
        c.put("graph", &d, "c", "c", 1, json!(3));
        assert_eq!(c.len(), 2);
        assert!(
            c.get("graph", &b, "c", "c", 1).is_none(),
            "b should be evicted"
        );
        assert!(
            c.get("graph", &a, "c", "c", 1).is_some(),
            "a was kept (recent)"
        );
        assert!(c.get("graph", &d, "c", "c", 1).is_some(), "d is newest");
    }
}
