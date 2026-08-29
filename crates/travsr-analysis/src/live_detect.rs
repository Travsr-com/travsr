//! RFC-027 section 8.2 — detection-only reference set for the live LSP lane.
//!
//! The three languages with a native Phase B extractor (Rust, TypeScript/JS,
//! Python) get their on-save reference set from that extractor: a fully typed
//! record carrying the caller's identity, the receiver type where it could be
//! recovered, and a per-language signature key. The other thirteen have Phase A
//! structural nodes and no on-save reference-detection pass at all, which is the
//! single reason the live lane never reached them — resolution, node mapping,
//! target production, emit, ratification, and the precision meter are already
//! language-agnostic.
//!
//! This module closes exactly that gap and nothing else. It answers *"which
//! references exist in this buffer, where, and of what kind"* and stops there:
//!
//! - **No receiver-type recovery.** `x.Foo()` yields the name `Foo` and its
//!   line. What `x` is at that position is the editor's language server's
//!   question, and asking it is the whole point of the LSP lane (RFC-027
//!   section 7.3b).
//! - **No signature building.** A signature is a claim about identity, and
//!   identity belongs to SCIP (section 8.2 fencing rule). Nothing here mints a
//!   VName or names a node.
//! - **No resolution.** The daemon maps positions to nodes against the graph it
//!   already owns.
//!
//! Because it produces no receiver type and no signature key, the fail-closed
//! lexical floor (section 7.3a) cannot consume this output, so these languages
//! run the **editor lane only** (section 8.3). With no language server installed
//! they abstain and keep today's commit-gated behavior exactly, which is the
//! RFC's zero-regression floor (section 7.3c).
//!
//! Adding a language is one tree-sitter query plus an arm in the daemon's gate,
//! and each is measured against the per-`(language, kind)` precision gate
//! (section 12) before it ships enabled.

use anyhow::Context as _;
use streaming_iterator::StreamingIterator as _;
use tree_sitter::{Parser, Query, QueryCursor};

use travsr_core::{InheritanceRef, Language};

/// Upper bound on a buffer this pass will parse.
///
/// Matches the Phase A parsers' own limit. A generated file past this size is
/// not what the live lane exists for, and parsing it on every save would cost
/// more than the freshness is worth.
const MAX_SOURCE_BYTES: usize = 10 * 1024 * 1024;

/// One detected reference: the name as written and the line it sits on.
///
/// The line, not a column: the editor recovers the column by finding `name` on
/// the line, which is far cheaper than teaching every detector to carry UTF-16
/// offsets, and is what the daemon-driven target contract already asks of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveRef {
    /// 1-based source line the reference appears on.
    pub line: u32,
    /// The referenced name exactly as written (`Foo` in `x.Foo()`).
    pub name: String,
}

/// Every reference in one buffer the live lane can act on, bucketed by the edge
/// kind it would become.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiveRefs {
    /// Call sites — `EdgeKind::RefCall`.
    pub calls: Vec<LiveRef>,
    /// Field / property reads that are not calls — `EdgeKind::RefField`.
    pub fields: Vec<LiveRef>,
    /// `extends` / `implements` style clauses — `EdgeKind::IsImplementation`.
    pub inheritance: Vec<InheritanceRef>,
}

impl LiveRefs {
    /// True when nothing was detected, so the caller can skip the whole pass.
    pub fn is_empty(&self) -> bool {
        self.calls.is_empty() && self.fields.is_empty() && self.inheritance.is_empty()
    }
}

/// Detect the live lane's reference set in `source`, or an empty set for a
/// language with no detector yet.
///
/// Returning empty rather than erroring for an unhandled language keeps the
/// daemon's per-language gate the single place a language is turned on: this
/// function is safe to call for anything, and adding a language here without
/// adding it there still ships nothing.
pub fn detect_live_refs(lang: Language, source: &[u8]) -> anyhow::Result<LiveRefs> {
    if source.len() > MAX_SOURCE_BYTES {
        return Ok(LiveRefs::default());
    }
    match lang {
        Language::Go => detect_go(source),
        _ => Ok(LiveRefs::default()),
    }
}

// ── Go ───────────────────────────────────────────────────────────────────────
//
// Two patterns cover every reference shape the lane can use:
//
//   `foo(...)`                 → (call_expression function: (identifier))
//   `x.Foo(...)` / `x.Foo`     → (selector_expression field: (field_identifier))
//
// The selector pattern matches both a method call and a plain field read, and
// tree-sitter queries cannot express "not the callee of a call" directly, so the
// two are told apart from the tree: a selector that *is* the `function` child of
// a `call_expression` is a call, anything else is a field read. That keeps a
// field read out of `RefCall`, so it never surfaces as a caller in
// `get_callers` / `get_blast_radius` while still being a use site (#757).
//
// **Go contributes no inheritance clauses, by design.** It has no `extends` /
// `implements`: interface satisfaction is structural and implicit, and struct
// embedding is composition, not implementation. There is no clause in the
// source that asserts "this type implements that interface", so emitting an
// `IsImplementation` edge would mean inferring one — precisely the guess
// section 8.1 forbids. Go's implements relation stays commit-gated, where the
// SCIP sidecar derives it from real type information.
const GO_QUERY: &str = r#"
(call_expression function: (identifier) @call.name)
(selector_expression field: (field_identifier) @sel.name)
"#;

fn detect_go(source: &[u8]) -> anyhow::Result<LiveRefs> {
    let language = tree_sitter::Language::new(tree_sitter_go::LANGUAGE);
    let query = Query::new(&language, GO_QUERY).context("compiling Go live-detect query")?;
    let capture_names: Vec<String> = query
        .capture_names()
        .iter()
        .map(|s| s.to_string())
        .collect();

    let mut parser = Parser::new();
    parser
        .set_language(&language)
        .context("loading Go grammar")?;
    // A parse that does not complete yields no references, which abstains for
    // the whole file. Fail-closed, and the commit-gated path still covers it.
    let Some(tree) = parser.parse(source, None) else {
        return Ok(LiveRefs::default());
    };

    let mut out = LiveRefs::default();
    let mut cursor = QueryCursor::new();
    let mut iter = cursor.matches(&query, tree.root_node(), source);
    while let Some(m) = iter.next() {
        for cap in m.captures {
            let Some(cap_name) = capture_names.get(cap.index as usize).map(String::as_str) else {
                continue;
            };
            let Ok(name) = cap.node.utf8_text(source) else {
                continue;
            };
            let r = LiveRef {
                line: cap.node.start_position().row as u32 + 1,
                name: name.to_string(),
            };
            match cap_name {
                "call.name" => out.calls.push(r),
                "sel.name" if is_call_callee(cap.node) => out.calls.push(r),
                "sel.name" => out.fields.push(r),
                _ => {}
            }
        }
    }
    Ok(out)
}

/// True when `field` is the field of a selector that is itself the callee of a
/// call — `x.Foo(…)` rather than `x.Foo`.
fn is_call_callee(field: tree_sitter::Node<'_>) -> bool {
    let Some(selector) = field.parent() else {
        return false;
    };
    let Some(parent) = selector.parent() else {
        return false;
    };
    parent.kind() == "call_expression" && parent.child_by_field_name("function") == Some(selector)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(refs: &[LiveRef]) -> Vec<(u32, &str)> {
        refs.iter().map(|r| (r.line, r.name.as_str())).collect()
    }

    #[test]
    fn go_separates_calls_from_field_reads() {
        let src = br#"
package main

func run(s *Session) int {
	s.Start()
	helper()
	n := s.count
	return n
}
"#;
        let refs = detect_live_refs(Language::Go, src).unwrap();
        assert_eq!(names(&refs.calls), vec![(5, "Start"), (6, "helper")]);
        assert_eq!(names(&refs.fields), vec![(7, "count")]);
    }

    #[test]
    fn go_detects_package_qualified_calls() {
        // `pkg.Foo()` is the shape that carries most of Go's cross-file value:
        // the receiver is a package, not a value, and only the server knows
        // which package the alias binds to.
        let src = br#"
package main

import svc "example.com/m/service"

func run() {
	svc.Start()
}
"#;
        let refs = detect_live_refs(Language::Go, src).unwrap();
        assert_eq!(names(&refs.calls), vec![(7, "Start")]);
        assert!(refs.fields.is_empty());
    }

    #[test]
    fn go_emits_no_inheritance_clauses() {
        // Interface satisfaction is implicit in Go and embedding is composition,
        // so there is no clause to detect and nothing may be inferred.
        let src = br#"
package main

type Reader interface { Read() }

type Base struct{ n int }

type Derived struct {
	Base
}
"#;
        let refs = detect_live_refs(Language::Go, src).unwrap();
        assert!(refs.inheritance.is_empty());
    }

    #[test]
    fn a_method_declaration_is_not_a_reference() {
        let src = br#"
package main

type T struct{}

func (t *T) Foo() {}
"#;
        let refs = detect_live_refs(Language::Go, src).unwrap();
        assert!(
            refs.is_empty(),
            "declarations are definitions, not references"
        );
    }

    #[test]
    fn a_language_without_a_detector_returns_empty() {
        let refs = detect_live_refs(Language::Java, b"class A { void f() { g(); } }").unwrap();
        assert!(refs.is_empty());
    }

    #[test]
    fn an_oversized_buffer_is_skipped() {
        let src = vec![b' '; MAX_SOURCE_BYTES + 1];
        assert!(detect_live_refs(Language::Go, &src).unwrap().is_empty());
    }
}
