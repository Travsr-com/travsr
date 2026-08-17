//! In-memory session store for Graph RBAC (RFC-006, S14).
//!
//! `SessionStore` is the authoritative runtime store; the SQLite `sessions` table
//! is a write-only persistence mirror (populated via `travsr-store`, not here).
//!
//! # SEC invariants
//! - SEC P0: `SessionId` is never logged in raw form. Use `session_id_log_hash`
//!   to produce a loggable BLAKE3-derived token.
//! - Sessions expire on `get()` — expiry is not only enforced at creation time.
//! - `DashMap` provides lock-free concurrent reads — suitable for the p95 50ms budget.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use dashmap::DashMap;

/// Opaque session identifier: 32 random bytes.
///
/// Never log the raw bytes. Use [`session_id_log_hash`] to produce a safe handle.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(pub [u8; 32]);

impl SessionId {
    /// Generate a new random session ID using the OS CSPRNG.
    ///
    /// Uses `getrandom` (OS-backed CSPRNG) for all 32 bytes of entropy.
    /// Panics if the OS CSPRNG is unavailable (should never happen on any
    /// supported platform). Cloud tier S16 may switch to a ring-based KDF
    /// for additional auditability, but this is the correct minimum bar.
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        getrandom::getrandom(&mut bytes).expect("OS CSPRNG unavailable");
        Self(bytes)
    }

    /// Hex representation for display (not for logging — use `session_id_log_hash`).
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Parse from a hex string (64 chars).
    pub fn from_hex(hex: &str) -> Option<Self> {
        if hex.len() != 64 {
            return None;
        }
        let mut bytes = [0u8; 32];
        for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
            let hi = hex_nibble(chunk[0])?;
            let lo = hex_nibble(chunk[1])?;
            bytes[i] = (hi << 4) | lo;
        }
        Some(Self(bytes))
    }
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Produce a loggable, non-reversible handle for a session ID.
///
/// Uses BLAKE3 of the raw bytes. The raw ID is never logged.
pub fn session_id_log_hash(id: &SessionId) -> String {
    let hash = blake3::hash(&id.0);
    let hex: String = hash.as_bytes()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("session:{hex}")
}

/// An active session: a set of corpora the holder may traverse.
#[derive(Debug, Clone)]
pub struct Session {
    pub id: SessionId,
    /// Corpora this session may read. Empty = access denied to everything.
    pub allowed_corpora: HashSet<String>,
    pub expires_at: Instant,
}

impl Session {
    pub fn is_expired(&self) -> bool {
        Instant::now() > self.expires_at
    }

    pub fn filter(&self) -> travsr_retrieval::RbacFilter {
        travsr_retrieval::RbacFilter::new(self.allowed_corpora.iter().cloned())
    }
}

/// Default session TTL (24 hours). Configurable in S16.
pub const DEFAULT_SESSION_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// In-memory session registry backed by `DashMap` for lock-free concurrent reads.
///
/// The authoritative store. SQLite persistence is a write-only mirror.
pub struct SessionStore {
    sessions: DashMap<[u8; 32], Session>,
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStore {
    pub fn new() -> Self {
        Self {
            sessions: DashMap::new(),
        }
    }

    /// Create a new session with the given allowed corpora and TTL.
    pub fn create(
        &self,
        allowed_corpora: impl IntoIterator<Item = impl Into<String>>,
        ttl: Duration,
    ) -> Session {
        let id = SessionId::generate();
        let session = Session {
            id: id.clone(),
            allowed_corpora: allowed_corpora.into_iter().map(Into::into).collect(),
            expires_at: Instant::now() + ttl,
        };
        // Key is the raw [u8; 32] array — Copy, no heap allocation on lookup.
        self.sessions.insert(id.0, session.clone());
        tracing::debug!(
            session = %session_id_log_hash(&id),
            "session created"
        );
        session
    }

    /// Look up a session by ID. Returns `None` if not found or expired.
    ///
    /// Expired sessions are evicted on lookup (lazy expiry).
    pub fn get(&self, id: &SessionId) -> Option<Session> {
        let session = self.sessions.get(&id.0)?.clone();
        if session.is_expired() {
            drop(session);
            self.sessions.remove(&id.0);
            tracing::debug!(
                session = %session_id_log_hash(id),
                "session expired and evicted on lookup"
            );
            return None;
        }
        Some(session)
    }

    /// Explicitly revoke a session.
    pub fn revoke(&self, id: &SessionId) {
        if self.sessions.remove(&id.0).is_some() {
            tracing::debug!(
                session = %session_id_log_hash(id),
                "session revoked"
            );
        }
    }

    /// Evict all expired sessions. Called periodically to bound memory usage.
    ///
    /// #736 item 6: lazy expiry on `get()` alone is not a memory bound — a
    /// session that is created but never looked up again is retained until
    /// process exit. The SSE server's maintenance task (spawned in
    /// `sse::router`) calls this on an interval to sweep those.
    pub fn evict_expired(&self) {
        let now = Instant::now();
        self.sessions.retain(|_, s| s.expires_at > now);
    }

    pub fn active_count(&self) -> usize {
        self.sessions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn session_id_generate_is_unique() {
        let a = SessionId::generate();
        let b = SessionId::generate();
        assert_ne!(a.0, b.0, "consecutive session IDs must differ");
    }

    #[test]
    fn session_id_hex_round_trips() {
        let id = SessionId::generate();
        let hex = id.to_hex();
        assert_eq!(hex.len(), 64);
        let parsed = SessionId::from_hex(&hex).expect("valid hex must parse");
        assert_eq!(id.0, parsed.0);
    }

    #[test]
    fn session_store_create_and_get() {
        let store = SessionStore::new();
        let session = store.create(["corp:a", "corp:b"], DEFAULT_SESSION_TTL);
        let fetched = store.get(&session.id).expect("session must be found");
        assert_eq!(fetched.id, session.id);
        assert!(fetched.allowed_corpora.contains("corp:a"));
        assert!(fetched.allowed_corpora.contains("corp:b"));
    }

    #[test]
    fn session_store_get_missing_returns_none() {
        let store = SessionStore::new();
        let id = SessionId::generate();
        assert!(store.get(&id).is_none());
    }

    #[test]
    fn session_store_expired_returns_none() {
        let store = SessionStore::new();
        let session = store.create(["corp:a"], Duration::from_nanos(1));
        // Wait for expiry — 1ns TTL is already expired by the time we call get.
        std::thread::sleep(Duration::from_millis(1));
        assert!(
            store.get(&session.id).is_none(),
            "expired session must return None"
        );
    }

    #[test]
    fn session_store_revoke() {
        let store = SessionStore::new();
        let session = store.create(["corp:a"], DEFAULT_SESSION_TTL);
        store.revoke(&session.id);
        assert!(
            store.get(&session.id).is_none(),
            "revoked session must be gone"
        );
    }

    #[test]
    fn evict_expired_sweeps_without_lookup() {
        // #736 item 6: the periodic sweep must remove expired sessions that are
        // never passed to `get()` — the lazy path can never reach them.
        let store = SessionStore::new();
        let expired = store.create(["corp:a"], Duration::from_nanos(1));
        let live = store.create(["corp:b"], DEFAULT_SESSION_TTL);
        std::thread::sleep(Duration::from_millis(1));

        assert_eq!(store.active_count(), 2, "both retained before the sweep");
        store.evict_expired();
        assert_eq!(store.active_count(), 1, "expired session must be swept");
        assert!(store.get(&live.id).is_some(), "live session must survive");
        assert!(store.get(&expired.id).is_none());
    }

    #[test]
    fn session_log_hash_does_not_leak_raw_id() {
        let id = SessionId::generate();
        let log_token = session_id_log_hash(&id);
        let raw_hex = id.to_hex();
        assert!(
            !log_token.contains(&raw_hex),
            "log hash must not contain raw ID"
        );
        assert!(
            log_token.starts_with("session:"),
            "log token must have prefix"
        );
    }

    #[test]
    fn session_filter_returns_rbac_filter() {
        let store = SessionStore::new();
        let session = store.create(["corp:allowed"], DEFAULT_SESSION_TTL);
        let filter = session.filter();
        use travsr_core::NodeId;
        use travsr_retrieval::EdgeFilter;
        assert!(filter.allow(NodeId(1), NodeId(2), Some("corp:allowed")));
        assert!(!filter.allow(NodeId(1), NodeId(2), Some("corp:denied")));
    }
}
