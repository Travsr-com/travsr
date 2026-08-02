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
        // A doc-chunk is already bounded at chunk-build time (§3.3's 2600/3900
        // char split thresholds) — 40 lines (the code default) would truncate
        // a normal-length prose section well before that boundary. 200 lines
        // comfortably covers a full chunk at typical prose line lengths while
        // still bounding the pathological case (one line per word, say).
        "doc-chunk" => 200,
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
    snippet_for_node_capped(node, repo_root, snippet_line_cap(&node.kind))
}

/// Read the *entire* definition body for `node` (`line..=end_line`) with no
/// per-kind line cap. Used by `get_snippets(mode="full")` when a caller pins a
/// node into the AI context and wants the complete source regardless of size.
///
/// Same SEC path guard and leading-docblock stripping as [`snippet_for_node`];
/// only the line ceiling differs.
pub fn snippet_for_node_full(node: &Node, repo_root: &Path) -> Option<String> {
    snippet_for_node_capped(node, repo_root, usize::MAX)
}

/// Shared implementation for [`snippet_for_node`] (kind-capped) and
/// [`snippet_for_node_full`] (uncapped). `cap` bounds how many lines past the
/// declaration are read; `usize::MAX` reads the whole `line..=end_line` range.
pub fn snippet_for_node_capped(node: &Node, repo_root: &Path, cap: usize) -> Option<String> {
    let start_1based = node.line? as usize; // 1-based, None → bail
    let end_1based = node.end_line.unwrap_or(node.line.unwrap()) as usize;

    let canon_abs = resolve_source_path(node, repo_root)?;
    let content = std::fs::read_to_string(&canon_abs).ok()?;
    let all_lines: Vec<&str> = content.lines().collect();

    let from = start_1based.saturating_sub(1); // convert to 0-based
    if from >= all_lines.len() {
        return None;
    }

    // end_line is inclusive and 1-based; clamp to cap and file length.
    // `cap == usize::MAX` (full mode) means "no line ceiling" — `from + cap`
    // saturates so only end_line / file length bound the window.
    let to = (end_1based.min(from.saturating_add(cap))).min(all_lines.len());
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
    // `skip_leading_comments` exists to strip a leading docblock above a code
    // declaration; its `is_comment_line` heuristic treats any bare `#` line as
    // a comment. In prose that heuristic is simply wrong — a markdown heading
    // (or a run of nested headings with no intervening paragraph) starts with
    // `#` and is the content itself, not noise to skip past (#376). A
    // doc-chunk's window is never a declaration-plus-docblock in the first
    // place, so it always gets the full raw window unstripped.
    let trimmed = if node.kind == "doc-chunk" {
        window
    } else {
        skip_leading_comments(&window)
    };
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.join("\n"))
}

/// Resolve `node.vname.path` to a canonicalized absolute path under `repo_root`,
/// applying the SEC path-traversal / containment guards shared by all source
/// readers here. Returns `None` when the path is empty, looks absolute, contains
/// `..`, or (after canonicalization) escapes `repo_root`.
///
/// vname.path always uses '/' (POSIX) as stored by Tree-sitter/LSIF, but a
/// crafted DB entry could hold Windows-style absolute paths — both are covered.
pub fn resolve_source_path(node: &Node, repo_root: &Path) -> Option<std::path::PathBuf> {
    if node.vname.path.is_empty() {
        return None;
    }
    let p = &node.vname.path;
    let looks_absolute = p.starts_with('/')         // Unix absolute
        || p.starts_with('\\')                      // Windows UNC prefix
        || p.get(1..3).map(|s| s == ":\\" || s == ":/").unwrap_or(false); // C:\ or C:/
    if looks_absolute || p.contains("..") {
        tracing::debug!(
            path = %node.vname.path,
            "resolve_source_path: skipping node with absolute or traversal path"
        );
        return None;
    }
    let abs = repo_root.join(&node.vname.path);
    // Canonicalize both paths to resolve symlinks before comparing. Fall back to
    // lexical comparison when canonicalize fails (e.g. file missing) — the
    // starts_with guard below still catches traversal attempts.
    let canon_abs = abs.canonicalize().unwrap_or_else(|_| abs.clone());
    let canon_root = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf());
    if !canon_abs.starts_with(&canon_root) {
        tracing::warn!(
            path = %node.vname.path,
            "resolve_source_path: path escapes repo_root — skipping"
        );
        return None;
    }
    Some(canon_abs)
}

/// Scan `node`'s source span for lines where `name` occurs as a standalone
/// identifier token, returning their 1-based line numbers (capped at `max`).
///
/// For a node with a known span (`line`..=`end_line`) only that range is read;
/// for a span-less node (e.g. a file-kind caller) the whole file is scanned.
/// Reuses [`resolve_source_path`]'s SEC guards. Empty when the file can't be
/// read or `name` never appears bounded by non-identifier characters.
///
/// This backs `get_callers` call-site resolution (#399): a `RefCall` graph edge
/// is deduplicated by `(src,dst,kind)`, so one edge can represent several calls
/// from the same caller — re-scanning the caller body recovers each site as
/// `path:line` without storing per-occurrence rows.
pub fn call_site_lines(node: &Node, repo_root: &Path, name: &str, max: usize) -> Vec<u32> {
    if name.is_empty() || max == 0 {
        return Vec::new();
    }
    let path = match resolve_source_path(node, repo_root) {
        Some(p) => p,
        None => return Vec::new(),
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let all_lines: Vec<&str> = content.lines().collect();

    // Scan window (0-based, end-exclusive): the node's span, or the whole file
    // when the node has no line (file-kind caller).
    let (from, to) = match node.line {
        Some(l) => {
            let from = (l as usize).saturating_sub(1);
            let end = node.end_line.map(|e| e as usize).unwrap_or(l as usize);
            (from, end.min(all_lines.len()))
        }
        None => (0, all_lines.len()),
    };
    if from >= all_lines.len() || to <= from {
        return Vec::new();
    }

    let mut hits: Vec<u32> = Vec::new();
    for (offset, line) in all_lines[from..to].iter().enumerate() {
        if line_has_identifier(line, name) {
            hits.push((from + offset + 1) as u32); // back to 1-based
            if hits.len() >= max {
                break;
            }
        }
    }
    hits
}

/// True when `name` appears in `line` bounded by non-identifier characters on
/// both sides, so `Sync` does not match inside `SyncPod`. Identifier characters
/// are ASCII alphanumerics and `_`. Byte offsets from `match_indices` are always
/// UTF-8 char boundaries, so multibyte neighbours are treated as boundaries.
fn line_has_identifier(line: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let bytes = line.as_bytes();
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    for (idx, _) in line.match_indices(name) {
        let before_ok = idx == 0 || !is_ident(bytes[idx - 1]);
        let after = idx + name.len();
        let after_ok = after >= bytes.len() || !is_ident(bytes[after]);
        if before_ok && after_ok {
            return true;
        }
    }
    false
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

    fn make_doc_chunk_node(path: &str, sig: &str, line: u32, end_line: u32) -> Node {
        Node::new(VName::new("corpus", "", path, "markdown", sig), "doc-chunk")
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

    // ── line_has_identifier ───────────────────────────────────────────────────

    #[test]
    fn line_has_identifier_respects_word_boundaries() {
        assert!(line_has_identifier("  SyncPod(ctx)", "SyncPod"));
        assert!(line_has_identifier("x = a.SyncPod();", "SyncPod"));
        assert!(line_has_identifier("SyncPod", "SyncPod"));
        // Must NOT match inside a longer identifier.
        assert!(!line_has_identifier("  SyncPodWorker(ctx)", "SyncPod"));
        assert!(!line_has_identifier("  mySyncPod(ctx)", "SyncPod"));
        assert!(!line_has_identifier("  _SyncPod2()", "SyncPod"));
    }

    // ── call_site_lines (#399) ────────────────────────────────────────────────

    #[test]
    fn call_site_lines_finds_each_occurrence_in_span() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("worker.go");
        // caller func spans lines 2..=8; calls SyncPod on lines 4, 6 and a decoy
        // SyncPodX on line 7 that must NOT match.
        std::fs::write(
            &src,
            "package main\nfunc run() {\n  setup()\n  SyncPod(a)\n  log()\n  SyncPod(b)\n  SyncPodX(c)\n}\n",
        )
        .unwrap();
        let caller = make_fn_node("worker.go", "fn:run", 2, 8);
        let sites = call_site_lines(&caller, dir.path(), "SyncPod", 50);
        assert_eq!(sites, vec![4, 6], "both call sites, decoy excluded");
    }

    #[test]
    fn call_site_lines_scans_whole_file_when_no_span() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("f.ts");
        std::fs::write(&src, "a();\nfoo();\nb();\nfoo();\n").unwrap();
        // Span-less caller (e.g. file-kind node): line = None → whole-file scan.
        let node = Node::new(
            VName::new("corpus", "", "f.ts", "typescript", "file:f"),
            "file",
        );
        let sites = call_site_lines(&node, dir.path(), "foo", 50);
        assert_eq!(sites, vec![2, 4]);
    }

    #[test]
    fn call_site_lines_respects_max_cap() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("m.ts");
        std::fs::write(&src, "foo();\nfoo();\nfoo();\nfoo();\n").unwrap();
        let node = make_fn_node("m.ts", "fn:wrap", 1, 4);
        let sites = call_site_lines(&node, dir.path(), "foo", 2);
        assert_eq!(sites.len(), 2, "capped at max");
    }

    #[test]
    fn call_site_lines_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let node = make_fn_node("does_not_exist.rs", "fn:x", 1, 3);
        assert!(call_site_lines(&node, dir.path(), "foo", 50).is_empty());
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

    // ── #376: doc-chunk snippets ─────────────────────────────────────────────

    #[test]
    fn doc_chunk_snippet_preserves_heading_not_stripped_as_comment() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("guide.md");
        std::fs::write(&src, "## Real Heading\n### Nested\nprose body\n").unwrap();

        // Span covers all three lines — a code-declaration reader would treat
        // line 0 as "the signature" (kept) and strip the immediately-following
        // "### Nested" as a leading comment/docblock (§376 latent bug). A
        // doc-chunk must return the window untouched.
        let node = make_doc_chunk_node("guide.md", "doc:real-heading", 1, 3);
        let snippet = snippet_for_node(&node, dir.path()).unwrap();
        assert!(snippet.contains("### Nested"), "got: {snippet:?}");
        assert!(snippet.contains("prose body"));
    }

    #[test]
    fn doc_chunk_snippet_rejects_path_traversal_and_absolute_paths() {
        let dir = tempfile::tempdir().unwrap();
        let mut traversal = make_doc_chunk_node("../secret.md", "doc:x", 1, 1);
        traversal.vname.path = "../secret.md".to_string();
        assert!(snippet_for_node(&traversal, dir.path()).is_none());

        let mut absolute = make_doc_chunk_node("/etc/passwd", "doc:x", 1, 1);
        absolute.vname.path = "/etc/passwd".to_string();
        assert!(snippet_for_node(&absolute, dir.path()).is_none());
    }

    #[test]
    fn doc_chunk_uses_the_200_line_cap_not_the_code_default() {
        assert_eq!(snippet_line_cap("doc-chunk"), 200);
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
