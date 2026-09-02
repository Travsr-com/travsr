//! RFC-027 section 7.3 step 1 — the local-scope check for the lexical floor.
//!
//! Step 3a of the resolution flow may emit only when a reference is
//! *unambiguous* **and** *local-clean*: "is R a free/unbound reference (not a
//! local var, param, or shadowed name)?". Uniqueness alone is not enough, and
//! the gap is not theoretical:
//!
//! ```text
//! // src/order.ts
//! export function process() {
//!   const save = () => 1;
//!   return save();          // <- a local closure, nothing to do with...
//! }
//! // src/unrelated.ts
//! export function save() { return 2; }   // ...this
//! ```
//!
//! The call-site extractor sees `save()` as a bare identifier call and emits
//! `fn:save` with no receiver, and Phase A does not index the local `const`, so
//! `fn:save` has exactly *one* definition repo-wide. Without this check the
//! floor resolves the local call to the unrelated function and writes a
//! `provenance='live'` edge — a fabricated structural relationship, surfaced by
//! `get_callers` / `find_references` / `get_blast_radius` until the next commit
//! sweeps it. That is the section 8.1 failure the RFC calls a breach of the
//! value proposition, it is reachable from ordinary code, and it gets *more*
//! likely as a repo grows, because uniqueness is what triggers it.
//!
//! ## What counts as a hazardous binder
//!
//! Only binders **Phase A does not index as nodes**. If Phase A indexes the
//! binding, the floor is already safe by construction: the local definition is
//! itself a candidate, so either it is the only one repo-wide (and resolving to
//! it is correct) or there are two and `unique_definition` abstains. Measured
//! against the Phase A walkers, the indexed set covers nested `function` /
//! `def` / `fn` declarations, nested classes and structs, and TypeScript's
//! module-level `const f = () => …`. What is left — and what this module
//! collects — is:
//!
//! - variable bindings (`const`/`let`/`var`, Python assignment, Rust `let`)
//! - function, method and closure parameters
//!
//! Import bindings are deliberately **not** collected. `import { save } from
//! "./unrelated"` is exactly the cross-file reference the live lane exists to
//! resolve, and treating it as local would disable the lane for its own purpose.
//!
//! ## Why a whole-file set, not a scope tree
//!
//! The set is per file and is not scoped to the enclosing function: a name bound
//! anywhere in the file disables the floor for that name *in that file*. Real
//! scope resolution is what a language server does, and asking for one here
//! would defeat the point of a lane that runs with no server attached. The
//! over-approximation errs toward abstention, which costs recall and never
//! correctness (section 16.2), and it costs almost none of the recall that
//! matters: the set is per file, so a cross-file call to `formatDate()` is
//! unaffected unless the *calling* file also binds `formatDate` locally.

use std::collections::HashSet;

use anyhow::Context as _;
use streaming_iterator::StreamingIterator as _;
use tree_sitter::{Parser, Query, QueryCursor};

use travsr_core::Language;

/// Upper bound on a buffer this pass will parse, matching the limit and the
/// reasoning in `live_detect`: a generated file this large is not what the live
/// lane exists for, and parsing it on every save would cost more than the
/// freshness is worth.
const MAX_SOURCE_BYTES: usize = 10 * 1024 * 1024;

/// Names bound by a local variable or a parameter anywhere in `source`.
///
/// A bare-identifier reference whose leaf is in this set is **not** provably
/// free, so the lexical floor must abstain on it rather than resolve it
/// repo-wide.
///
/// Returns an empty set for a language with no native lexical floor (the
/// editor-only lane sends every reference to a language server, which resolves
/// scope correctly on its own) and on any parse failure — but note that an empty
/// set means "no binder found", so the caller's gate opens. That is the right
/// failure direction only because the two other gates (exactly one repo-wide
/// definition, and the soundness gate in `edge_if_sound`) still apply; a parse
/// that fails here also fails in the extractor, which then produces no
/// references to gate at all.
pub fn local_binding_names(lang: Language, source: &[u8]) -> HashSet<String> {
    if source.len() > MAX_SOURCE_BYTES {
        return HashSet::new();
    }
    let Some((grammar, query_src)) = binder_spec(lang) else {
        return HashSet::new();
    };
    collect(grammar, query_src, source).unwrap_or_else(|e| {
        tracing::debug!(language = lang.as_str(), error = %e, "live: binder scan failed");
        HashSet::new()
    })
}

/// The grammar and binder query for a language with a native lexical floor.
///
/// Only the three native Phase B languages appear. The other thirteen run the
/// editor lane alone (section 8.3), where the language server does the scope
/// resolution, so there is no lexical floor to gate.
fn binder_spec(lang: Language) -> Option<(tree_sitter::Language, &'static str)> {
    match lang {
        Language::TypeScript => Some((
            tree_sitter::Language::new(tree_sitter_typescript::LANGUAGE_TYPESCRIPT),
            TYPESCRIPT_BINDERS,
        )),
        Language::Python => Some((
            tree_sitter::Language::new(tree_sitter_python::LANGUAGE),
            PYTHON_BINDERS,
        )),
        Language::Rust => Some((
            tree_sitter::Language::new(tree_sitter_rust::LANGUAGE),
            RUST_BINDERS,
        )),
        _ => None,
    }
}

/// `const`/`let`/`var` declarators and parameters. Nested `function` and `class`
/// declarations are omitted on purpose: Phase A indexes both, so they are
/// already covered by the uniqueness gate.
const TYPESCRIPT_BINDERS: &str = "
(variable_declarator name: (identifier) @bind)
(required_parameter pattern: (identifier) @bind)
(optional_parameter pattern: (identifier) @bind)
";

/// Assignment targets (including module level, which Phase A does not index for
/// Python the way it does TypeScript's `const f = () => …`), parameters in every
/// shape the grammar gives them, and `for` targets.
const PYTHON_BINDERS: &str = "
(assignment left: (identifier) @bind)
(parameters (identifier) @bind)
(lambda_parameters (identifier) @bind)
(default_parameter name: (identifier) @bind)
(typed_parameter (identifier) @bind)
(typed_default_parameter name: (identifier) @bind)
(for_statement left: (identifier) @bind)
";

/// `let` bindings, function parameters and closure parameters. Nested `fn`
/// items and `struct`s are omitted for the same reason as TypeScript's: Phase A
/// indexes them.
const RUST_BINDERS: &str = "
(let_declaration pattern: (identifier) @bind)
(parameter pattern: (identifier) @bind)
(closure_parameters (identifier) @bind)
";

fn collect(
    grammar: tree_sitter::Language,
    query_src: &str,
    source: &[u8],
) -> anyhow::Result<HashSet<String>> {
    let query = Query::new(&grammar, query_src).context("compiling binder query")?;
    let mut parser = Parser::new();
    parser.set_language(&grammar).context("loading grammar")?;
    let Some(tree) = parser.parse(source, None) else {
        return Ok(HashSet::new());
    };

    let mut out = HashSet::new();
    let mut cursor = QueryCursor::new();
    let mut iter = cursor.matches(&query, tree.root_node(), source);
    while let Some(m) = iter.next() {
        for cap in m.captures {
            if let Ok(name) = cap.node.utf8_text(source) {
                out.insert(name.to_string());
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(lang: Language, src: &str) -> HashSet<String> {
        local_binding_names(lang, src.as_bytes())
    }

    /// The exact shape of the reported defect: a local closure whose name is
    /// unique elsewhere in the repo.
    #[test]
    fn typescript_local_closure_is_a_binder() {
        let found = names(
            Language::TypeScript,
            "export function process() { const save = () => 1; return save(); }",
        );
        assert!(found.contains("save"), "got {found:?}");
    }

    #[test]
    fn typescript_parameters_are_binders() {
        let found = names(
            Language::TypeScript,
            "function run(cb: any, opt?: any) { cb(); }",
        );
        assert!(found.contains("cb"), "got {found:?}");
        assert!(found.contains("opt"), "got {found:?}");
    }

    /// An imported name is the cross-file reference the lane exists to resolve,
    /// so it must not be collected.
    #[test]
    fn typescript_imports_are_not_binders() {
        let found = names(
            Language::TypeScript,
            "import { save } from './unrelated';\nexport function p() { return save(); }",
        );
        assert!(!found.contains("save"), "got {found:?}");
    }

    #[test]
    fn python_assignments_and_parameters_are_binders() {
        let found = names(
            Language::Python,
            "mod = lambda: 1\ndef outer(param, opt=None):\n    local = lambda: 2\n    return local() + param() + mod()\n",
        );
        for n in ["mod", "param", "opt", "local"] {
            assert!(found.contains(n), "{n} missing from {found:?}");
        }
    }

    #[test]
    fn python_imports_are_not_binders() {
        let found = names(
            Language::Python,
            "from b import save\ndef p():\n    return save()\n",
        );
        assert!(!found.contains("save"), "got {found:?}");
    }

    #[test]
    fn rust_let_and_parameters_are_binders() {
        let found = names(
            Language::Rust,
            "fn outer(param: fn() -> i32) -> i32 { let local = || 2; local() + param() }",
        );
        assert!(found.contains("local"), "got {found:?}");
        assert!(found.contains("param"), "got {found:?}");
    }

    /// `use` is Rust's import form and must stay out of the set.
    #[test]
    fn rust_use_is_not_a_binder() {
        let found = names(
            Language::Rust,
            "use crate::store::save;\nfn p() -> i32 { save() }",
        );
        assert!(!found.contains("save"), "got {found:?}");
    }

    /// An editor-only language has no lexical floor to gate.
    #[test]
    fn a_language_without_a_native_floor_yields_nothing() {
        assert!(names(Language::Go, "func main() { save() }").is_empty());
    }
}
