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
//! Supported languages: Rust, TypeScript, Python, Go, Java, Kotlin, C, C++,
//! C#, Swift, Dart, Ruby, Scala, PHP, Objective-C (all 15 languages).
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
/// - the language is unsupported
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
    Java,
    Kotlin,
    C,
    Cpp,
    CSharp,
    Swift,
    Dart,
    Ruby,
    Scala,
    Php,
    ObjectiveC,
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
        "java" => return Some(Lang::Java),
        "kotlin" => return Some(Lang::Kotlin),
        "c" => return Some(Lang::C),
        "cpp" | "c++" => return Some(Lang::Cpp),
        "csharp" | "c#" => return Some(Lang::CSharp),
        "swift" => return Some(Lang::Swift),
        "dart" => return Some(Lang::Dart),
        "ruby" => return Some(Lang::Ruby),
        "scala" => return Some(Lang::Scala),
        "php" => return Some(Lang::Php),
        "objectivec" | "objective-c" | "objc" => return Some(Lang::ObjectiveC),
        _ => {}
    }
    let ext = node.vname.path.rsplit('.').next().unwrap_or("");
    match ext {
        "rs" => Some(Lang::Rust),
        "ts" | "mts" | "cts" | "js" | "mjs" | "cjs" => Some(Lang::TypeScript { tsx: false }),
        "tsx" | "jsx" => Some(Lang::TypeScript { tsx: true }),
        "py" | "pyi" => Some(Lang::Python),
        "go" => Some(Lang::Go),
        "java" => Some(Lang::Java),
        "kt" | "kts" => Some(Lang::Kotlin),
        "c" | "h" => Some(Lang::C),
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" | "c++" => Some(Lang::Cpp),
        "cs" => Some(Lang::CSharp),
        "swift" => Some(Lang::Swift),
        "dart" => Some(Lang::Dart),
        "rb" | "rake" | "gemspec" => Some(Lang::Ruby),
        "scala" | "sc" => Some(Lang::Scala),
        "php" | "phtml" | "php3" | "php4" | "php5" | "php8" => Some(Lang::Php),
        "m" | "mm" => Some(Lang::ObjectiveC),
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
        Lang::Java => tree_sitter::Language::new(tree_sitter_java::LANGUAGE),
        Lang::Kotlin => (crate::kotlin::CONFIG.get_grammar)(),
        Lang::C => (crate::c::CONFIG.get_grammar)(),
        Lang::Cpp => (crate::cpp::CONFIG.get_grammar)(),
        Lang::CSharp => (crate::csharp::CONFIG.get_grammar)(),
        Lang::Swift => (crate::swift::CONFIG.get_grammar)(),
        Lang::Dart => (crate::dart::CONFIG.get_grammar)(),
        Lang::Ruby => (crate::ruby::CONFIG.get_grammar)(),
        Lang::Scala => (crate::scala::CONFIG.get_grammar)(),
        Lang::Php => (crate::php::CONFIG.get_grammar)(),
        Lang::ObjectiveC => (crate::objc::CONFIG.get_grammar)(),
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
        Lang::Java => &[
            "class_declaration",
            "interface_declaration",
            "enum_declaration",
            "record_declaration",
            "annotation_type_declaration",
            "method_declaration",
            "constructor_declaration",
        ],
        Lang::Kotlin => &[
            "class_declaration",
            "object_declaration",
            "function_declaration",
            "secondary_constructor",
        ],
        Lang::C => &["function_definition", "struct_specifier", "enum_specifier"],
        Lang::Cpp => &[
            "function_definition",
            "class_specifier",
            "struct_specifier",
            "template_declaration",
            "enum_specifier",
        ],
        Lang::CSharp => &[
            "class_declaration",
            "interface_declaration",
            "enum_declaration",
            "struct_declaration",
            "record_declaration",
            "record_struct_declaration",
            "method_declaration",
            "constructor_declaration",
        ],
        Lang::Swift => &[
            "function_declaration",
            "init_declaration",
            "class_declaration", // covers struct/enum/actor/extension too (declaration_kind field)
            "protocol_declaration",
        ],
        Lang::Dart => &[
            "class_declaration",
            "mixin_declaration",
            "enum_declaration",
            "function_declaration",
            "method_declaration",
        ],
        Lang::Ruby => &["method", "singleton_method", "class", "module"],
        Lang::Scala => &[
            "function_definition",
            "class_definition",
            "object_definition",
            "trait_definition",
        ],
        Lang::Php => &[
            "function_definition",
            "method_declaration",
            "class_declaration",
            "interface_declaration",
            "enum_declaration",
        ],
        Lang::ObjectiveC => &[
            "class_interface",
            "class_implementation",
            "protocol_declaration",
            "method_definition",
            "function_definition",
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
        Lang::Java => extract_java(decl, src, &node.kind, token_estimate),
        Lang::Kotlin => extract_kotlin(decl, src, &node.kind, token_estimate),
        Lang::C => extract_c(decl, src, &node.kind, token_estimate),
        Lang::Cpp => extract_cpp(decl, src, &node.kind, token_estimate),
        Lang::CSharp => extract_csharp(decl, src, &node.kind, token_estimate),
        Lang::Swift => extract_swift(decl, src, &node.kind, token_estimate),
        Lang::Dart => extract_dart(decl, src, &node.kind, token_estimate),
        Lang::Ruby => extract_ruby(decl, src, &node.kind, token_estimate),
        Lang::Scala => extract_scala(decl, src, &node.kind, token_estimate),
        Lang::Php => extract_php(decl, src, &node.kind, token_estimate),
        Lang::ObjectiveC => extract_objc(decl, src, &node.kind, token_estimate),
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
                            if matches!(m.kind(), "method_elem" | "type_elem") {
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

// ── Java ──────────────────────────────────────────────────────────────────────

fn extract_java(
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
        "method_declaration" => {
            // formal_parameters -> formal_parameter children
            if let Some(fp) = decl.child_by_field_name("parameters") {
                collect_formal_params(
                    fp,
                    &["formal_parameter", "spread_parameter"],
                    src,
                    &mut params,
                );
            }
            // return type is the `type` field
            if let Some(rt) = decl.child_by_field_name("type") {
                return_type = Some(node_text(rt, src).to_string());
            }
            if let Some(body) = decl.child_by_field_name("body") {
                // method_invocation -> name field (not "function")
                collect_callees_field(body, src, "method_invocation", "name", &mut callees);
            }
        }
        "constructor_declaration" => {
            if let Some(fp) = decl.child_by_field_name("parameters") {
                collect_formal_params(
                    fp,
                    &["formal_parameter", "spread_parameter"],
                    src,
                    &mut params,
                );
            }
            if let Some(body) = decl.child_by_field_name("body") {
                collect_callees_field(body, src, "method_invocation", "name", &mut callees);
            }
        }
        "class_declaration" | "record_declaration" => {
            if let Some(body) = decl.child_by_field_name("body") {
                for i in 0..body.named_child_count() {
                    let Some(c) = body.named_child(i as u32) else {
                        continue;
                    };
                    match c.kind() {
                        "method_declaration" | "constructor_declaration" => {
                            fields.push(first_line(c, src))
                        }
                        "field_declaration" => fields.push(node_text(c, src).to_string()),
                        _ => {}
                    }
                }
            }
        }
        "interface_declaration" | "annotation_type_declaration" => {
            if let Some(body) = decl.child_by_field_name("body") {
                for i in 0..body.named_child_count() {
                    let Some(c) = body.named_child(i as u32) else {
                        continue;
                    };
                    if matches!(c.kind(), "method_declaration" | "constant_declaration") {
                        fields.push(first_line(c, src));
                    }
                }
            }
        }
        "enum_declaration" => {
            if let Some(body) = decl.child_by_field_name("body") {
                for i in 0..body.named_child_count() {
                    let Some(c) = body.named_child(i as u32) else {
                        continue;
                    };
                    if c.kind() == "enum_constant" {
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

// ── Kotlin ────────────────────────────────────────────────────────────────────

fn extract_kotlin(
    decl: TsNode<'_>,
    src: &[u8],
    node_kind: &str,
    token_estimate: usize,
) -> AstSkeleton {
    let mut params: Vec<String> = Vec::new();
    let mut fields: Vec<String> = Vec::new();
    let mut callees: Vec<String> = Vec::new();

    match decl.kind() {
        "function_declaration" | "secondary_constructor" => {
            // Parameters: function_value_parameters is a named child (not a field)
            // tree-sitter-kotlin-ng: function_value_parameters -> parameter (direct children)
            if let Some(fp) = named_child_of_kind(decl, "function_value_parameters") {
                for i in 0..fp.named_child_count() {
                    let Some(p) = fp.named_child(i as u32) else {
                        continue;
                    };
                    if p.kind() == "parameter" {
                        params.push(node_text(p, src).to_string());
                    }
                }
            }
            // Body is a function_body named child; collect callees from it
            if let Some(body) = named_child_of_kind(decl, "function_body") {
                collect_callees_dfs(body, src, "call_expression", &mut callees);
            }
        }
        "class_declaration" | "object_declaration" => {
            if let Some(body) = named_child_of_kind(decl, "class_body") {
                for i in 0..body.named_child_count() {
                    let Some(c) = body.named_child(i as u32) else {
                        continue;
                    };
                    if matches!(c.kind(), "function_declaration" | "secondary_constructor") {
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
        return_type: None, // shown in signature; no dedicated field in tree-sitter-kotlin-ng
        fields,
        callees,
        token_estimate,
    }
}

// ── C ─────────────────────────────────────────────────────────────────────────

fn extract_c(decl: TsNode<'_>, src: &[u8], node_kind: &str, token_estimate: usize) -> AstSkeleton {
    let mut params: Vec<String> = Vec::new();
    let mut return_type: Option<String> = None;
    let mut fields: Vec<String> = Vec::new();
    let mut callees: Vec<String> = Vec::new();

    match decl.kind() {
        "function_definition" => {
            // Return type is the `type` field
            if let Some(rt) = decl.child_by_field_name("type") {
                return_type = Some(node_text(rt, src).to_string());
            }
            // Params: declarator -> function_declarator -> parameters (parameter_list)
            if let Some(declarator) = decl.child_by_field_name("declarator") {
                if let Some(fd) = first_descendant_of_kind(declarator, "function_declarator") {
                    if let Some(pl) = fd.child_by_field_name("parameters") {
                        for i in 0..pl.named_child_count() {
                            let Some(p) = pl.named_child(i as u32) else {
                                continue;
                            };
                            if p.kind() == "parameter_declaration" {
                                params.push(node_text(p, src).to_string());
                            }
                        }
                    }
                }
            }
            if let Some(body) = decl.child_by_field_name("body") {
                collect_callees_dfs(body, src, "call_expression", &mut callees);
            }
        }
        "struct_specifier" => {
            if let Some(body) = decl.child_by_field_name("body") {
                for i in 0..body.named_child_count() {
                    let Some(c) = body.named_child(i as u32) else {
                        continue;
                    };
                    if c.kind() == "field_declaration" {
                        fields.push(node_text(c, src).to_string());
                    }
                }
            }
        }
        "enum_specifier" => {
            if let Some(body) = decl.child_by_field_name("body") {
                for i in 0..body.named_child_count() {
                    let Some(c) = body.named_child(i as u32) else {
                        continue;
                    };
                    if c.kind() == "enumerator" {
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

// ── C++ ───────────────────────────────────────────────────────────────────────

fn extract_cpp(
    decl: TsNode<'_>,
    src: &[u8],
    node_kind: &str,
    token_estimate: usize,
) -> AstSkeleton {
    // function_definition and struct_specifier/enum_specifier share logic with C
    match decl.kind() {
        "function_definition" | "struct_specifier" | "enum_specifier" => {
            return extract_c(decl, src, node_kind, token_estimate)
        }
        _ => {}
    }

    let mut fields: Vec<String> = Vec::new();

    match decl.kind() {
        "class_specifier" => {
            if let Some(body) = decl.child_by_field_name("body") {
                // field_declaration_list contains methods and fields
                for i in 0..body.named_child_count() {
                    let Some(c) = body.named_child(i as u32) else {
                        continue;
                    };
                    match c.kind() {
                        "function_definition" | "declaration" => fields.push(first_line(c, src)),
                        "field_declaration" => fields.push(node_text(c, src).to_string()),
                        _ => {}
                    }
                }
            }
        }
        "template_declaration" => {
            // Template params shown in signature; delegate member extraction
            // to the inner declaration if it's a class or struct
            for i in 0..decl.named_child_count() {
                let Some(c) = decl.named_child(i as u32) else {
                    continue;
                };
                if matches!(c.kind(), "class_specifier" | "struct_specifier") {
                    if let Some(body) = c.child_by_field_name("body") {
                        for j in 0..body.named_child_count() {
                            let Some(m) = body.named_child(j as u32) else {
                                continue;
                            };
                            if matches!(m.kind(), "function_definition" | "declaration") {
                                fields.push(first_line(m, src));
                            }
                        }
                    }
                    break;
                }
            }
        }
        _ => {}
    }

    AstSkeleton {
        kind: node_kind.to_string(),
        signature: decl_header(decl, src),
        params: vec![],
        return_type: None,
        fields,
        callees: vec![],
        token_estimate,
    }
}

// ── C# ────────────────────────────────────────────────────────────────────────

fn extract_csharp(
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
        "method_declaration" => {
            if let Some(fp) = decl.child_by_field_name("parameters") {
                collect_formal_params(fp, &["parameter"], src, &mut params);
            }
            // C# uses `returns` (not `type`) for the return type field
            if let Some(rt) = decl.child_by_field_name("returns") {
                return_type = Some(node_text(rt, src).to_string());
            }
            if let Some(body) = decl.child_by_field_name("body") {
                collect_callees_dfs(body, src, "invocation_expression", &mut callees);
            }
        }
        "constructor_declaration" => {
            if let Some(fp) = decl.child_by_field_name("parameters") {
                collect_formal_params(fp, &["parameter"], src, &mut params);
            }
            if let Some(body) = decl.child_by_field_name("body") {
                collect_callees_dfs(body, src, "invocation_expression", &mut callees);
            }
        }
        "class_declaration"
        | "interface_declaration"
        | "struct_declaration"
        | "record_declaration"
        | "record_struct_declaration" => {
            if let Some(body) = decl.child_by_field_name("body") {
                for i in 0..body.named_child_count() {
                    let Some(c) = body.named_child(i as u32) else {
                        continue;
                    };
                    match c.kind() {
                        "method_declaration" | "constructor_declaration" => {
                            fields.push(first_line(c, src))
                        }
                        "field_declaration" | "property_declaration" => {
                            fields.push(first_line(c, src))
                        }
                        _ => {}
                    }
                }
            }
        }
        "enum_declaration" => {
            if let Some(body) = decl.child_by_field_name("body") {
                for i in 0..body.named_child_count() {
                    let Some(c) = body.named_child(i as u32) else {
                        continue;
                    };
                    if c.kind() == "enum_member_declaration" {
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

// ── Swift ─────────────────────────────────────────────────────────────────────

fn extract_swift(
    decl: TsNode<'_>,
    src: &[u8],
    node_kind: &str,
    token_estimate: usize,
) -> AstSkeleton {
    let mut params: Vec<String> = Vec::new();
    let mut return_type: Option<String> = None;
    let mut fields: Vec<String> = Vec::new();

    match decl.kind() {
        "function_declaration" | "protocol_function_declaration" => {
            // return_type is a named field
            if let Some(rt) = decl.child_by_field_name("return_type") {
                return_type = Some(node_text(rt, src).to_string());
            }
            // Swift `parameter` nodes are direct named children of function_declaration
            // (no function_value_parameters wrapper exists in tree-sitter-swift 0.7)
            for i in 0..decl.named_child_count() {
                let Some(c) = decl.named_child(i as u32) else {
                    continue;
                };
                if c.kind() == "parameter" {
                    params.push(node_text(c, src).to_string());
                }
            }
        }
        "init_declaration" => {
            for i in 0..decl.named_child_count() {
                let Some(c) = decl.named_child(i as u32) else {
                    continue;
                };
                if c.kind() == "parameter" {
                    params.push(node_text(c, src).to_string());
                }
            }
        }
        "class_declaration" | "protocol_declaration" => {
            // class_declaration covers class/struct/enum/actor/extension (declaration_kind field)
            if let Some(body) = decl.child_by_field_name("body") {
                if body.kind() == "enum_class_body" {
                    // Swift enums: collect enum_entry names as variants
                    for i in 0..body.named_child_count() {
                        let Some(c) = body.named_child(i as u32) else {
                            continue;
                        };
                        if c.kind() == "enum_entry" {
                            if let Some(name) = c.child_by_field_name("name") {
                                fields.push(node_text(name, src).to_string());
                            }
                        }
                    }
                } else {
                    for i in 0..body.named_child_count() {
                        let Some(c) = body.named_child(i as u32) else {
                            continue;
                        };
                        match c.kind() {
                            "function_declaration"
                            | "init_declaration"
                            | "deinit_declaration"
                            | "protocol_function_declaration" => fields.push(first_line(c, src)),
                            _ => {}
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
        callees: vec![], // call_expression in Swift has no named fields
        token_estimate,
    }
}

// ── Dart ──────────────────────────────────────────────────────────────────────

fn extract_dart(
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
        "function_declaration"
        | "method_declaration"
        | "getter_declaration"
        | "setter_declaration" => {
            // The `signature` field is function_signature for function_declaration, but
            // method_signature for method_declaration. method_signature has no named fields —
            // its first child is function_signature (or getter/setter/constructor_signature).
            // Descend one level when needed to reach function_signature.
            if let Some(sig_outer) = decl.child_by_field_name("signature") {
                let sig = if sig_outer.kind() == "method_signature" {
                    named_child_of_kind(sig_outer, "function_signature").unwrap_or(sig_outer)
                } else {
                    sig_outer
                };
                if let Some(rt) = sig.child_by_field_name("return_type") {
                    return_type = Some(node_text(rt, src).to_string());
                }
                if let Some(fp) = sig.child_by_field_name("parameters") {
                    for i in 0..fp.named_child_count() {
                        let Some(p) = fp.named_child(i as u32) else {
                            continue;
                        };
                        if p.kind() == "formal_parameter" {
                            params.push(node_text(p, src).to_string());
                        }
                    }
                }
            }
            if let Some(body) = decl.child_by_field_name("body") {
                collect_callees_dfs(body, src, "call_expression", &mut callees);
            }
        }
        "class_declaration" => {
            // class_body is a named child (not a field); members are wrapped in class_member
            if let Some(body) = named_child_of_kind(decl, "class_body") {
                for i in 0..body.named_child_count() {
                    let Some(member) = body.named_child(i as u32) else {
                        continue;
                    };
                    // class_member wraps each member; skip annotation nodes to find actual decl
                    let member = if member.kind() == "class_member" {
                        let mut inner = member;
                        for j in 0..member.named_child_count() {
                            let Some(c) = member.named_child(j as u32) else {
                                continue;
                            };
                            if c.kind() != "annotation" {
                                inner = c;
                                break;
                            }
                        }
                        inner
                    } else {
                        member
                    };
                    match member.kind() {
                        "method_declaration"
                        | "function_declaration"
                        | "getter_declaration"
                        | "setter_declaration" => {
                            // Annotation is a child of method_declaration in tree-sitter-dart;
                            // use the `signature` field's first line to skip past it.
                            let sig = member.child_by_field_name("signature").unwrap_or(member);
                            fields.push(first_line(sig, src));
                        }
                        "declaration" => {
                            // constructor_signature or field_declaration wrapped in declaration
                            if let Some(inner) = member.named_child(0) {
                                if inner.kind() == "constructor_signature" {
                                    fields.push(first_line(inner, src));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        "mixin_declaration" => {
            if let Some(body) = decl.child_by_field_name("body") {
                for i in 0..body.named_child_count() {
                    let Some(c) = body.named_child(i as u32) else {
                        continue;
                    };
                    let c = if c.kind() == "class_member" {
                        c.named_child(0).unwrap_or(c)
                    } else {
                        c
                    };
                    if matches!(
                        c.kind(),
                        "method_declaration" | "getter_declaration" | "setter_declaration"
                    ) {
                        let sig = c.child_by_field_name("signature").unwrap_or(c);
                        fields.push(first_line(sig, src));
                    }
                }
            }
        }
        "enum_declaration" => {
            if let Some(body) = decl.child_by_field_name("body") {
                for i in 0..body.named_child_count() {
                    let Some(c) = body.named_child(i as u32) else {
                        continue;
                    };
                    if c.kind() == "enum_constant" {
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

// ── Ruby ──────────────────────────────────────────────────────────────────────

fn extract_ruby(
    decl: TsNode<'_>,
    src: &[u8],
    node_kind: &str,
    token_estimate: usize,
) -> AstSkeleton {
    let mut params: Vec<String> = Vec::new();
    let mut fields: Vec<String> = Vec::new();
    let mut callees: Vec<String> = Vec::new();

    match decl.kind() {
        "method" | "singleton_method" => {
            // parameters field -> method_parameters -> various param kinds
            if let Some(fp) = decl.child_by_field_name("parameters") {
                for i in 0..fp.named_child_count() {
                    let Some(p) = fp.named_child(i as u32) else {
                        continue;
                    };
                    match p.kind() {
                        "identifier"
                        | "optional_parameter"
                        | "splat_parameter"
                        | "hash_splat_parameter"
                        | "block_parameter"
                        | "keyword_parameter"
                        | "destructured_parameter" => {
                            params.push(node_text(p, src).to_string());
                        }
                        _ => {}
                    }
                }
            }
            if let Some(body) = decl.child_by_field_name("body") {
                // Ruby call nodes: `call` -> method field
                collect_callees_field(body, src, "call", "method", &mut callees);
            }
        }
        "class" | "module" => {
            if let Some(body) = decl.child_by_field_name("body") {
                for i in 0..body.named_child_count() {
                    let Some(c) = body.named_child(i as u32) else {
                        continue;
                    };
                    if matches!(c.kind(), "method" | "singleton_method") {
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
        return_type: None, // Ruby is dynamically typed
        fields,
        callees,
        token_estimate,
    }
}

// ── Scala ─────────────────────────────────────────────────────────────────────

fn extract_scala(
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
            // parameters and return_type are named fields in tree-sitter-scala
            if let Some(fp) = decl.child_by_field_name("parameters") {
                for i in 0..fp.named_child_count() {
                    let Some(p) = fp.named_child(i as u32) else {
                        continue;
                    };
                    // Accept both `parameter` and `class_parameter`
                    if matches!(p.kind(), "parameter" | "class_parameter") {
                        params.push(node_text(p, src).to_string());
                    }
                }
            }
            if let Some(rt) = decl.child_by_field_name("return_type") {
                return_type = Some(node_text(rt, src).to_string());
            }
            if let Some(body) = decl.child_by_field_name("body") {
                collect_callees_dfs(body, src, "call_expression", &mut callees);
            }
        }
        "class_definition" | "object_definition" | "trait_definition" => {
            if let Some(body) = decl.child_by_field_name("body") {
                // body is template_body
                for i in 0..body.named_child_count() {
                    let Some(c) = body.named_child(i as u32) else {
                        continue;
                    };
                    match c.kind() {
                        "function_definition" => fields.push(first_line(c, src)),
                        "val_definition" | "var_definition" | "val_declaration"
                        | "var_declaration" => fields.push(first_line(c, src)),
                        _ => {}
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

// ── PHP ───────────────────────────────────────────────────────────────────────

fn extract_php(
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
        "function_definition" | "method_declaration" => {
            if let Some(fp) = decl.child_by_field_name("parameters") {
                collect_formal_params(
                    fp,
                    &[
                        "simple_parameter",
                        "variadic_parameter",
                        "property_promotion_parameter",
                    ],
                    src,
                    &mut params,
                );
            }
            if let Some(rt) = decl.child_by_field_name("return_type") {
                return_type = Some(node_text(rt, src).to_string());
            }
            if let Some(body) = decl.child_by_field_name("body") {
                collect_callees_dfs(body, src, "function_call_expression", &mut callees);
                // member calls use `name` field
                collect_callees_field(body, src, "member_call_expression", "name", &mut callees);
            }
        }
        "class_declaration" | "interface_declaration" => {
            if let Some(body) = decl.child_by_field_name("body") {
                for i in 0..body.named_child_count() {
                    let Some(c) = body.named_child(i as u32) else {
                        continue;
                    };
                    match c.kind() {
                        "method_declaration" => fields.push(first_line(c, src)),
                        "property_declaration" => fields.push(first_line(c, src)),
                        _ => {}
                    }
                }
            }
        }
        "enum_declaration" => {
            if let Some(body) = decl.child_by_field_name("body") {
                for i in 0..body.named_child_count() {
                    let Some(c) = body.named_child(i as u32) else {
                        continue;
                    };
                    if c.kind() == "enum_case" {
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

// ── Objective-C ───────────────────────────────────────────────────────────────

fn extract_objc(
    decl: TsNode<'_>,
    src: &[u8],
    node_kind: &str,
    token_estimate: usize,
) -> AstSkeleton {
    let mut fields: Vec<String> = Vec::new();
    let mut callees: Vec<String> = Vec::new();

    match decl.kind() {
        "function_definition" => {
            // ObjC C-style functions are identical to C
            return extract_c(decl, src, node_kind, token_estimate);
        }
        "method_definition" => {
            // ObjC method body is compound_statement (not a field — added to decl_header fallback)
            if let Some(body) = named_child_of_kind(decl, "compound_statement") {
                // message_expression -> method field gives the selector
                collect_callees_field(body, src, "message_expression", "method", &mut callees);
            }
        }
        "class_interface" | "protocol_declaration" => {
            // method_declaration nodes are direct named children of @interface / @protocol
            for i in 0..decl.named_child_count() {
                let Some(c) = decl.named_child(i as u32) else {
                    continue;
                };
                if c.kind() == "method_declaration" {
                    fields.push(first_line(c, src));
                }
            }
        }
        "class_implementation" => {
            // method_definition nodes live inside implementation_definition, not directly
            // on class_implementation (verified against tree-sitter-objc-3.0.2 node-types.json)
            if let Some(impl_def) = named_child_of_kind(decl, "implementation_definition") {
                for i in 0..impl_def.named_child_count() {
                    let Some(c) = impl_def.named_child(i as u32) else {
                        continue;
                    };
                    if c.kind() == "method_definition" {
                        fields.push(first_line(c, src));
                    }
                }
            }
        }
        _ => {}
    }

    // For ObjC @interface/@implementation/@protocol, first_line gives a cleaner
    // signature than decl_header (no body block to strip, just the opening line).
    let signature = match decl.kind() {
        "class_interface" | "class_implementation" | "protocol_declaration" => {
            first_line(decl, src)
        }
        _ => decl_header(decl, src),
    };

    AstSkeleton {
        kind: node_kind.to_string(),
        signature,
        params: vec![], // ObjC selector syntax is fully in the signature
        return_type: None,
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
/// Recognises `body` fields (Rust / TS / Python / Java / C / C# / Dart / PHP / Scala / Swift)
/// plus named-child body-block kinds for languages where body is not a grammar field
/// (Kotlin's `function_body` / `class_body`, ObjC method's `compound_statement`).
fn decl_header(decl: TsNode<'_>, src: &[u8]) -> String {
    let body_start: Option<usize> = decl
        .child_by_field_name("body")
        .map(|b| b.start_byte())
        .or_else(|| {
            (0..decl.named_child_count())
                .filter_map(|i| decl.named_child(i as u32))
                .find(|c| {
                    matches!(
                        c.kind(),
                        "block"
                            | "function_body"  // Kotlin
                            | "class_body"     // Kotlin class
                            | "compound_statement" // ObjC method_definition
                            | "code_block"     // Swift
                            | "declaration_list" // C# (fallback)
                            | "template_body" // Scala (fallback)
                    )
                })
                .map(|b| b.start_byte())
        });

    let end = body_start.unwrap_or(decl.end_byte());
    std::str::from_utf8(&src[decl.start_byte()..end])
        .unwrap_or("")
        .trim_end_matches(['{', ':', '=', ' ', '\n', '\r', '\t'])
        .trim()
        .to_string()
}

fn named_child_of_kind<'a>(n: TsNode<'a>, kind: &str) -> Option<TsNode<'a>> {
    for i in 0..n.named_child_count() {
        if let Some(c) = n.named_child(i as u32) {
            if c.kind() == kind {
                return Some(c);
            }
        }
    }
    None
}

/// DFS to find the first descendant (or self) of a given node kind.
/// Used to locate `function_declarator` inside pointer/reference declarators in C/C++.
fn first_descendant_of_kind<'a>(node: TsNode<'a>, kind: &str) -> Option<TsNode<'a>> {
    if node.kind() == kind {
        return Some(node);
    }
    for i in 0..node.named_child_count() {
        if let Some(c) = node.named_child(i as u32) {
            if let Some(found) = first_descendant_of_kind(c, kind) {
                return Some(found);
            }
        }
    }
    None
}

/// Collect parameter entries from a parameter-list node by iterating named children
/// that match the given `param_kinds` slice.
fn collect_formal_params(fp: TsNode<'_>, param_kinds: &[&str], src: &[u8], out: &mut Vec<String>) {
    for i in 0..fp.named_child_count() {
        let Some(p) = fp.named_child(i as u32) else {
            continue;
        };
        if param_kinds.contains(&p.kind()) {
            out.push(node_text(p, src).to_string());
        }
    }
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

/// Variant of `collect_callees_dfs` for call nodes that use a field other than
/// `"function"` for the callee name (e.g. Java `method_invocation` → `name`,
/// Ruby `call` → `method`, ObjC `message_expression` → `method`).
fn collect_callees_field(
    node: TsNode<'_>,
    src: &[u8],
    call_kind: &str,
    callee_field: &str,
    out: &mut Vec<String>,
) {
    if out.len() >= 20 {
        return;
    }
    if node.kind() == call_kind {
        if let Some(fn_node) = node.child_by_field_name(callee_field) {
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
            collect_callees_field(child, src, call_kind, callee_field, out);
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
    fn truly_unsupported_language_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("main.cobol");
        std::fs::write(&src, "IDENTIFICATION DIVISION.").unwrap();
        let node = make_node("main.cobol", "fn:main", "cobol", "function", 1, 1);
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

    // ── Java ──────────────────────────────────────────────────────────────────

    #[test]
    fn java_class_extracts_members() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("Animal.java");
        std::fs::write(
            &src,
            "public class Animal {\n\
                 private String name;\n\
                 public Animal(String name) { this.name = name; }\n\
                 public void makeSound() { System.out.println(name); }\n\
             }\n",
        )
        .unwrap();
        let node = make_node("Animal.java", "class:Animal", "java", "class", 1, 5);
        let skel = skeleton_for_node(&node, dir.path()).unwrap();
        assert!(skel.signature.contains("Animal"), "sig: {}", skel.signature);
        assert!(!skel.fields.is_empty(), "expected class members");
    }

    #[test]
    fn java_method_extracts_params_and_return() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("Billing.java");
        std::fs::write(
            &src,
            "public class Billing {\n\
                 public Receipt charge(double amount, String currency) {\n\
                     return process(amount);\n\
                 }\n\
             }\n",
        )
        .unwrap();
        let node = make_node("Billing.java", "fn:charge", "java", "function", 2, 4);
        let skel = skeleton_for_node(&node, dir.path()).unwrap();
        assert!(skel.signature.contains("charge"), "sig: {}", skel.signature);
        assert!(!skel.params.is_empty(), "expected params");
        assert!(
            skel.return_type.is_some(),
            "expected return type: {:?}",
            skel.return_type
        );
    }

    // ── Kotlin ────────────────────────────────────────────────────────────────

    #[test]
    fn kotlin_function_extracts_params() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("billing.kt");
        std::fs::write(
            &src,
            "fun add(a: Int, b: Int): Int {\n\
                 return a + b\n\
             }\n",
        )
        .unwrap();
        let node = make_node("billing.kt", "fn:add", "kotlin", "function", 1, 3);
        let skel = skeleton_for_node(&node, dir.path()).unwrap();
        assert!(skel.signature.contains("add"), "sig: {}", skel.signature);
        assert!(!skel.params.is_empty(), "expected params for Kotlin fn");
    }

    #[test]
    fn kotlin_class_extracts_members() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("Greeter.kt");
        std::fs::write(
            &src,
            "class Greeter(val name: String) {\n\
                 fun greet(): String {\n\
                     return \"Hello, $name\"\n\
                 }\n\
             }\n",
        )
        .unwrap();
        let node = make_node("Greeter.kt", "class:Greeter", "kotlin", "class", 1, 5);
        let skel = skeleton_for_node(&node, dir.path()).unwrap();
        assert!(
            skel.signature.contains("Greeter"),
            "sig: {}",
            skel.signature
        );
        assert!(!skel.fields.is_empty(), "expected Kotlin class members");
    }

    // ── C ─────────────────────────────────────────────────────────────────────

    #[test]
    fn c_function_extracts_params_and_return() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("math.c");
        std::fs::write(
            &src,
            "int add(int a, int b) {\n\
                 return a + b;\n\
             }\n",
        )
        .unwrap();
        let node = make_node("math.c", "fn:add", "c", "function", 1, 3);
        let skel = skeleton_for_node(&node, dir.path()).unwrap();
        assert!(skel.signature.contains("add"), "sig: {}", skel.signature);
        assert!(!skel.params.is_empty(), "expected C params");
        assert!(skel.return_type.is_some(), "expected return type");
    }

    #[test]
    fn c_struct_extracts_fields() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("point.c");
        std::fs::write(
            &src,
            "struct Point {\n\
                 int x;\n\
                 int y;\n\
             };\n",
        )
        .unwrap();
        let node = make_node("point.c", "class:Point", "c", "struct", 1, 4);
        let skel = skeleton_for_node(&node, dir.path()).unwrap();
        assert!(skel.signature.contains("Point"), "sig: {}", skel.signature);
        assert!(!skel.fields.is_empty(), "expected struct fields");
    }

    // ── C++ ───────────────────────────────────────────────────────────────────

    #[test]
    fn cpp_class_extracts_members() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("animal.cpp");
        std::fs::write(
            &src,
            "class Animal {\n\
             public:\n\
                 std::string name;\n\
                 void speak();\n\
             };\n",
        )
        .unwrap();
        let node = make_node("animal.cpp", "class:Animal", "cpp", "class", 1, 5);
        let skel = skeleton_for_node(&node, dir.path()).unwrap();
        assert!(skel.signature.contains("Animal"), "sig: {}", skel.signature);
        assert!(!skel.fields.is_empty(), "expected C++ class members");
    }

    // ── C# ────────────────────────────────────────────────────────────────────

    #[test]
    fn csharp_method_extracts_params_and_return() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("Billing.cs");
        std::fs::write(
            &src,
            "public class Billing {\n\
                 public Receipt Charge(double amount, string currency) {\n\
                     return Process(amount);\n\
                 }\n\
             }\n",
        )
        .unwrap();
        let node = make_node("Billing.cs", "fn:Charge", "csharp", "function", 2, 4);
        let skel = skeleton_for_node(&node, dir.path()).unwrap();
        assert!(skel.signature.contains("Charge"), "sig: {}", skel.signature);
        assert!(!skel.params.is_empty(), "expected C# params");
    }

    #[test]
    fn csharp_class_extracts_members() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("Animal.cs");
        std::fs::write(
            &src,
            "public class Animal {\n\
                 public string Name { get; set; }\n\
                 public void MakeSound() {}\n\
             }\n",
        )
        .unwrap();
        let node = make_node("Animal.cs", "class:Animal", "csharp", "class", 1, 4);
        let skel = skeleton_for_node(&node, dir.path()).unwrap();
        assert!(skel.signature.contains("Animal"), "sig: {}", skel.signature);
        assert!(!skel.fields.is_empty(), "expected C# class members");
    }

    // ── Swift ─────────────────────────────────────────────────────────────────

    #[test]
    fn swift_function_extracts_params_and_return() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("billing.swift");
        std::fs::write(
            &src,
            "func charge(amount: Double, currency: String) -> Bool {\n\
                 return process(amount)\n\
             }\n",
        )
        .unwrap();
        let node = make_node("billing.swift", "fn:charge", "swift", "function", 1, 3);
        let skel = skeleton_for_node(&node, dir.path()).unwrap();
        assert!(skel.signature.contains("charge"), "sig: {}", skel.signature);
        assert!(skel.return_type.is_some(), "expected Swift return type");
    }

    #[test]
    fn swift_struct_extracts_members() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("animal.swift");
        std::fs::write(
            &src,
            "struct Animal {\n\
                 var name: String\n\
                 func speak() {}\n\
             }\n",
        )
        .unwrap();
        let node = make_node("animal.swift", "class:Animal", "swift", "class", 1, 4);
        let skel = skeleton_for_node(&node, dir.path()).unwrap();
        assert!(skel.signature.contains("Animal"), "sig: {}", skel.signature);
        assert!(!skel.fields.is_empty(), "expected Swift struct members");
    }

    // ── Dart ──────────────────────────────────────────────────────────────────

    #[test]
    fn dart_class_extracts_members() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("animal.dart");
        std::fs::write(
            &src,
            "class Animal {\n\
                 String name;\n\
                 Animal(this.name);\n\
                 void speak() {}\n\
             }\n",
        )
        .unwrap();
        let node = make_node("animal.dart", "class:Animal", "dart", "class", 1, 5);
        let skel = skeleton_for_node(&node, dir.path()).unwrap();
        assert!(skel.signature.contains("Animal"), "sig: {}", skel.signature);
        assert!(!skel.fields.is_empty(), "expected Dart class members");
    }

    // ── Ruby ──────────────────────────────────────────────────────────────────

    #[test]
    fn ruby_method_extracts_params() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("billing.rb");
        std::fs::write(
            &src,
            "def charge(amount, currency)\n\
                 process(amount)\n\
             end\n",
        )
        .unwrap();
        let node = make_node("billing.rb", "fn:charge", "ruby", "function", 1, 3);
        let skel = skeleton_for_node(&node, dir.path()).unwrap();
        assert!(skel.signature.contains("charge"), "sig: {}", skel.signature);
        assert!(!skel.params.is_empty(), "expected Ruby params");
    }

    #[test]
    fn ruby_class_extracts_methods() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("animal.rb");
        std::fs::write(
            &src,
            "class Animal\n\
                 def initialize(name)\n\
                     @name = name\n\
                 end\n\
                 def speak\n\
                     puts @name\n\
                 end\n\
             end\n",
        )
        .unwrap();
        let node = make_node("animal.rb", "class:Animal", "ruby", "class", 1, 9);
        let skel = skeleton_for_node(&node, dir.path()).unwrap();
        assert!(skel.signature.contains("Animal"), "sig: {}", skel.signature);
        assert!(!skel.fields.is_empty(), "expected Ruby class methods");
    }

    // ── Scala ─────────────────────────────────────────────────────────────────

    #[test]
    fn scala_function_extracts_params_and_return() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("billing.scala");
        std::fs::write(
            &src,
            "def charge(amount: Double, currency: String): Boolean = {\n\
                 process(amount)\n\
             }\n",
        )
        .unwrap();
        let node = make_node("billing.scala", "fn:charge", "scala", "function", 1, 3);
        let skel = skeleton_for_node(&node, dir.path()).unwrap();
        assert!(skel.signature.contains("charge"), "sig: {}", skel.signature);
        assert!(!skel.params.is_empty(), "expected Scala params");
        assert!(skel.return_type.is_some(), "expected Scala return type");
    }

    #[test]
    fn scala_class_extracts_members() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("animal.scala");
        std::fs::write(
            &src,
            "class Animal(val name: String) {\n\
                 def speak(): String = name\n\
             }\n",
        )
        .unwrap();
        let node = make_node("animal.scala", "class:Animal", "scala", "class", 1, 3);
        let skel = skeleton_for_node(&node, dir.path()).unwrap();
        assert!(skel.signature.contains("Animal"), "sig: {}", skel.signature);
        assert!(!skel.fields.is_empty(), "expected Scala class members");
    }

    // ── PHP ───────────────────────────────────────────────────────────────────

    #[test]
    fn php_function_extracts_params() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("billing.php");
        std::fs::write(
            &src,
            "<?php\nfunction charge($amount, $currency) {\n    return process($amount);\n}\n",
        )
        .unwrap();
        let node = make_node("billing.php", "fn:charge", "php", "function", 2, 4);
        let skel = skeleton_for_node(&node, dir.path()).unwrap();
        assert!(skel.signature.contains("charge"), "sig: {}", skel.signature);
        assert!(!skel.params.is_empty(), "expected PHP params");
    }

    #[test]
    fn php_class_extracts_members() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("Animal.php");
        std::fs::write(
            &src,
            "<?php\nclass Animal {\n    private string $name;\n    public function speak(): void {}\n}\n",
        )
        .unwrap();
        let node = make_node("Animal.php", "class:Animal", "php", "class", 2, 5);
        let skel = skeleton_for_node(&node, dir.path()).unwrap();
        assert!(skel.signature.contains("Animal"), "sig: {}", skel.signature);
        assert!(!skel.fields.is_empty(), "expected PHP class members");
    }

    // ── Objective-C ───────────────────────────────────────────────────────────

    #[test]
    fn objc_method_has_signature() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("Dog.m");
        std::fs::write(
            &src,
            "@implementation Dog\n\
             - (void)bark {\n\
                 NSLog(@\"Woof!\");\n\
             }\n\
             @end\n",
        )
        .unwrap();
        let node = make_node("Dog.m", "fn:bark", "objectivec", "function", 2, 4);
        let skel = skeleton_for_node(&node, dir.path()).unwrap();
        assert!(skel.signature.contains("bark"), "sig: {}", skel.signature);
    }

    #[test]
    fn objc_class_interface_extracts_methods() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("Dog.h");
        std::fs::write(
            &src,
            "@interface Dog : NSObject\n\
             - (void)bark;\n\
             - (NSString *)name;\n\
             @end\n",
        )
        .unwrap();
        let node = make_node("Dog.h", "class:Dog", "objectivec", "class", 1, 4);
        let skel = skeleton_for_node(&node, dir.path()).unwrap();
        assert!(skel.signature.contains("Dog"), "sig: {}", skel.signature);
        assert!(!skel.fields.is_empty(), "expected ObjC interface methods");
    }

    // ── Go interface ──────────────────────────────────────────────────────────

    #[test]
    fn go_interface_extracts_method_stubs() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo.go");
        std::fs::write(
            &src,
            "package repo\n\
             \n\
             type Repo interface {\n\
             \tFind(id string) (*Item, error)\n\
             \tSave(item *Item) error\n\
             }\n",
        )
        .unwrap();
        let node = make_node("repo.go", "type:Repo", "go", "interface", 3, 6);
        let skel = skeleton_for_node(&node, dir.path()).unwrap();
        assert!(skel.signature.contains("Repo"), "sig: {}", skel.signature);
        assert!(
            !skel.fields.is_empty(),
            "Go interface methods must not be empty"
        );
        let f = skel.fields.join(",");
        assert!(f.contains("Find") || f.contains("Save"), "fields: {f}");
    }

    // ── Swift params + enum ───────────────────────────────────────────────────

    #[test]
    fn swift_function_extracts_params() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("billing.swift");
        std::fs::write(
            &src,
            "func charge(amount: Double, currency: String) -> Bool {\n\
                 return true\n\
             }\n",
        )
        .unwrap();
        let node = make_node("billing.swift", "fn:charge", "swift", "function", 1, 3);
        let skel = skeleton_for_node(&node, dir.path()).unwrap();
        assert!(!skel.params.is_empty(), "Swift function must have params");
        let p = skel.params.join(",");
        assert!(
            p.contains("amount") || p.contains("currency"),
            "params: {p}"
        );
    }

    #[test]
    fn swift_enum_extracts_cases() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("status.swift");
        std::fs::write(
            &src,
            "enum Status {\n\
                 case pending\n\
                 case active\n\
                 case failed\n\
             }\n",
        )
        .unwrap();
        let node = make_node("status.swift", "enum:Status", "swift", "enum", 1, 5);
        let skel = skeleton_for_node(&node, dir.path()).unwrap();
        assert!(!skel.fields.is_empty(), "Swift enum must have cases");
        let f = skel.fields.join(",");
        assert!(f.contains("pending") || f.contains("active"), "cases: {f}");
    }

    // ── Dart method params + annotated member ─────────────────────────────────

    #[test]
    fn dart_method_extracts_params() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("animal.dart");
        std::fs::write(
            &src,
            "class Animal {\n\
                 String speak(String loudness, int times) {\n\
                     return loudness;\n\
                 }\n\
             }\n",
        )
        .unwrap();
        let node = make_node("animal.dart", "fn:speak", "dart", "function", 2, 4);
        let skel = skeleton_for_node(&node, dir.path()).unwrap();
        assert!(!skel.params.is_empty(), "Dart method must have params");
        let p = skel.params.join(",");
        assert!(p.contains("loudness") || p.contains("times"), "params: {p}");
    }

    #[test]
    fn dart_annotated_class_member_extracted() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("animal.dart");
        std::fs::write(
            &src,
            "class Animal {\n\
                 @override\n\
                 void speak() {}\n\
             }\n",
        )
        .unwrap();
        let node = make_node("animal.dart", "class:Animal", "dart", "class", 1, 4);
        let skel = skeleton_for_node(&node, dir.path()).unwrap();
        assert!(
            !skel.fields.is_empty(),
            "annotated Dart member must appear in fields"
        );
        let f = skel.fields.join(",");
        assert!(f.contains("speak"), "fields: {f}");
    }

    // ── ObjC @implementation extracts methods ─────────────────────────────────

    #[test]
    fn objc_implementation_extracts_methods() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("Dog.m");
        std::fs::write(
            &src,
            "@implementation Dog\n\
             - (void)bark {\n\
                 NSLog(@\"Woof!\");\n\
             }\n\
             - (NSString *)name {\n\
                 return _name;\n\
             }\n\
             @end\n",
        )
        .unwrap();
        let node = make_node("Dog.m", "class:Dog", "objectivec", "class", 1, 8);
        let skel = skeleton_for_node(&node, dir.path()).unwrap();
        assert!(
            !skel.fields.is_empty(),
            "@implementation must expose methods"
        );
        let f = skel.fields.join(",");
        assert!(f.contains("bark") || f.contains("name"), "fields: {f}");
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
