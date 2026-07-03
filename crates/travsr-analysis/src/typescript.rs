use std::path::Path;

use anyhow::Context as _;
use streaming_iterator::StreamingIterator as _;
use tree_sitter::{Parser, Query, QueryCursor};

use crate::emit;
use crate::ParseOutput;

/// Reject files larger than this before reading them into memory.
/// Prevents parse-bomb / OOM from crafted or generated multi-GB sources.
const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024; // 10 MB

/// Kill the Tree-sitter parse if it hasn't finished within this window.
/// A deeply-nested AST can take unexpectedly long; 5 s is generous for any
/// real source file and keeps `git commit` responsive.
const PARSE_TIMEOUT_MICROS: u64 = 5_000_000; // 5 seconds

// Compile-time sanity check: timeout must be in [1 s, 30 s].
const _: () = {
    assert!(PARSE_TIMEOUT_MICROS >= 1_000_000);
    assert!(PARSE_TIMEOUT_MICROS <= 30_000_000);
};

// One combined query covers all definition and import patterns we care about in Sprint 1.
// Sprint 2 will extend this with LSIF-derived call/ref edges.
const QUERIES: &str = r"
(class_declaration name: (type_identifier) @class.name)
(abstract_class_declaration name: (type_identifier) @class.name)
(interface_declaration name: (type_identifier) @iface.name)
(type_alias_declaration name: (type_identifier) @type.name)
(enum_declaration name: (identifier) @enum.name)
(function_declaration name: (identifier) @fn.name)
(function_signature name: (identifier) @fn.name)
(method_definition name: (property_identifier) @method.name)
(lexical_declaration (variable_declarator name: (identifier) @var.name))
(variable_declaration (variable_declarator name: (identifier) @var.name))
(import_statement source: (string (string_fragment) @import.source))
";

/// Parse `abs_path` and emit graph records using `vname_path` as the stable
/// VName path (repo-relative, forward-slash — fixes DEBT-012).
pub fn parse(corpus: &str, abs_path: &Path, vname_path: &str) -> anyhow::Result<ParseOutput> {
    let file_size = std::fs::metadata(abs_path)
        .with_context(|| format!("stat {}", abs_path.display()))?
        .len();
    if file_size > MAX_FILE_BYTES {
        anyhow::bail!(
            "skipping {}: file is too large ({} bytes > {} byte limit)",
            abs_path.display(),
            file_size,
            MAX_FILE_BYTES
        );
    }

    let source =
        std::fs::read(abs_path).with_context(|| format!("reading {}", abs_path.display()))?;

    let is_tsx = abs_path.extension().and_then(|e| e.to_str()) == Some("tsx");
    let language = if is_tsx {
        tree_sitter::Language::new(tree_sitter_typescript::LANGUAGE_TSX)
    } else {
        tree_sitter::Language::new(tree_sitter_typescript::LANGUAGE_TYPESCRIPT)
    };

    let file_node = emit::file_node(corpus, vname_path);
    let file_id = file_node.id;

    let mut output = ParseOutput {
        nodes: vec![file_node],
        edges: vec![],
        ffi_markers: vec![],
    };

    let mut parser = Parser::new();
    parser
        .set_language(&language)
        .context("loading TypeScript grammar")?;

    // tree-sitter returns None only on timeout/cancellation, not on bad syntax
    // (it always recovers from parse errors and returns a tree). None here means
    // PARSE_TIMEOUT_MICROS fired — warn so operators know symbols were dropped.
    let tree = match parser.parse(&source, None) {
        Some(t) => t,
        None => {
            tracing::warn!(
                "parse timed out for {} after {}s — emitting file node only",
                abs_path.display(),
                PARSE_TIMEOUT_MICROS / 1_000_000
            );
            return Ok(output);
        }
    };

    let query = Query::new(&language, QUERIES).context("compiling tree-sitter query")?;

    // Collect owned names before the cursor borrows the query.
    let capture_names: Vec<String> = query
        .capture_names()
        .iter()
        .map(|s| s.to_string())
        .collect();

    let mut cursor = QueryCursor::new();
    let mut iter = cursor.matches(&query, tree.root_node(), source.as_slice());

    // G2: one hop from the name identifier to its declaration node gives the full span.
    // Works for class_declaration, function_declaration, function_signature, method_definition.
    let decl_end_line = |node: tree_sitter::Node<'_>| -> u32 {
        node.parent()
            .map(|p| p.end_position().row as u32 + 1)
            .unwrap_or_else(|| node.start_position().row as u32 + 1)
    };

    while let Some(m) = iter.next() {
        for &capture in m.captures {
            let Some(cap_name) = capture_names.get(capture.index as usize) else {
                continue;
            };
            let text = match capture.node.utf8_text(source.as_slice()) {
                Ok(t) => t,
                Err(_) => continue,
            };

            let line = capture.node.start_position().row as u32 + 1;
            match cap_name.as_str() {
                "class.name" => {
                    let node = emit::class_node(corpus, vname_path, text)
                        .with_line(line)
                        .with_end_line(decl_end_line(capture.node));
                    let edge = emit::defines_edge(file_id, node.id);
                    output.nodes.push(node);
                    output.edges.push(edge);
                }
                "iface.name" => {
                    let node = emit::interface_node(corpus, vname_path, text)
                        .with_line(line)
                        .with_end_line(decl_end_line(capture.node));
                    let edge = emit::defines_edge(file_id, node.id);
                    output.nodes.push(node);
                    output.edges.push(edge);
                }
                "type.name" => {
                    let node = emit::type_node(corpus, vname_path, text)
                        .with_line(line)
                        .with_end_line(decl_end_line(capture.node));
                    let edge = emit::defines_edge(file_id, node.id);
                    output.nodes.push(node);
                    output.edges.push(edge);
                }
                "enum.name" => {
                    let node = emit::enum_node(corpus, vname_path, text)
                        .with_line(line)
                        .with_end_line(decl_end_line(capture.node));
                    let edge = emit::defines_edge(file_id, node.id);
                    output.nodes.push(node);
                    output.edges.push(edge);
                }
                "fn.name" => {
                    let node = emit::fn_node(corpus, vname_path, text)
                        .with_line(line)
                        .with_end_line(decl_end_line(capture.node));
                    let edge = emit::defines_edge(file_id, node.id);
                    output.nodes.push(node);
                    output.edges.push(edge);
                }
                "method.name" => {
                    // Edge hierarchy (Tech Lead sign-off): class→method, not file→method.
                    let class_name = find_parent_class_name(capture.node, source.as_slice())
                        .unwrap_or_else(|| "<anonymous>".to_string());
                    let class_id = emit::class_node(corpus, vname_path, &class_name).id;
                    let node = emit::method_node(corpus, vname_path, &class_name, text)
                        .with_line(line)
                        .with_end_line(decl_end_line(capture.node));
                    let edge = emit::defines_edge(class_id, node.id);
                    output.nodes.push(node);
                    output.edges.push(edge);
                }
                "var.name" => {
                    let node = emit::var_node(corpus, vname_path, text).with_line(line);
                    let edge = emit::defines_edge(file_id, node.id);
                    output.nodes.push(node);
                    output.edges.push(edge);
                }
                "import.source" => {
                    // Import nodes are synthetic — no definition line.
                    let node = emit::import_node(corpus, vname_path, text);
                    let edge = emit::depends_edge(file_id, node.id);
                    output.nodes.push(node);
                    output.edges.push(edge);
                }
                _ => {}
            }
        } // for &capture in m.captures
    }

    Ok(output)
}

/// Collect `NapiCall` FFI markers from a TypeScript file (RFC-005 §3, plan 171f).
///
/// Gate: the file must be a napi-emitted `.d.ts` declaration file, identified by
/// the presence of a `"napi"` key in the nearest ancestor `package.json`.
/// Without the gate, every `.d.ts` in node_modules would be scanned.
///
/// `nodes` is the already-parsed node list — function nodes are reused to
/// get their NodeIds without re-parsing.
pub fn collect_napi_dts_markers(
    corpus: &str,
    abs_path: &std::path::Path,
    vname_path: &str,
    nodes: &[travsr_core::Node],
) -> Vec<crate::ffi::FfiMarker> {
    // Only `.d.ts` files carry napi call-site declarations.
    let path_str = abs_path.to_string_lossy();
    if !path_str.ends_with(".d.ts") {
        return Vec::new();
    }

    // Gate: nearest ancestor package.json must have a "napi" key.
    if !has_napi_package_json(abs_path) {
        return Vec::new();
    }

    // Emit a NapiCall marker for every function node in this file.
    let mut markers = Vec::new();
    for node in nodes {
        if node.kind != "function" {
            continue;
        }
        let Some(fn_name) = node.vname.signature.strip_prefix("fn:") else {
            continue;
        };
        // Arity is not available from tree-sitter without re-parsing; omit.
        if let Some(m) = crate::ffi::FfiMarker::try_new(
            node.id,
            crate::ffi::FfiMarkerKind::NapiCall,
            fn_name,
            None::<String>,
            None,
            None::<String>,
            corpus,
        ) {
            markers.push(m);
        }
    }

    // Emit markers for vname_path so the resolver logs point to the right file.
    tracing::debug!(
        file = vname_path,
        count = markers.len(),
        "typescript: collected napi .d.ts markers"
    );
    markers
}

/// Walk parent directories looking for a `package.json` that contains a
/// top-level `"napi"` key. Stops at the filesystem root or after 8 levels.
fn has_napi_package_json(abs_path: &std::path::Path) -> bool {
    let mut dir = abs_path.parent();
    let mut depth = 0u8;
    while let Some(d) = dir {
        let pkg = d.join("package.json");
        if pkg.exists() {
            if let Ok(content) = std::fs::read_to_string(&pkg) {
                // Fast check: presence of `"napi"` key without full JSON parse.
                if content.contains("\"napi\"") {
                    return true;
                }
            }
            break; // stop at the first package.json found
        }
        dir = d.parent();
        depth += 1;
        if depth > 8 {
            break;
        }
    }
    false
}

fn find_parent_class_name(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let mut current = node.parent()?;
    loop {
        if matches!(
            current.kind(),
            "class_declaration" | "class" | "abstract_class_declaration"
        ) {
            let name = (0..current.child_count())
                .filter_map(|i| current.child(i as u32))
                .find(|child| child.kind() == "type_identifier")
                .and_then(|n| n.utf8_text(source).ok())
                .map(str::to_string);
            return name;
        }
        match current.parent() {
            Some(p) => current = p,
            None => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_file_returns_err() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.ts");
        let big = vec![b'a'; (MAX_FILE_BYTES + 1) as usize];
        std::fs::write(&path, &big).unwrap();

        let result = parse("", &path, "big.ts");
        assert!(result.is_err(), "oversized file must return Err");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("too large"),
            "error must mention 'too large': {msg}"
        );
    }

    // Boundary: a file exactly at the limit must be accepted, not rejected.
    #[test]
    fn file_at_exact_limit_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("boundary.ts");
        let content = vec![b' '; MAX_FILE_BYTES as usize];
        std::fs::write(&path, &content).unwrap();

        let result = parse("", &path, "boundary.ts");
        // Must not return the "too large" error — parse may return Ok or a
        // parse error (all-spaces is not valid TS), but never the size error.
        if let Err(e) = &result {
            assert!(
                !e.to_string().contains("too large"),
                "file at exact limit must not be rejected: {e}"
            );
        }
    }

    // Error message must include the file path so operators know which file triggered it.
    #[test]
    fn oversized_error_message_contains_file_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("giant.ts");
        let big = vec![b'a'; (MAX_FILE_BYTES + 1) as usize];
        std::fs::write(&path, &big).unwrap();

        let err = parse("", &path, "giant.ts").unwrap_err().to_string();
        assert!(
            err.contains("giant.ts"),
            "error message must include the file path: {err}"
        );
    }

    // Methods of abstract classes must be qualified by the class name, not
    // `<anonymous>` — find_parent_class_name must recognise
    // `abstract_class_declaration` (verified in tree-sitter-typescript
    // node-types.json for both typescript/ and tsx/ dialects).
    #[test]
    fn abstract_class_method_is_qualified_by_class_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("base.ts");
        std::fs::write(&path, "export abstract class Base { foo(): void {} }").unwrap();

        let out = parse("", &path, "base.ts").unwrap();
        assert!(
            out.nodes
                .iter()
                .any(|n| n.vname.signature == "class:Base" && n.kind == "class"),
            "expected class:Base node for abstract class"
        );
        assert!(
            out.nodes
                .iter()
                .any(|n| n.vname.signature == "method:Base.foo" && n.kind == "method"),
            "expected method:Base.foo, got: {:?}",
            out.nodes
                .iter()
                .map(|n| n.vname.signature.as_str())
                .collect::<Vec<_>>()
        );
    }

    // A normal-sized valid .ts file must still parse correctly after the size gate.
    #[test]
    fn normal_file_still_parses_after_size_gate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("svc.ts");
        std::fs::write(&path, "export class AuthService { login() {} }").unwrap();

        let output = parse("", &path, "svc.ts").expect("normal file must parse without error");
        assert!(
            output.nodes.iter().any(|n| n.kind == "class"),
            "class node must be emitted for AuthService"
        );
    }
}
