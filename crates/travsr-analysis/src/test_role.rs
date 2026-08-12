//! Issue #479: index-time test classification from tree-sitter captures.
//!
//! Detection is *data* (two capture names every language's query contributes),
//! evaluation is *one language-agnostic function* (this module). There is no
//! per-language Rust branching here — a grammar decides what is a test by which
//! nodes it tags `@test.entry` / `@test.scope`, and the asymmetric-cost rule
//! (an attribute/annotation is decisive; a bare name pattern needs a second
//! corroborating signal) is enforced in each language's query, not here.
//!
//! ## Correlation
//!
//! The Phase-A walk emits one graph [`Node`](travsr_core::Node) per definition,
//! each carrying its 1-based start `line`. The `@test.entry` / `@test.scope`
//! captures land on nodes in the same tree; we correlate by **line span**: a
//! definition whose start line falls inside an `@test.entry` span is an entry
//! point, one inside an `@test.scope` span is support code. Line spans avoid any
//! per-push byte bookkeeping (every parser already records `line`) and are
//! OS-independent (tree-sitter positions, not file paths).

use travsr_core::TestRole;

/// The capture name a grammar uses to tag a **test entry point** — the exact
/// declaration a runner invokes (`#[test] fn`, `@Test`-annotated method,
/// `func TestX(t *testing.T)`, …). Not present in any `capture_kinds`, so the
/// generic walk routes it here instead of emitting a node. Capture it on the
/// name node (a 1-line span) so only the entry itself is classified.
pub const TEST_ENTRY_CAPTURE: &str = "test.entry";

/// The capture name a grammar uses to tag a **test scope** — a container
/// (`#[cfg(test)] mod`, a `TestCase` subclass body, …) whose every nested
/// declaration is test support code. Capture it on the container node so its
/// whole line span covers the members.
pub const TEST_SCOPE_CAPTURE: &str = "test.scope";

/// 1-based inclusive line span `[start, end]`.
type LineSpan = (u32, u32);

/// Line-span signals collected from a file's `@test.entry` / `@test.scope`
/// captures during the Phase-A walk. Language-agnostic.
#[derive(Debug, Default, Clone)]
pub struct TestSignals {
    entries: Vec<LineSpan>,
    scopes: Vec<LineSpan>,
}

impl TestSignals {
    /// True when no test captures fired — lets the caller skip the post-pass
    /// entirely (the overwhelmingly common non-test file).
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.scopes.is_empty()
    }

    /// Record a `@test.entry` line span (from `tree_sitter::Node` start/end row).
    pub fn push_entry_span(&mut self, start_row: usize, end_row: usize) {
        self.entries
            .push((start_row as u32 + 1, end_row as u32 + 1));
    }

    /// Record a `@test.scope` line span.
    pub fn push_scope_span(&mut self, start_row: usize, end_row: usize) {
        self.scopes.push((start_row as u32 + 1, end_row as u32 + 1));
    }

    /// Route a capture named [`TEST_ENTRY_CAPTURE`] / [`TEST_SCOPE_CAPTURE`] to
    /// the right bucket, taking its start/end row (0-based, as tree-sitter
    /// reports). Returns `true` if `cap_name` was one of the two, so the caller
    /// knows to skip emitting a node for it. Any other name is ignored.
    pub fn route_capture(&mut self, cap_name: &str, start_row: usize, end_row: usize) -> bool {
        match cap_name {
            TEST_ENTRY_CAPTURE => {
                self.push_entry_span(start_row, end_row);
                true
            }
            TEST_SCOPE_CAPTURE => {
                self.push_scope_span(start_row, end_row);
                true
            }
            _ => false,
        }
    }

    /// Classify a definition by its 1-based start `line`.
    ///
    /// 1. line ∈ some `@test.entry` span → [`TestRole::EntryPoint`]
    /// 2. line ∈ some `@test.scope` span → [`TestRole::Support`]
    /// 3. otherwise                      → [`TestRole::None`]
    ///
    /// Entry wins over scope so a `#[test] fn` inside a `#[cfg(test)] mod` is an
    /// `EntryPoint`, not `Support`.
    pub fn role_for_line(&self, line: u32) -> TestRole {
        if self.entries.iter().any(|(s, e)| *s <= line && line <= *e) {
            return TestRole::EntryPoint;
        }
        if self.scopes.iter().any(|(s, e)| *s <= line && line <= *e) {
            return TestRole::Support;
        }
        TestRole::None
    }
}

/// Set each node's `test_role`, preferring the AST captures and falling back to
/// the path (#479 Phase 2).
///
/// Priority (asymmetric-cost, §2):
/// 1. an `@test.entry` / `@test.scope` capture span containing the node's start
///    `line` — the only source that can yield [`TestRole::EntryPoint`];
/// 2. otherwise [`travsr_core::noise::test_role_from_path`] on the node's path —
///    yields at most [`TestRole::Support`], covering test-directory declarations
///    that emit no capture (and languages with no Phase-1 capture rule:
///    C/C++/Dart/Obj-C).
///
/// A node whose `test_role` a collector already set (Rust detects in the
/// collector) is left untouched. Nodes without a `line` still get the path
/// fallback (a test file's synthetic file node classifies as `Support`).
pub fn apply_test_roles(signals: &TestSignals, nodes: &mut [travsr_core::Node]) {
    for node in nodes.iter_mut() {
        if node.test_role != TestRole::None {
            continue; // already classified by a collector
        }
        // 1. AST capture span (entry wins over scope; see `role_for_line`).
        if let Some(line) = node.line {
            let role = signals.role_for_line(line);
            if role != TestRole::None {
                node.test_role = role;
                continue;
            }
        }
        // 2. Path fallback (Support only).
        let role = travsr_core::noise::test_role_from_path(&node.vname.path);
        if role != TestRole::None {
            node.test_role = role;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_beats_scope_and_span_containment() {
        let mut s = TestSignals::default();
        s.push_scope_span(9, 49); // #[cfg(test)] mod spanning lines 10..=50
        s.push_entry_span(11, 11); // a #[test] fn name on line 12
        assert_eq!(s.role_for_line(12), TestRole::EntryPoint); // entry wins
        assert_eq!(s.role_for_line(30), TestRole::Support); // helper in scope
        assert_eq!(s.role_for_line(200), TestRole::None); // outside everything
    }

    #[test]
    fn empty_signals_yield_none() {
        let s = TestSignals::default();
        assert!(s.is_empty());
        assert_eq!(s.role_for_line(1), TestRole::None);
    }

    #[test]
    fn route_capture_recognizes_only_the_two_names() {
        let mut s = TestSignals::default();
        assert!(s.route_capture(TEST_ENTRY_CAPTURE, 0, 0));
        assert!(s.route_capture(TEST_SCOPE_CAPTURE, 5, 9));
        assert!(!s.route_capture("fn.name", 10, 10));
        assert_eq!(s.role_for_line(1), TestRole::EntryPoint);
        assert_eq!(s.role_for_line(7), TestRole::Support);
    }
}
