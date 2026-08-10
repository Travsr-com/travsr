//! Corpus-scoped RBAC filter for graph traversal (RFC-006, S14).
//!
//! Access control is enforced **at traversal time**, not post-filter.
//! Post-filtering leaks the existence of restricted nodes via timing side-channels.
//!
//! # SEC invariants (RFC-006)
//! - SEC P0: callers must return identical "not found" responses for both
//!   "node does not exist" and "node exists but access is denied".
//! - SEC P1-C1: Cross-corpus FFI edges are only traversable if both
//!   endpoint nodes are in `allowed_corpora`.

use std::collections::HashSet;

use travsr_core::NodeId;

/// Gate applied to every edge during BFS / PCST traversal.
///
/// The filter receives the destination node's corpus string (from `vname.corpus`)
/// and decides whether the traversal may cross to that node.
///
/// Implementations must be `Send + Sync` — they are shared across BFS/PCST
/// traversal which may run concurrently in a Tokio task.
pub trait EdgeFilter: Send + Sync {
    /// Return `true` if traversal may proceed to `dst`.
    ///
    /// `dst_corpus` is `Some(corpus)` when the destination node's corpus is
    /// known at the call site, and `None` when the corpus cannot be determined
    /// (e.g. the node is missing from the store). Implementations must fail
    /// **closed**: when the corpus is unknown, deny access.
    fn allow(&self, _src: NodeId, dst: NodeId, dst_corpus: Option<&str>) -> bool;

    /// Whether this filter's verdict can depend on `dst_corpus`.
    ///
    /// #413: PPR's BFS has to know each destination's corpus to gate on it,
    /// and that means one extra batched node lookup per edge chunk on the
    /// primary retrieval path. A filter that ignores the corpus entirely gains
    /// nothing from those queries, so it can decline them and keep the local
    /// single-repo path at exactly the cost it had before RBAC was threaded
    /// through.
    ///
    /// Defaults to `true`: a filter that does not override this is assumed to
    /// need the corpus, so a new implementation fails safe rather than
    /// silently receiving `None` and denying (or worse, allowing) everything.
    fn needs_corpus(&self) -> bool {
        true
    }
}

/// Open filter — allows all traversal. Used in unauthenticated local mode
/// where process isolation IS the auth boundary (RFC-006 §3.1).
pub struct OpenFilter;

impl EdgeFilter for OpenFilter {
    fn allow(&self, _src: NodeId, _dst: NodeId, _dst_corpus: Option<&str>) -> bool {
        true
    }

    /// Allows everything regardless of corpus, so the lookups that would
    /// supply it are pure cost (#413).
    fn needs_corpus(&self) -> bool {
        false
    }
}

/// Corpus-scoped RBAC filter. Only traverses to nodes whose `vname.corpus`
/// is in `allowed_corpora`. Unknown corpus → deny (fail-closed).
pub struct RbacFilter {
    pub(crate) allowed_corpora: HashSet<String>,
}

impl RbacFilter {
    pub fn new(allowed: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            allowed_corpora: allowed.into_iter().map(Into::into).collect(),
        }
    }
}

impl EdgeFilter for RbacFilter {
    fn allow(&self, _src: NodeId, _dst: NodeId, dst_corpus: Option<&str>) -> bool {
        match dst_corpus {
            Some(c) => self.allowed_corpora.contains(c),
            None => false, // fail-closed: unknown corpus is denied
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use travsr_core::NodeId;

    fn id(n: u64) -> NodeId {
        NodeId(n)
    }

    #[test]
    fn open_filter_allows_everything() {
        let f = OpenFilter;
        assert!(f.allow(id(1), id(2), Some("corp:a")));
        assert!(f.allow(id(1), id(2), None));
    }

    #[test]
    fn rbac_filter_allows_permitted_corpus() {
        let f = RbacFilter::new(["corp:a", "corp:b"]);
        assert!(f.allow(id(1), id(2), Some("corp:a")));
        assert!(f.allow(id(1), id(2), Some("corp:b")));
    }

    #[test]
    fn rbac_filter_denies_unknown_corpus() {
        let f = RbacFilter::new(["corp:a"]);
        assert!(!f.allow(id(1), id(2), Some("corp:evil")));
    }

    #[test]
    fn rbac_filter_denies_missing_corpus_fail_closed() {
        let f = RbacFilter::new(["corp:a"]);
        assert!(!f.allow(id(1), id(2), None));
    }

    #[test]
    fn rbac_filter_empty_set_denies_all() {
        let f = RbacFilter::new(std::iter::empty::<String>());
        assert!(!f.allow(id(1), id(2), Some("corp:a")));
        assert!(!f.allow(id(1), id(2), None));
    }
}
