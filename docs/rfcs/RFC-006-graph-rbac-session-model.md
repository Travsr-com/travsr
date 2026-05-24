# RFC-006: Graph RBAC Session Model

**Status:** Accepted  
**Author:** Travsr Engineering  
**Date:** 2026-05-24  
**Crate(s) affected:** travsr-store, travsr-retrieval, travsr-mcp

---

## Summary

Introduce a corpus-scoped RBAC layer that enforces access control at graph-traversal time (not post-filter), backed by an in-memory `SessionStore` keyed by opaque session IDs. Nodes carry an `access_corpus` tag; traversal is gated by an `EdgeFilter` trait. This is the minimum viable auth layer before S14's `get_execution_path` tool can safely span multiple corpora.

---

## Motivation

`get_execution_path` traverses arbitrary node paths from `source` to `sink`. Without access control, a compromised MCP client can extract nodes from corpora the requester does not own. The threat model entry T7 (cross-tenant data leak) is directly applicable even in local-first deployments where multiple repos with different sensitivity levels are indexed.

---

## Detailed Design

### 1. Schema change (migration v7)

```sql
-- nodes table gains access_corpus tag (NULL = public)
ALTER TABLE nodes ADD COLUMN access_corpus TEXT;

-- sessions table for persistence (in-memory cache is authoritative)
CREATE TABLE IF NOT EXISTS sessions (
    id       TEXT PRIMARY KEY,
    corpus   TEXT NOT NULL,
    created  INTEGER NOT NULL DEFAULT (unixepoch()),
    expires  INTEGER NOT NULL
);
```

`access_corpus` being NULL means the node is publicly readable by any authenticated session. A non-NULL value means only sessions whose `allowed_corpora` set contains that corpus may see the node.

### 2. EdgeFilter trait

```rust
pub trait EdgeFilter: Send + Sync {
    /// Return true if the traversal may cross this edge.
    fn allow(&self, src: NodeId, dst: NodeId, dst_corpus: Option<&str>) -> bool;
}

pub struct OpenFilter; // allow all — default for unauthenticated local mode

pub struct RbacFilter {
    pub allowed_corpora: FxHashSet<String>,
}
```

`RbacFilter::allow` returns `false` when `dst_corpus` is `Some(c)` and `c` is not in `allowed_corpora`. This is enforced at traversal time in BFS and PCST, not as a post-filter. Post-filtering leaks the existence of excluded nodes via timing.

### 3. SessionStore

```rust
pub struct Session {
    pub id: SessionId,           // opaque, 256-bit random
    pub allowed_corpora: Vec<String>,
    pub expires_at: Instant,
}

pub struct SessionStore {
    sessions: DashMap<SessionId, Session>,
}
```

`SessionId` is a 32-byte array displayed as hex. It is **never logged in raw form**; logs must use `session_id_hash` (BLAKE3 of the session ID). Session expiry is enforced on `get()`, not only on `create()`.

### 4. SEC invariants

- **SEC P0**: `get_execution_path` returns "not found" for both "node does not exist" and "node exists but not in allowed_corpora". These cases are indistinguishable to the caller (prevents existence oracle).
- **SEC P0**: `session_id` is never written to any log at INFO or below. Only `session_id_hash` may appear in DEBUG logs.
- **SEC P1-C1**: Cross-corpus FFI edges (from RFC-005) are allowed iff both endpoints are in `allowed_corpora`.

---

## Alternatives Considered

**Post-filter approach**: Traverse the full graph, then remove forbidden nodes from the result. Rejected because: (1) timing side-channel leaks existence of forbidden nodes, (2) forbidden nodes pollute intermediate traversal paths in PCST.

**Per-query auth token**: Each MCP call carries its own token. Rejected for S14 — adds round-trip latency and token management complexity. Session-based auth is simpler and sufficient for the threat model.

---

## Drawbacks

Session management adds per-call overhead (DashMap lookup ≈ 50ns). Acceptable at the p95 50ms budget. Sessions table in SQLite adds a persistence write path; this is write-only in the hot path (reads use in-memory DashMap).

---

## Unresolved Questions

- Session TTL: default to 24h for S14, configurable in S16.
- Maximum sessions per corpus: not enforced in S14 (local deployments only have O(1) sessions).
- Session revocation: not implemented in S14; expiry-based only.
