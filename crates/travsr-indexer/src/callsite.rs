//! Language-agnostic call-site detection for reference occurrences (#650).
//!
//! ## Why this exists
//! General-purpose semantic indexers (rust-analyzer LSIF, the scip-* family)
//! report a reference occurrence for **every** resolved use of a symbol —
//! `self`/`Self`, type annotations, path segments, bare identifier reads, and
//! genuine calls alike. Our function-attributed ingestion paths
//! ([`crate::lsif::ingest_rust_positional`], [`crate::lsif::ingest_scip_g2`])
//! turned each such occurrence into a `ref/call` edge. Non-call occurrences
//! whose callee definition resolves to the *enclosing* function collapsed into
//! `src == dst` self-loops (#650, 17.7% of Rust `ref/call` edges); off the
//! diagonal they are spurious non-call edges that pollute `get_callers` /
//! `find_references` / blast-radius.
//!
//! The write-side guard (`write_scip_attributed_batch`) removes the self-loop
//! *symptom*. This module removes the *cause* at the source: keep only
//! occurrences that are actually the callee of a call expression.
//!
//! ## The predicate
//! We have no per-language call-site query for every indexed language, so the
//! test is lexical and language-agnostic: an occurrence is a call iff the
//! identifier at that position is immediately followed by a call `(` — allowing
//! a macro `!`, a turbofish `::<...>`, and surrounding whitespace. That covers
//! `foo()`, `obj.method()`, `Type::assoc()`, `mod::fn()`, and `mac!()` across
//! the C-family, Rust, Python, Go, Java, and friends, while dropping
//! `self`/type/path references.
//!
//! ## Paren-optional languages
//! The `(`-rule is only sound for languages whose call syntax **requires**
//! parentheses. Ruby and Scala allow paren-less calls (`obj.method` *is* a
//! call), so filtering them by `(` would drop real calls. [`uses_call_parens`]
//! gates the SCIP path to the paren-required allowlist; every other language is
//! left untouched (recall preserved — the self-loop write guard still protects
//! it from the diagonal).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Returns the byte offset in `line` for a 0-based UTF-16 `column`, clamped to
/// the line length. LSIF and SCIP positions are UTF-16 code units; source is
/// UTF-8. For ASCII (the overwhelming majority of identifier positions) this is
/// the identity map. The returned offset always lands on a `char` boundary.
fn utf16_col_to_byte(line: &str, column: u32) -> usize {
    let mut units = 0u32;
    for (byte_idx, ch) in line.char_indices() {
        if units >= column {
            return byte_idx;
        }
        units += ch.len_utf16() as u32;
    }
    line.len()
}

/// True iff `after` — the source immediately following an identifier token —
/// opens a call: optional whitespace, an optional macro `!`, an optional
/// turbofish `::<...>`, then a call delimiter.
///
/// Without a macro bang the delimiter must be `(`. After a macro bang we also
/// accept `[` and `{` (`vec![..]`, `hashmap!{..}`) since those are invocations
/// too. A bare `::ident` (path segment, e.g. the `Type` occurrence in
/// `Type::method()`) is intentionally *not* a call — only the trailing `method`
/// occurrence is.
fn opens_call(after: &str) -> bool {
    let mut s = after.trim_start();

    // Optional macro bang. Guard against `!=` and friends: we consume the `!`
    // and only report a call if a delimiter actually follows.
    let macro_bang = if let Some(rest) = s.strip_prefix('!') {
        s = rest.trim_start();
        true
    } else {
        false
    };

    // Optional turbofish `::<...>` on the same identifier: skip the balanced
    // angle-bracket group. `Vec::<u8>::new()` — the `new` occurrence sees
    // `()`; a `Vec` occurrence sees `::<u8>::new(` → after the turbofish the
    // next char is `:`, not `(`, so it is correctly rejected.
    if let Some(rest) = s.strip_prefix("::<") {
        let mut depth = 1usize;
        let mut end = None;
        for (i, ch) in rest.char_indices() {
            match ch {
                '<' => depth += 1,
                '>' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(i + ch.len_utf8());
                        break;
                    }
                }
                _ => {}
            }
        }
        match end {
            Some(e) => s = rest[e..].trim_start(),
            None => return false, // unbalanced — treat as non-call
        }
    }

    match s.chars().next() {
        Some('(') => true,
        Some('[') | Some('{') => macro_bang,
        _ => false,
    }
}

/// Language-agnostic call-site predicate for a reference occurrence.
///
/// `line` is the full source line text; `start_col` is the occurrence's 0-based
/// UTF-16 start column (as reported by LSIF / SCIP). Returns true iff the
/// identifier at that position is the callee of a call expression. See the
/// module docs for the exact shape recognised.
pub fn occurrence_is_call(line: &str, start_col: u32) -> bool {
    let start = utf16_col_to_byte(line, start_col);
    let rest = &line[start..];

    // Consume the identifier token at the occurrence position.
    let mut idx_after_ident = 0usize;
    let mut saw_ident = false;
    for (off, ch) in rest.char_indices() {
        if ch.is_alphanumeric() || ch == '_' {
            saw_ident = true;
            idx_after_ident = off + ch.len_utf8();
        } else {
            break;
        }
    }
    if !saw_ident {
        return false;
    }
    opens_call(&rest[idx_after_ident..])
}

/// Whether `language` uses parentheses for **all** call expressions, making the
/// [`occurrence_is_call`] `(`-rule sound. Conservative: languages with
/// significant paren-less call syntax (Ruby, Scala) and anything unknown return
/// `false` so their occurrences are left unfiltered rather than risk dropping a
/// real call.
pub fn uses_call_parens(language: &str) -> bool {
    matches!(
        language.to_ascii_lowercase().as_str(),
        "rust"
            | "python"
            | "go"
            | "golang"
            | "javascript"
            | "typescript"
            | "tsx"
            | "jsx"
            | "java"
            | "kotlin"
            | "c"
            | "cpp"
            | "c++"
            | "cxx"
            | "csharp"
            | "c#"
            | "cs"
            | "objc"
            | "objective-c"
            | "swift"
            | "php"
            | "dart"
    )
}

/// Lazy, cached reader of source lines for the call-site filter.
///
/// Files are read once and split into lines on first access; misses (unreadable
/// file, out-of-range line) are remembered so a bad path is not re-`stat`ed for
/// every occurrence in it. Keyed by absolute path.
#[derive(Default)]
pub struct SourceLines {
    files: HashMap<PathBuf, Option<Vec<String>>>,
}

impl SourceLines {
    pub fn new() -> Self {
        Self::default()
    }

    /// The `line_1based`-th line of `abs_path`, or `None` when the file cannot
    /// be read or the line is out of range. Callers treat `None` as
    /// fail-open (keep the occurrence): in production the file always exists
    /// because it was just indexed, and the write-side self-loop guard is the
    /// backstop for the diagonal.
    pub fn line(&mut self, abs_path: &Path, line_1based: u32) -> Option<&str> {
        let entry = self.files.entry(abs_path.to_path_buf()).or_insert_with(|| {
            std::fs::read_to_string(abs_path)
                .ok()
                .map(|s| s.lines().map(str::to_string).collect())
        });
        let lines = entry.as_ref()?;
        let idx = (line_1based as usize).checked_sub(1)?;
        lines.get(idx).map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_call_is_a_call() {
        assert!(occurrence_is_call("    foo()", 4));
        assert!(occurrence_is_call("let x = compute(a, b);", 8));
    }

    #[test]
    fn method_and_scoped_calls_are_calls() {
        // occurrence on `method`
        assert!(occurrence_is_call("self.method()", 5));
        // occurrence on `assoc`
        assert!(occurrence_is_call("Type::assoc(x)", 6));
        // path call: occurrence on the trailing `swap`
        assert!(occurrence_is_call("std::mem::swap(&mut a, &mut b)", 10));
    }

    #[test]
    fn whitespace_before_paren_is_a_call() {
        assert!(occurrence_is_call("foo ()", 0));
    }

    #[test]
    fn turbofish_call_is_a_call() {
        assert!(occurrence_is_call("collect::<Vec<u8>>()", 0));
        // occurrence on `parse` in `str::parse::<i32>()`
        assert!(occurrence_is_call("s.parse::<i32>()", 2));
    }

    #[test]
    fn macro_invocations_are_calls() {
        assert!(occurrence_is_call("println!(\"{}\", x)", 0));
        assert!(occurrence_is_call("vec![1, 2, 3]", 0));
        assert!(occurrence_is_call("hashmap!{ a => b }", 0));
    }

    #[test]
    fn self_reference_is_not_a_call() {
        // The #650 reproducer: `match self {` — occurrence on `self`.
        assert!(!occurrence_is_call("        match self {", 14));
    }

    #[test]
    fn type_and_path_references_are_not_calls() {
        // type annotation
        assert!(!occurrence_is_call("let x: MyType = y;", 7));
        // the `Type` occurrence in `Type::method()` is a path segment, not a call
        assert!(!occurrence_is_call("Type::method()", 0));
        // field access, not a call
        assert!(!occurrence_is_call("obj.field", 4));
        // bare identifier read
        assert!(!occurrence_is_call("return value;", 7));
    }

    #[test]
    fn not_equals_is_not_a_call() {
        assert!(!occurrence_is_call("a != b", 0));
    }

    #[test]
    fn function_value_without_call_is_not_a_call() {
        // passing a function by value, not invoking it
        assert!(!occurrence_is_call("register(handler);", 9));
    }

    #[test]
    fn utf16_column_maps_across_multibyte_prefix() {
        // `café.` is 5 UTF-16 units (é is one unit); the call `len()` starts at
        // UTF-16 column 5.
        let line = "café.len()";
        assert!(occurrence_is_call(line, 5));
    }

    #[test]
    fn paren_language_allowlist() {
        assert!(uses_call_parens("rust"));
        assert!(uses_call_parens("Rust"));
        assert!(uses_call_parens("go"));
        assert!(uses_call_parens("java"));
        // paren-optional / unknown → not filtered
        assert!(!uses_call_parens("ruby"));
        assert!(!uses_call_parens("scala"));
        assert!(!uses_call_parens("elixir"));
        assert!(!uses_call_parens(""));
    }

    #[test]
    fn out_of_range_position_is_not_a_call() {
        assert!(!occurrence_is_call("foo()", 99));
    }
}
