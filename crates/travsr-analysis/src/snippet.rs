//! Kind-aware, docblock-stripped snippet extraction.
//!
//! Pure file I/O — no Tree-sitter required. Reads source lines for a node
//! whose position is already stored in the graph (travsr-store).

use std::path::Path;

use travsr_core::Node;

pub const SNIPPET_SEP: &str = "───";
pub const SNIPPET_DEFAULT_BUDGET: usize = 2000;

/// Per-kind line ceiling for snippet extraction.
/// Classes can span thousands of lines; only the header + fields are useful.
/// Interfaces/traits/enums are almost always short — take more.
/// Functions and methods default to 40 lines.
pub fn snippet_line_cap(kind: &str) -> usize {
    match kind {
        "class" | "struct" | "impl" => 15,
        "interface" | "trait" | "enum" | "type" | "type_alias" => 60,
        _ => 40,
    }
}

/// Returns true if `s` is a pure comment/blank line in any language Travsr indexes.
/// Used to detect and skip leading docblocks so the snippet starts at real code.
pub fn is_comment_line(s: &str) -> bool {
    let t = s.trim();
    t.is_empty()
        || t.starts_with("//")
        || t.starts_with('*')
        || t.starts_with("/*")
        || t.starts_with("*/")
        || (t.starts_with('#') && !t.starts_with("#[") && !t.starts_with("#![")) // Python/Ruby/shell — not Rust attributes
        || t.starts_with("\"\"\"")
        || t.starts_with("'''")
        || t.starts_with("--")   // SQL / Haskell / Lua
        || t.starts_with("rem ") // Batch
        || t.starts_with("REM ") // Batch (upper-case)
}

/// Keep line 0 (the signature/declaration) always, then skip any immediately
/// following comment/docstring run, then return the rest of the body.
/// This strips leading docblocks regardless of language.
pub fn skip_leading_comments<'a>(lines: &'a [&'a str]) -> Vec<&'a str> {
    if lines.is_empty() {
        return vec![];
    }
    let mut result = vec![lines[0]]; // signature line is always kept
    let mut i = 1;
    while i < lines.len() && is_comment_line(lines[i]) {
        i += 1;
    }
    result.extend_from_slice(&lines[i..]);
    result
}

/// Read a kind-aware, docblock-stripped snippet for `node` from disk.
///
/// Returns `None` when:
/// - `node.line` is absent (file-kind nodes, synthetic import nodes)
/// - the source file cannot be read (stale index, file deleted since last init)
/// - `vname.path` would escape `repo_root` (SEC path-traversal guard)
///
/// Platform note: `vname.path` always uses forward slashes (POSIX-style) as
/// stored by Tree-sitter/LSIF.  On Windows `Path::join` accepts both `/` and
/// `\`, so no pre-normalisation is required.
pub fn snippet_for_node(node: &Node, repo_root: &Path) -> Option<String> {
    let start_1based = node.line? as usize; // 1-based, None → bail
    let end_1based = node.end_line.unwrap_or(node.line.unwrap()) as usize;

    if node.vname.path.is_empty() {
        return None;
    }

    // SEC: reject any vname.path that attempts to escape the repo root.
    // Path::join on an absolute component replaces the prefix entirely on all
    // platforms. Detect explicit traversal patterns and absolute-path prefixes
    // before constructing the joined path.
    // vname.path always uses '/' (POSIX) as stored by Tree-sitter/LSIF, but a
    // crafted DB entry could hold Windows-style absolute paths — cover both.
    let p = &node.vname.path;
    let looks_absolute = p.starts_with('/')         // Unix absolute
        || p.starts_with('\\')                      // Windows UNC prefix
        || p.get(1..3).map(|s| s == ":\\" || s == ":/").unwrap_or(false); // C:\ or C:/
    if looks_absolute || p.contains("..") {
        tracing::debug!(
            path = %node.vname.path,
            "snippet_for_node: skipping node with absolute or traversal path"
        );
        return None;
    }
    let abs = repo_root.join(&node.vname.path);
    // Canonicalize both paths to resolve symlinks before comparing.
    // Fall back to lexical comparison when canonicalize fails (e.g. file
    // missing) — the starts_with guard below still catches traversal attempts.
    let canon_abs = abs.canonicalize().unwrap_or_else(|_| abs.clone());
    let canon_root = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf());
    if !canon_abs.starts_with(&canon_root) {
        tracing::warn!(
            path = %node.vname.path,
            "snippet_for_node: path escapes repo_root — skipping"
        );
        return None;
    }

    let content = std::fs::read_to_string(&canon_abs).ok()?;
    let all_lines: Vec<&str> = content.lines().collect();

    let from = start_1based.saturating_sub(1); // convert to 0-based
    if from >= all_lines.len() {
        return None;
    }

    let cap = snippet_line_cap(&node.kind);
    // end_line is inclusive and 1-based; clamp to cap and file length.
    let to = (end_1based.min(from + cap)).min(all_lines.len());
    // Guard against a corrupt DB entry where end_line < line — the slice
    // would panic with "range start > end". Degrade gracefully instead.
    if to < from {
        tracing::debug!(
            path = %node.vname.path,
            from,
            to,
            "snippet_for_node: end_line < line in DB — skipping node"
        );
        return None;
    }
    let window: Vec<&str> = all_lines[from..to].to_vec();
    let trimmed = skip_leading_comments(&window);
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use travsr_core::VName;

    fn make_fn_node(path: &str, sig: &str, line: u32, end_line: u32) -> Node {
        Node::new(
            VName::new("corpus", "", path, "typescript", sig),
            "function",
        )
        .with_line(line)
        .with_end_line(end_line)
    }

    fn make_class_node(path: &str, sig: &str, line: u32, end_line: u32) -> Node {
        Node::new(VName::new("corpus", "", path, "typescript", sig), "class")
            .with_line(line)
            .with_end_line(end_line)
    }

    // ── is_comment_line ───────────────────────────────────────────────────────

    #[test]
    fn is_comment_line_detects_all_styles() {
        assert!(is_comment_line("// C-style"));
        assert!(is_comment_line("  * javadoc middle"));
        assert!(is_comment_line("/* block start */"));
        assert!(is_comment_line("*/"));
        assert!(is_comment_line("# Python"));
        assert!(is_comment_line("\"\"\""));
        assert!(is_comment_line("'''"));
        assert!(is_comment_line("-- SQL"));
        assert!(is_comment_line(""));
        assert!(is_comment_line("   "));
        // Must NOT flag real code as a comment
        assert!(!is_comment_line("fn foo() {}"));
        assert!(!is_comment_line("const x = 1;"));
        assert!(!is_comment_line("public class Foo {"));
        // Rust attributes must NOT be treated as comments
        assert!(!is_comment_line("#[inline]"));
        assert!(!is_comment_line("#[derive(Debug, Clone)]"));
        assert!(!is_comment_line("#![allow(dead_code)]"));
    }

    // ── skip_leading_comments ─────────────────────────────────────────────────

    #[test]
    fn skip_leading_comments_keeps_signature_discards_docblock() {
        let lines = vec![
            "fn charge(amount: f64) -> Result<()> {",
            "    // Calculate fee",
            "    // and apply",
            "    let fee = amount * 0.02;",
            "    Ok(())",
            "}",
        ];
        let out = skip_leading_comments(&lines);
        assert_eq!(out[0], "fn charge(amount: f64) -> Result<()> {");
        assert_eq!(out[1], "    let fee = amount * 0.02;");
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn skip_leading_comments_no_docblock_passes_through() {
        let lines = vec!["fn foo() {", "    bar();", "}"];
        let out = skip_leading_comments(&lines);
        assert_eq!(out, lines);
    }

    #[test]
    fn skip_leading_comments_empty_input() {
        let out = skip_leading_comments(&[]);
        assert!(out.is_empty());
    }

    // ── snippet_line_cap ──────────────────────────────────────────────────────

    #[test]
    fn snippet_line_cap_class_is_15() {
        assert_eq!(snippet_line_cap("class"), 15);
        assert_eq!(snippet_line_cap("struct"), 15);
        assert_eq!(snippet_line_cap("impl"), 15);
    }

    #[test]
    fn snippet_line_cap_interface_is_60() {
        assert_eq!(snippet_line_cap("interface"), 60);
        assert_eq!(snippet_line_cap("trait"), 60);
        assert_eq!(snippet_line_cap("enum"), 60);
    }

    #[test]
    fn snippet_line_cap_function_is_40() {
        assert_eq!(snippet_line_cap("function"), 40);
        assert_eq!(snippet_line_cap("method"), 40);
        assert_eq!(snippet_line_cap(""), 40);
    }

    // ── snippet_for_node ──────────────────────────────────────────────────────

    #[test]
    fn snippet_for_node_reads_correct_lines() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src").join("a.ts");
        std::fs::create_dir_all(src.parent().unwrap()).unwrap();
        std::fs::write(&src, "// file header\nfunction foo() {\n  return 1;\n}\n").unwrap();

        let node = make_fn_node("src/a.ts", "fn:foo", 2, 4);
        let snippet = snippet_for_node(&node, dir.path()).unwrap();
        assert!(snippet.contains("function foo()"));
        assert!(snippet.contains("return 1;"));
        assert!(!snippet.contains("file header"));
    }

    #[test]
    fn snippet_for_node_strips_leading_docblock() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("charge.ts");
        std::fs::write(
            &src,
            "function charge(amount) {\n  // apply fee\n  // docblock\n  return amount * 1.02;\n}\n",
        )
        .unwrap();

        let node = make_fn_node("charge.ts", "fn:charge", 1, 5);
        let snippet = snippet_for_node(&node, dir.path()).unwrap();
        assert!(snippet.starts_with("function charge(amount)"));
        assert!(!snippet.contains("apply fee"));
        assert!(snippet.contains("return amount * 1.02"));
    }

    #[test]
    fn snippet_for_node_missing_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let node = make_fn_node("nonexistent.ts", "fn:ghost", 1, 5);
        assert!(snippet_for_node(&node, dir.path()).is_none());
    }

    #[test]
    fn snippet_for_node_path_traversal_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let mut node = make_fn_node("../etc/passwd", "fn:evil", 1, 5);
        node.vname.path = "../etc/passwd".to_string();
        assert!(
            snippet_for_node(&node, dir.path()).is_none(),
            "path traversal must be rejected"
        );
    }

    #[test]
    fn snippet_for_node_absolute_path_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let mut node = make_fn_node("/etc/passwd", "fn:evil", 1, 1);
        node.vname.path = "/etc/passwd".to_string();
        assert!(
            snippet_for_node(&node, dir.path()).is_none(),
            "Unix absolute path must be rejected"
        );
    }

    #[test]
    fn snippet_for_node_windows_absolute_path_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        for evil in ["C:\\Windows\\System32\\evil.txt", "C:/Windows/evil.txt"] {
            let mut node = make_fn_node(evil, "fn:evil", 1, 1);
            node.vname.path = evil.to_string();
            assert!(
                snippet_for_node(&node, dir.path()).is_none(),
                "Windows absolute path '{evil}' must be rejected"
            );
        }
    }

    #[test]
    fn snippet_for_node_class_capped_at_15_lines() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("big.ts");
        let body: String = std::iter::once("class BigClass {\n".to_string())
            .chain((0..49).map(|i| format!("  method{i}() {{}}\n")))
            .collect();
        std::fs::write(&src, &body).unwrap();

        let node = make_class_node("big.ts", "class:BigClass", 1, 50);
        let snippet = snippet_for_node(&node, dir.path()).unwrap();
        let line_count = snippet.lines().count();
        assert!(
            line_count <= 15,
            "class snippet must be capped at 15 lines, got {line_count}"
        );
    }
}
