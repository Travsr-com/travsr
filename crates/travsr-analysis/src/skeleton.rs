//! AST skeleton generation — structured summaries without line-cap truncation.
//!
//! Called as a fallback from `get_snippets` / `get_context(include_snippets=true)`
//! when a node's full body exceeds the raw-text snippet line cap.
//!
//! For each node this module:
//!   1. Applies the same SEC path guard as `snippet_for_node`.
//!   2. Re-parses the source file with the appropriate Tree-sitter grammar.
//!   3. Walks to the declaration at `node.line` and extracts structure.
//!   4. Returns `AstSkeleton` — signature, params, return type, fields, callees.
//!
//! Supported languages (RFC-017 Phase 5): Rust, TypeScript, Python, Go.
//! All other languages return `None` so callers can fall through to header-only.
//!
//! Invariant: this module depends ONLY on travsr-core and tree-sitter crates.

use std::path::Path;

use travsr_core::Node;
use tree_sitter::{Node as TsNode, Parser};

// ── Public types ──────────────────────────────────────────────────────────────

/// Structured summary of a declaration extracted from its live AST.
#[derive(Debug, Clone, Default)]
pub struct AstSkeleton {
    /// Matches the graph node kind: "function", "class", "struct", "enum", etc.
    pub kind: String,
    /// First line of the declaration (function signature, struct/class header, etc.)
    pub signature: String,
    /// Parameter list entries (name + type where available).
    pub params: Vec<String>,
    /// Return type string, if present and extractable.
    pub return_type: Option<String>,
    /// Struct fields, class members, enum variants, or trait method stubs.
    pub fields: Vec<String>,
    /// Unresolved direct callee identifiers within the body (structural, not semantic).
    /// Capped at 20. Member calls are de-qualified: `self.foo()` → `foo`.
    pub callees: Vec<String>,
    /// Rough token estimate: `(declaration_byte_length / 4).max(512)`.
    pub token_estimate: usize,
}

impl AstSkeleton {
    /// Format as a compact, LLM-readable text block.
    pub fn render(&self) -> String {
        let mut out: Vec<String> = Vec::new();
        out.push(format!("[skeleton: {}]", self.kind));
        out.push(self.signature.clone());
        if !self.params.is_empty() {
            out.push(format!("  params: {}", self.params.join(", ")));
        }
        if let Some(r) = &self.return_type {
            out.push(format!("  returns: {r}"));
        }
        if !self.fields.is_empty() {
            let label = match self.kind.as_str() {
                "impl" | "trait" => "methods",
                "interface" => "members",
                "enum" => "variants",
                _ => "fields",
            };
            out.push(format!("  {label}: {}", self.fields.join(", ")));
        }
        if !self.callees.is_empty() {
            out.push(format!("  calls: {}", self.callees.join(", ")));
        }
        out.join("\n")
    }
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Generate a structured skeleton for `node`, re-parsing its source file on demand.
///
/// Returns `None` when:
/// - `node.line` is absent (file-kind or synthetic nodes)
/// - the language is unsupported (non-Rust/TS/Python/Go)
/// - the source file cannot be read (stale index, file deleted)
/// - `vname.path` would escape `repo_root` (SEC path-traversal guard)
pub fn skeleton_for_node(node: &Node, repo_root: &Path) -> Option<AstSkeleton> {
    let start_row = node.line?.saturating_sub(1) as usize; // 1-based → 0-based

    if node.vname.path.is_empty() {
        return None;
    }

    // SEC: reject path traversal / absolute paths — identical to snippet_for_node.
    let p = &node.vname.path;
    let looks_absolute = p.starts_with('/')
        || p.starts_with('\\')
        || p.get(1..3)
            .map(|s| s == ":\\" || s == ":/")
            .unwrap_or(false);
    if looks_absolute || p.contains("..") {
        tracing::debug!(path = %p, "skeleton_for_node: rejecting traversal/absolute path");
        return None;
    }
    let abs = repo_root.join(p);
    let canon_abs = abs.canonicalize().unwrap_or_else(|_| abs.clone());
    let canon_root = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf());
    if !canon_abs.starts_with(&canon_root) {
        tracing::warn!(path = %p, "skeleton_for_node: path escapes repo_root — skipping");
        return None;
    }

    let lang = detect_lang(node)?;
    let ts_lang = grammar_for(&lang, p);
    let src = std::fs::read(&canon_abs).ok()?;

    let mut parser = Parser::new();
    parser.set_language(&ts_lang).ok()?;
    let tree = parser.parse(&src, None)?;

    let decl = find_decl_at_row(tree.root_node(), start_row, &lang)?;
    Some(extract_skeleton(decl, &src, node, &lang))
}

// ── Language detection ────────────────────────────────────────────────────────

enum Lang {
    Rust,
    TypeScript { tsx: bool },
    Python,
    Go,
}

fn detect_lang(node: &Node) -> Option<Lang> {
    match node.vname.language.as_str() {
        "rust" => return Some(Lang::Rust),
        "typescript" => {
            let tsx = node.vname.path.ends_with(".tsx") || node.vname.path.ends_with(".jsx");
            return Some(Lang::TypeScript { tsx });
        }
        "python" => return Some(Lang::Python),
        "go" => return Some(Lang::Go),
        _ => {}
    }
    let ext = node.vname.path.rsplit('.').next().unwrap_or("");
    match ext {
        "rs" => Some(Lang::Rust),
        "ts" | "mts" | "cts" | "js" | "mjs" | "cjs" => Some(Lang::TypeScript { tsx: false }),
        "tsx" | "jsx" => Some(Lang::TypeScript { tsx: true }),
        "py" | "pyi" => Some(Lang::Python),
        "go" => Some(Lang::Go),
        _ => None,
    }
}

fn grammar_for(lang: &Lang, _path: &str) -> tree_sitter::Language {
    match lang {
        Lang::Rust => tree_sitter::Language::new(tree_sitter_rust::LANGUAGE),
        Lang::TypeScript { tsx: true } => {
            tree_sitter::Language::new(tree_sitter_typescript::LANGUAGE_TSX)
        }
        Lang::TypeScript { tsx: false } => {
            tree_sitter::Language::new(tree_sitter_typescript::LANGUAGE_TYPESCRIPT)
        }
        Lang::Python => tree_sitter::Language::new(tree_sitter_python::LANGUAGE),
        Lang::Go => tree_sitter::Language::new(tree_sitter_go::LANGUAGE),
    }
}

// ── Declaration finder ────────────────────────────────────────────────────────

fn decl_kinds_for(lang: &Lang) -> &'static [&'static str] {
    match lang {
        Lang::Rust => &[
            "function_item",
            "struct_item",
            "enum_item",
            "trait_item",
            "impl_item",
            "type_item",
            "union_item",
            "const_item",
            "static_item",
        ],
        Lang::TypeScript { .. } => &[
            "function_declaration",
            "method_definition",
            "class_declaration",
            "abstract_class_declaration",
            "interface_declaration",
            "type_alias_declaration",
            "enum_declaration",
        ],
        Lang::Python => &["function_definition", "class_definition"],
        Lang::Go => &[
            "function_declaration",
            "method_declaration",
            "type_declaration",
        ],
    }
}

/// DFS from `root` to find the outermost declaration node that starts at `target_row`.
/// Prunes branches whose byte range doesn't cover the target row — O(depth + siblings).
fn find_decl_at_row<'a>(root: TsNode<'a>, target_row: usize, lang: &Lang) -> Option<TsNode<'a>> {
    find_decl_dfs(root, target_row, decl_kinds_for(lang))
}

fn find_decl_dfs<'a>(node: TsNode<'a>, target_row: usize, kinds: &[&str]) -> Option<TsNode<'a>> {
    // Prune: this subtree ends before target_row or starts after it.
    if node.end_position().row < target_row || node.start_position().row > target_row {
        return None;
    }
    // Match: a declaration kind whose start row is exactly target_row.
    if kinds.contains(&node.kind()) && node.start_position().row == target_row {
        return Some(node);
    }
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i as u32) {
            if let Some(found) = find_decl_dfs(child, target_row, kinds) {
                return Some(found);
            }
        }
    }
    None
}

// ── Skeleton extraction dispatch ──────────────────────────────────────────────

fn extract_skeleton(decl: TsNode<'_>, src: &[u8], node: &Node, lang: &Lang) -> AstSkeleton {
    let token_estimate = ((decl.end_byte() - decl.start_byte()) / 4).max(512);
    match lang {
        Lang::Rust => extract_rust(decl, src, &node.kind, token_estimate),
        Lang::TypeScript { .. } => extract_typescript(decl, src, &node.kind, token_estimate),
        Lang::Python => extract_python(decl, src, &node.kind, token_estimate),
        Lang::Go => extract_go(decl, src, &node.kind, token_estimate),
    }
}

// ── Rust ──────────────────────────────────────────────────────────────────────

fn extract_rust(
    decl: TsNode<'_>,
    src: &[u8],
    node_kind: &str,
    token_estimate: usize,
) -> AstSkeleton {
    let mut params: Vec<String> = Vec::new();
    let mut return_type: Option<String> = None;
    let mut fields: Vec<String> = Vec::new();
    let mut callees: Vec<String> = Vec::new();

    match decl.kind() {
        "function_item" => {
            if let Some(p) = decl.child_by_field_name("parameters") {
                for i in 0..p.named_child_count() {
                    let Some(c) = p.named_child(i as u32) else {
                        continue;
                    };
                    if c.kind() == "parameter" {
                        params.push(node_text(c, src).to_string());
                    }
                }
            }
            if let Some(r) = decl.child_by_field_name("return_type") {
                return_type = Some(node_text(r, src).to_string());
            }
            if let Some(body) = decl.child_by_field_name("body") {
                collect_callees_dfs(body, src, "call_expression", &mut callees);
            }
        }
        "struct_item" => {
            if let Some(body) = decl.child_by_field_name("body") {
                for i in 0..body.named_child_count() {
                    let Some(c) = body.named_child(i as u32) else {
                        continue;
                    };
                    if c.kind() == "field_declaration" {
                        let name = c
                            .child_by_field_name("name")
                            .map(|n| node_text(n, src))
                            .unwrap_or("");
                        let ty = c
                            .child_by_field_name("type")
                            .map(|n| node_text(n, src))
                            .unwrap_or("");
                        if !name.is_empty() {
                            fields.push(format!("{name}: {ty}"));
                        }
                    }
                }
            }
        }
        "enum_item" => {
            if let Some(body) = decl.child_by_field_name("body") {
                for i in 0..body.named_child_count() {
                    let Some(c) = body.named_child(i as u32) else {
                        continue;
                    };
                    if c.kind() == "enum_variant" {
                        if let Some(name) = c.child_by_field_name("name") {
                            fields.push(node_text(name, src).to_string());
                        }
                    }
                }
            }
        }
        "trait_item" => {
            if let Some(body) = decl.child_by_field_name("body") {
                for i in 0..body.named_child_count() {
                    let Some(c) = body.named_child(i as u32) else {
                        continue;
                    };
                    if matches!(c.kind(), "function_signature_item" | "function_item") {
                        fields.push(first_line(c, src));
                    }
                }
            }
        }
        "impl_item" => {
            if let Some(body) = decl.child_by_field_name("body") {
                for i in 0..body.named_child_count() {
                    let Some(c) = body.named_child(i as u32) else {
                        continue;
                    };
                    if c.kind() == "function_item" {
                        fields.push(first_line(c, src));
                    }
                }
            }
        }
        _ => {}
    }

    AstSkeleton {
        kind: node_kind.to_string(),
        signature: decl_header(decl, src),
        params,
        return_type,
        fields,
        callees,
        token_estimate,
    }
}

// ── TypeScript ────────────────────────────────────────────────────────────────

fn extract_typescript(
    decl: TsNode<'_>,
    src: &[u8],
    node_kind: &str,
    token_estimate: usize,
) -> AstSkeleton {
    let mut params: Vec<String> = Vec::new();
    let mut return_type: Option<String> = None;
    let mut fields: Vec<String> = Vec::new();
    let mut callees: Vec<String> = Vec::new();

    match decl.kind() {
        "function_declaration" | "method_definition" => {
            if let Some(p) = decl.child_by_field_name("parameters") {
                for i in 0..p.named_child_count() {
                    let Some(c) = p.named_child(i as u32) else {
                        continue;
                    };
                    if matches!(
                        c.kind(),
                        "required_parameter"
                            | "optional_parameter"
                            | "rest_pattern"
                            | "assignment_pattern"
                    ) {
                        params.push(node_text(c, src).to_string());
                    }
                }
            }
            if let Some(r) = decl.child_by_field_name("return_type") {
                // type_annotation text is ": SomeType" — strip the leading colon.
                let raw = node_text(r, src);
                return_type = Some(raw.trim_start_matches(':').trim().to_string());
            }
            if let Some(body) = decl.child_by_field_name("body") {
                collect_callees_dfs(body, src, "call_expression", &mut callees);
            }
        }
        "class_declaration" | "abstract_class_declaration" => {
            if let Some(body) = decl.child_by_field_name("body") {
                for i in 0..body.named_child_count() {
                    let Some(c) = body.named_child(i as u32) else {
                        continue;
                    };
                    match c.kind() {
                        "method_definition" => fields.push(first_line(c, src)),
                        "public_field_definition" => {
                            fields.push(node_text(c, src).to_string());
                        }
                        _ => {}
                    }
                }
            }
        }
        "interface_declaration" => {
            if let Some(body) = decl.child_by_field_name("body") {
                for i in 0..body.named_child_count() {
                    let Some(c) = body.named_child(i as u32) else {
                        continue;
                    };
                    if matches!(
                        c.kind(),
                        "property_signature" | "method_signature" | "call_signature"
                    ) {
                        fields.push(node_text(c, src).to_string());
                    }
                }
            }
        }
        "enum_declaration" => {
            if let Some(body) = named_child_of_kind(decl, "enum_body") {
                for i in 0..body.named_child_count() {
                    let Some(c) = body.named_child(i as u32) else {
                        continue;
                    };
                    if c.kind() == "property_identifier" || c.kind() == "enum_assignment" {
                        fields.push(node_text(c, src).to_string());
                    }
                }
            }
        }
        _ => {}
    }

    AstSkeleton {
        kind: node_kind.to_string(),
        signature: decl_header(decl, src),
        params,
        return_type,
        fields,
        callees,
        token_estimate,
    }
}

// ── Python ────────────────────────────────────────────────────────────────────

fn extract_python(
    decl: TsNode<'_>,
    src: &[u8],
    node_kind: &str,
    token_estimate: usize,
) -> AstSkeleton {
    let mut params: Vec<String> = Vec::new();
    let mut return_type: Option<String> = None;
    let mut fields: Vec<String> = Vec::new();
    let mut callees: Vec<String> = Vec::new();

    match decl.kind() {
        "function_definition" => {
            if let Some(p) = decl.child_by_field_name("parameters") {
                for i in 0..p.named_child_count() {
                    let Some(c) = p.named_child(i as u32) else {
                        continue;
                    };
                    match c.kind() {
                        "identifier" => {
                            let t = node_text(c, src);
                            if t != "self" && t != "cls" {
                                params.push(t.to_string());
                            }
                        }
                        "default_parameter"
                        | "typed_parameter"
                        | "typed_default_parameter"
                        | "list_splat_pattern"
                        | "dictionary_splat_pattern" => {
                            params.push(node_text(c, src).to_string());
                        }
                        _ => {}
                    }
                }
            }
            if let Some(r) = decl.child_by_field_name("return_type") {
                return_type = Some(node_text(r, src).to_string());
            }
            if let Some(body) = decl.child_by_field_name("body") {
                collect_callees_dfs(body, src, "call", &mut callees);
            }
        }
        "class_definition" => {
            if let Some(body) = decl.child_by_field_name("body") {
                for i in 0..body.named_child_count() {
                    let Some(c) = body.named_child(i as u32) else {
                        continue;
                    };
                    if c.kind() == "function_definition" {
                        if let Some(name) = c.child_by_field_name("name") {
                            fields.push(node_text(name, src).to_string());
                        }
                    }
                }
            }
        }
        _ => {}
    }

    AstSkeleton {
        kind: node_kind.to_string(),
        signature: decl_header(decl, src),
        params,
        return_type,
        fields,
        callees,
        token_estimate,
    }
}

// ── Go ────────────────────────────────────────────────────────────────────────

fn extract_go(decl: TsNode<'_>, src: &[u8], node_kind: &str, token_estimate: usize) -> AstSkeleton {
    let mut params: Vec<String> = Vec::new();
    let mut return_type: Option<String> = None;
    let mut fields: Vec<String> = Vec::new();
    let mut callees: Vec<String> = Vec::new();

    match decl.kind() {
        "function_declaration" | "method_declaration" => {
            if let Some(p) = decl.child_by_field_name("parameters") {
                for i in 0..p.named_child_count() {
                    let Some(c) = p.named_child(i as u32) else {
                        continue;
                    };
                    if matches!(
                        c.kind(),
                        "parameter_declaration" | "variadic_parameter_declaration"
                    ) {
                        params.push(node_text(c, src).to_string());
                    }
                }
            }
            if let Some(r) = decl.child_by_field_name("result") {
                return_type = Some(node_text(r, src).to_string());
            }
            // body is a block — find it as a named child
            for i in 0..decl.named_child_count() {
                let Some(c) = decl.named_child(i as u32) else {
                    continue;
                };
                if c.kind() == "block" {
                    collect_callees_dfs(c, src, "call_expression", &mut callees);
                    break;
                }
            }
        }
        "type_declaration" => {
            // Walk type_spec children to find struct_type or interface_type
            for i in 0..decl.named_child_count() {
                let Some(ts) = decl.named_child(i as u32) else {
                    continue;
                };
                if ts.kind() != "type_spec" {
                    continue;
                }
                let Some(ty) = ts.child_by_field_name("type") else {
                    continue;
                };
                match ty.kind() {
                    "struct_type" => {
                        let fdl = named_child_of_kind(ty, "field_declaration_list")
                            .or_else(|| ty.child_by_field_name("fields"));
                        if let Some(flist) = fdl {
                            for j in 0..flist.named_child_count() {
                                let Some(f) = flist.named_child(j as u32) else {
                                    continue;
                                };
                                if f.kind() == "field_declaration" {
                                    fields.push(node_text(f, src).to_string());
                                }
                            }
                        }
                    }
                    "interface_type" => {
                        for j in 0..ty.named_child_count() {
                            let Some(m) = ty.named_child(j as u32) else {
                                continue;
                            };
                            if matches!(m.kind(), "method_spec" | "type_elem") {
                                fields.push(node_text(m, src).to_string());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }

    AstSkeleton {
        kind: node_kind.to_string(),
        signature: decl_header(decl, src),
        params,
        return_type,
        fields,
        callees,
        token_estimate,
    }
}

// ── Tree-sitter helpers ───────────────────────────────────────────────────────

fn node_text<'a>(n: TsNode<'_>, src: &'a [u8]) -> &'a str {
    n.utf8_text(src).unwrap_or("").trim()
}

fn first_line(n: TsNode<'_>, src: &[u8]) -> String {
    node_text(n, src)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Extract the declaration header up to (but not including) the body block.
///
/// For multi-line signatures like:
///   pub fn parse_file_with_vname(
///       &self,
///       abs_path: &Path,
///   ) -> Result<ParseOutput, IndexError> {
/// this returns the full signature without the opening `{`.
///
/// Falls back to the first line when no body block is found (e.g. struct fields,
/// enum variants, type aliases — their "body" IS their entire text).
fn decl_header(decl: TsNode<'_>, src: &[u8]) -> String {
    // Find where the body block starts via the "body" field (Rust / TS / Python),
    // or by locating the first child node of kind "block" (Go).
    let body_start: Option<usize> = decl
        .child_by_field_name("body")
        .map(|b| b.start_byte())
        .or_else(|| {
            (0..decl.named_child_count())
                .filter_map(|i| decl.named_child(i as u32))
                .find(|c| c.kind() == "block")
                .map(|b| b.start_byte())
        });

    let end = body_start.unwrap_or(decl.end_byte());
    std::str::from_utf8(&src[decl.start_byte()..end])
        .unwrap_or("")
        .trim_end_matches(['{', ':', ' ', '\n', '\r', '\t'])
        .trim()
        .to_string()
}

fn named_child_of_kind<'a>(n: TsNode<'a>, kind: &str) -> Option<TsNode<'a>> {
    for i in 0..n.named_child_count() {
        let c = n.named_child(i as u32)?;
        if c.kind() == kind {
            return Some(c);
        }
    }
    None
}

/// DFS over named children collecting call expression targets.
/// Capped at 20 entries; member calls are de-qualified: `self.foo()` → `foo`.
fn collect_callees_dfs(node: TsNode<'_>, src: &[u8], call_kind: &str, out: &mut Vec<String>) {
    if out.len() >= 20 {
        return;
    }
    if node.kind() == call_kind {
        if let Some(fn_node) = node.child_by_field_name("function") {
            let text = node_text(fn_node, src);
            let callee = text.rsplit('.').next().unwrap_or(text).trim();
            if !callee.is_empty() && callee.len() < 64 {
                let owned = callee.to_string();
                if !out.contains(&owned) {
                    out.push(owned);
                }
            }
        }
    }
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i as u32) {
            collect_callees_dfs(child, src, call_kind, out);
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use travsr_core::VName;

    fn make_node(
        path: &str,
        sig: &str,
        language: &str,
        kind: &str,
        line: u32,
        end_line: u32,
    ) -> Node {
        Node::new(VName::new("corpus", "", path, language, sig), kind)
            .with_line(line)
            .with_end_line(end_line)
    }

    // ── render ────────────────────────────────────────────────────────────────

    #[test]
    fn render_full_skeleton() {
        let s = AstSkeleton {
            kind: "function".to_string(),
            signature: "fn charge(amount: f64) -> Result<()>".to_string(),
            params: vec!["amount: f64".to_string()],
            return_type: Some("Result<()>".to_string()),
            fields: vec![],
            callees: vec!["validate".to_string()],
            token_estimate: 512,
        };
        let r = s.render();
        assert!(r.contains("[skeleton: function]"));
        assert!(r.contains("fn charge"));
        assert!(r.contains("params: amount: f64"));
        assert!(r.contains("returns: Result<()>"));
        assert!(r.contains("calls: validate"));
    }

    #[test]
    fn render_no_optional_fields() {
        let s = AstSkeleton {
            kind: "struct".to_string(),
            signature: "struct Foo".to_string(),
            ..Default::default()
        };
        let r = s.render();
        assert!(r.contains("[skeleton: struct]"));
        assert!(!r.contains("params:"));
        assert!(!r.contains("returns:"));
        assert!(!r.contains("fields:"));
        assert!(!r.contains("calls:"));
    }

    // ── SEC guards ────────────────────────────────────────────────────────────

    #[test]
    fn rejects_path_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let node = make_node("../etc/passwd", "fn:evil", "rust", "function", 1, 1);
        assert!(skeleton_for_node(&node, dir.path()).is_none());
    }

    #[test]
    fn rejects_absolute_unix_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut node = make_node("/etc/passwd", "fn:evil", "rust", "function", 1, 1);
        node.vname.path = "/etc/passwd".to_string();
        assert!(skeleton_for_node(&node, dir.path()).is_none());
    }

    #[test]
    fn rejects_windows_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        for evil in ["C:\\evil.rs", "C:/evil.rs"] {
            let mut node = make_node(evil, "fn:evil", "rust", "function", 1, 1);
            node.vname.path = evil.to_string();
            assert!(skeleton_for_node(&node, dir.path()).is_none(), "{evil}");
        }
    }

    #[test]
    fn missing_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let node = make_node("ghost.rs", "fn:ghost", "rust", "function", 1, 5);
        assert!(skeleton_for_node(&node, dir.path()).is_none());
    }

    #[test]
    fn unsupported_language_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("foo.java");
        std::fs::write(&src, "public class Foo {}").unwrap();
        let node = make_node("foo.java", "class:Foo", "java", "class", 1, 1);
        assert!(skeleton_for_node(&node, dir.path()).is_none());
    }

    // ── Rust ──────────────────────────────────────────────────────────────────

    #[test]
    fn rust_function_extracts_params_and_return() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("lib.rs");
        std::fs::write(
            &src,
            "pub fn charge(amount: f64, currency: &str) -> Result<Receipt, ChargeError> {\n\
                 validate(amount);\n\
                 process(currency)\n\
             }\n",
        )
        .unwrap();
        let node = make_node("lib.rs", "fn:charge", "rust", "function", 1, 4);
        let skel = skeleton_for_node(&node, dir.path()).unwrap();
        assert_eq!(skel.kind, "function");
        assert!(
            skel.signature.contains("fn charge"),
            "sig: {}",
            skel.signature
        );
        assert!(!skel.params.is_empty(), "expected params");
        assert!(skel.return_type.is_some(), "expected return type");
        // callees should include validate and/or process
        let callees_str = skel.callees.join(",");
        assert!(
            callees_str.contains("validate") || callees_str.contains("process"),
            "callees: {callees_str}"
        );
    }

    #[test]
    fn rust_struct_extracts_fields() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("model.rs");
        std::fs::write(
            &src,
            "pub struct PaymentProcessor {\n\
                 pub client: StripeClient,\n\
                 config: Config,\n\
             }\n",
        )
        .unwrap();
        let node = make_node("model.rs", "class:PaymentProcessor", "rust", "struct", 1, 4);
        let skel = skeleton_for_node(&node, dir.path()).unwrap();
        assert!(
            skel.signature.contains("PaymentProcessor"),
            "sig: {}",
            skel.signature
        );
        assert!(!skel.fields.is_empty(), "expected fields");
        let fields_str = skel.fields.join(",");
        assert!(
            fields_str.contains("client") || fields_str.contains("config"),
            "{fields_str}"
        );
    }

    #[test]
    fn rust_enum_extracts_variants() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("status.rs");
        std::fs::write(
            &src,
            "pub enum Status {\n    Pending,\n    Active,\n    Failed,\n}\n",
        )
        .unwrap();
        let node = make_node("status.rs", "enum:Status", "rust", "enum", 1, 5);
        let skel = skeleton_for_node(&node, dir.path()).unwrap();
        let fields_str = skel.fields.join(",");
        assert!(fields_str.contains("Pending"), "variants: {fields_str}");
        assert!(fields_str.contains("Active"), "variants: {fields_str}");
    }

    // ── TypeScript ────────────────────────────────────────────────────────────

    #[test]
    fn typescript_function_extracts_params_and_return() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("billing.ts");
        std::fs::write(
            &src,
            "function charge(amount: number, currency: string): Promise<Receipt> {\n\
                 validate(amount);\n\
                 return process(currency);\n\
             }\n",
        )
        .unwrap();
        let node = make_node("billing.ts", "fn:charge", "typescript", "function", 1, 4);
        let skel = skeleton_for_node(&node, dir.path()).unwrap();
        assert!(
            skel.signature.contains("function charge"),
            "sig: {}",
            skel.signature
        );
        assert!(!skel.params.is_empty(), "expected params");
        assert!(skel.return_type.is_some(), "expected return type");
    }

    #[test]
    fn typescript_class_extracts_members() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("service.ts");
        std::fs::write(
            &src,
            "class PaymentService {\n\
                 private client: StripeClient;\n\
                 constructor(client: StripeClient) {}\n\
                 charge(amount: number): void {}\n\
             }\n",
        )
        .unwrap();
        let node = make_node(
            "service.ts",
            "class:PaymentService",
            "typescript",
            "class",
            1,
            5,
        );
        let skel = skeleton_for_node(&node, dir.path()).unwrap();
        assert!(
            skel.signature.contains("PaymentService"),
            "sig: {}",
            skel.signature
        );
        assert!(!skel.fields.is_empty(), "expected class members");
    }

    // ── Python ────────────────────────────────────────────────────────────────

    #[test]
    fn python_function_extracts_params_and_return() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("billing.py");
        std::fs::write(
            &src,
            "def charge(amount: float, currency: str) -> Decimal:\n\
                 validate(amount)\n\
                 return process(currency)\n",
        )
        .unwrap();
        let node = make_node("billing.py", "fn:charge", "python", "function", 1, 3);
        let skel = skeleton_for_node(&node, dir.path()).unwrap();
        assert!(
            skel.signature.contains("def charge"),
            "sig: {}",
            skel.signature
        );
        assert!(!skel.params.is_empty(), "expected params");
        assert!(skel.return_type.is_some(), "expected return type");
    }

    #[test]
    fn python_class_extracts_methods() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("service.py");
        std::fs::write(
            &src,
            "class PaymentService:\n\n    def __init__(self, client):\n        self.client = client\n\n    def charge(self, amount):\n        return self.client.charge(amount)\n",
        )
        .unwrap();
        let node = make_node(
            "service.py",
            "class:PaymentService",
            "python",
            "class",
            1,
            7,
        );
        let skel = skeleton_for_node(&node, dir.path()).unwrap();
        assert!(
            skel.signature.contains("PaymentService"),
            "sig: {}",
            skel.signature
        );
        // methods should appear in fields
        let fields_str = skel.fields.join(",");
        assert!(
            fields_str.contains("__init__") || fields_str.contains("charge"),
            "fields: {fields_str}"
        );
    }

    // ── Go ────────────────────────────────────────────────────────────────────

    #[test]
    fn go_function_extracts_params_and_return() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("billing.go");
        std::fs::write(
            &src,
            "package billing\n\
             \n\
             func Charge(amount float64, currency string) (*Receipt, error) {\n\
             \treturn process(amount, currency)\n\
             }\n",
        )
        .unwrap();
        let node = make_node("billing.go", "fn:Charge", "go", "function", 3, 5);
        let skel = skeleton_for_node(&node, dir.path()).unwrap();
        assert!(
            skel.signature.contains("func Charge"),
            "sig: {}",
            skel.signature
        );
        assert!(!skel.params.is_empty(), "expected params");
        assert!(skel.return_type.is_some(), "expected return type");
    }

    // ── token_estimate ────────────────────────────────────────────────────────

    #[test]
    fn token_estimate_minimum_is_512() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("tiny.rs");
        std::fs::write(&src, "fn foo() {}\n").unwrap();
        let node = make_node("tiny.rs", "fn:foo", "rust", "function", 1, 1);
        let skel = skeleton_for_node(&node, dir.path()).unwrap();
        assert!(skel.token_estimate >= 512, "got {}", skel.token_estimate);
    }

    // ── callee dedup + cap ────────────────────────────────────────────────────

    #[test]
    fn callees_are_deduplicated() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("dup.rs");
        std::fs::write(&src, "fn foo() {\n    bar();\n    bar();\n    bar();\n}\n").unwrap();
        let node = make_node("dup.rs", "fn:foo", "rust", "function", 1, 5);
        let skel = skeleton_for_node(&node, dir.path()).unwrap();
        let bar_count = skel.callees.iter().filter(|c| *c == "bar").count();
        assert_eq!(
            bar_count, 1,
            "callees should be deduplicated: {:?}",
            skel.callees
        );
    }
}
