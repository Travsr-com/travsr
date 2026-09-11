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
(public_field_definition name: (property_identifier) @field.name)
(property_signature name: (property_identifier) @field.name)
(program (lexical_declaration (variable_declarator) @topvar))
(program (export_statement (lexical_declaration (variable_declarator) @topvar)))
(program (variable_declaration (variable_declarator) @topvar))
(program (export_statement (variable_declaration (variable_declarator) @topvar)))
(import_statement source: (string (string_fragment) @import.source))
(export_statement source: (string (string_fragment) @import.source))
(call_expression
  function: (identifier) @require.fn
  arguments: (arguments . (string (string_fragment) @require.source)))
(call_expression
  arguments: (arguments
    . (string . (string_fragment) @test.entry .)
    . [(arrow_function) (function_expression)])) @test.scope
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
        workspace_dep_markers: vec![],
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
                "parse timed out for {} after {}s, emitting file node only",
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

    // #479: a test *file* (`*.test.ts`, `*.spec.ts`, `__tests__/…`) is one whole
    // `@test.scope`: every declaration in it is `Support`. The path gate is what
    // keeps a production `testHelper` unmarked. #674 adds the precise rule on
    // top (a `describe` is its own scope, an `it`/`test` callback is an
    // `EntryPoint`); this stays as the fallback for every declaration no
    // `describe` encloses.
    let mut test_signals = crate::test_role::TestSignals::default();
    if ts_is_test_path(vname_path) {
        test_signals.push_scope_span(0, tree.root_node().end_position().row);
    }

    // G2: one hop from the name identifier to its declaration node gives the full span.
    // Works for class_declaration, function_declaration, function_signature, method_definition.
    let decl_end_line = |node: tree_sitter::Node<'_>| -> u32 {
        node.parent()
            .map(|p| p.end_position().row as u32 + 1)
            .unwrap_or_else(|| node.start_position().row as u32 + 1)
    };

    while let Some(m) = iter.next() {
        // #610: the `require.*` pattern matches every single-string call, so the
        // callee has to be checked before its argument is treated as an import.
        // The query cannot do it alone — matching an identifier's *text* needs a
        // `#eq?` predicate, and nothing in this crate evaluates predicates, so
        // the check lives here where the whole match is in scope.
        let is_require_call = m.captures.iter().any(|c| {
            capture_names
                .get(c.index as usize)
                .is_some_and(|n| n == "require.fn")
                && c.node.utf8_text(source.as_slice()) == Ok("require")
        });

        // #674: BDD test callbacks. Handled per match rather than per capture,
        // because the name literal and its enclosing call are needed together
        // and because the callee's *text* decides which of the two test signals
        // applies, which this parser has to do in Rust (see `is_require_call`).
        let bdd = capture_node(m, &capture_names, "test.scope")
            .zip(capture_node(m, &capture_names, "test.entry"))
            .and_then(|(call, name_node)| {
                let callee = call.child_by_field_name("function")?;
                let role = bdd_role(callee.utf8_text(source.as_slice()).ok()?)?;
                let name = name_node.utf8_text(source.as_slice()).ok()?.trim();
                if name.is_empty() {
                    return None;
                }
                Some((role, name.to_string(), call, name_node))
            });
        if let Some((role, name, call, name_node)) = bdd {
            let name_row = name_node.start_position().row;
            let chain = bdd_suite_chain(call, source.as_slice());
            let parent_id = if chain.is_empty() {
                file_id
            } else {
                emit::bdd_suite_node(corpus, vname_path, &chain.join(".")).id
            };
            let qualified = if chain.is_empty() {
                name
            } else {
                format!("{}.{name}", chain.join("."))
            };
            let node = match role {
                BddRole::Suite => {
                    // The whole `describe(...)` is a test scope, so every
                    // declaration nested in it is `Support` (#479) even when
                    // nothing in the file's path says "test".
                    test_signals
                        .push_scope_span(call.start_position().row, call.end_position().row);
                    emit::bdd_suite_node(corpus, vname_path, &qualified)
                }
                BddRole::Case => {
                    // One line, the name literal, so only the callback node
                    // itself becomes an `EntryPoint`; helpers around it stay
                    // `Support` (#479 has entry win over scope).
                    test_signals.push_entry_span(name_row, name_row);
                    emit::bdd_test_node(corpus, vname_path, &qualified)
                }
            };
            let node = node
                .with_line(name_row as u32 + 1)
                .with_end_line(call.end_position().row as u32 + 1);
            output.edges.push(emit::defines_edge(parent_id, node.id));
            output.nodes.push(node);
        }

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
                    // Anonymous containers (class expressions, object-literal methods) have
                    // no named class to bind to — parent the method to the file instead of
                    // emitting a class:<anonymous> node that nothing else ever creates.
                    let parent_class = find_parent_class_name(capture.node, source.as_slice());
                    let (class_name, container_id) = match &parent_class {
                        Some(name) => {
                            (name.as_str(), emit::class_node(corpus, vname_path, name).id)
                        }
                        None => ("<anonymous>", file_id),
                    };
                    let node = emit::method_node(corpus, vname_path, class_name, text)
                        .with_line(line)
                        .with_end_line(decl_end_line(capture.node));
                    let edge = emit::defines_edge(container_id, node.id);
                    output.nodes.push(node);
                    output.edges.push(edge);
                }
                "field.name" => {
                    // #757: class fields (`public_field_definition`) and
                    // interface members (`property_signature`) become
                    // owner-qualified `field:Owner.name` nodes contained by their
                    // type. Members of an anonymous class expression or an inline
                    // object type (`type T = { x: number }`) have no named owner
                    // and are dropped rather than minting a file-level field.
                    if let Some((prefix, owner)) = find_parent_type(capture.node, source.as_slice())
                    {
                        let container_id = if prefix == "interface" {
                            emit::interface_node(corpus, vname_path, &owner).id
                        } else {
                            emit::class_node(corpus, vname_path, &owner).id
                        };
                        let node = emit::field_node(corpus, vname_path, &owner, text)
                            .with_line(line)
                            .with_end_line(line);
                        output.edges.push(emit::defines_edge(container_id, node.id));
                        output.nodes.push(node);
                    }
                }
                "topvar" => {
                    // N4a: only top-level (program-child) declarators become
                    // nodes — locals inside function bodies no longer pollute
                    // the graph. A top-level arrow (`const f = () => {}`) or
                    // function expression is a function, not a variable.
                    let Some(name_node) = capture.node.child_by_field_name("name") else {
                        continue;
                    };
                    // Skip destructuring patterns (`const { a, b } = ...`) — not
                    // a single named symbol.
                    if name_node.kind() != "identifier" {
                        continue;
                    }
                    let Ok(name) = name_node.utf8_text(source.as_slice()) else {
                        continue;
                    };
                    let name_line = name_node.start_position().row as u32 + 1;
                    let is_fn = capture
                        .node
                        .child_by_field_name("value")
                        .map(|v| matches!(v.kind(), "arrow_function" | "function_expression"))
                        .unwrap_or(false);
                    let node = if is_fn {
                        emit::fn_node(corpus, vname_path, name)
                            .with_line(name_line)
                            .with_end_line(decl_end_line(capture.node))
                    } else {
                        emit::var_node(corpus, vname_path, name).with_line(name_line)
                    };
                    let edge = emit::defines_edge(file_id, node.id);
                    output.nodes.push(node);
                    output.edges.push(edge);
                }
                "import.source" => {
                    // Import nodes are synthetic, so no definition line.
                    // Captured from `import ... from "x"` and, since the barrel
                    // fix, from `export ... from "x"` too: a re-export carries
                    // exactly the same file dependency, but it is an
                    // `export_statement`, so the import-only pattern never saw
                    // it and a barrel file produced no `depends` edge at all.
                    // That left every importer of a barrel one hop short of the
                    // real definition for blast radius and import reachability.
                    let node = emit::import_node(corpus, vname_path, text);
                    let edge = emit::depends_edge(file_id, node.id);
                    output.nodes.push(node);
                    output.edges.push(edge);
                }
                // #610: CommonJS. `const { X } = require("./animal")` carries
                // the same dependency as an ES import, but it is a call
                // expression, so the `import_statement` pattern never saw it and
                // a `require`-only file produced no import node at all — leaving
                // `get_blast_radius` on the required module with nothing to
                // traverse back through. Emitted identically to the ES form so
                // everything downstream (Depends, ResolvesTo, blast radius) is
                // unchanged.
                "require.source" if is_require_call => {
                    let node = emit::import_node(corpus, vname_path, text);
                    let edge = emit::depends_edge(file_id, node.id);
                    output.nodes.push(node);
                    output.edges.push(edge);
                }
                _ => {}
            }
        } // for &capture in m.captures
    }

    // #479: language-agnostic post-pass sets test_role from the collected signals.
    crate::test_role::apply_test_roles(&test_signals, &mut output.nodes);

    Ok(output)
}

/// #674: what a BDD framework call contributes. `describe(...)` opens a test
/// scope; `it(...)` / `test(...)` is a test entry point.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BddRole {
    Suite,
    Case,
}

/// #674: classify a call by its callee text. A modifier suffix is dropped, so
/// `it.only` / `test.each` / `describe.skip` classify as their base name.
/// Anything else (`fetchAll("x", cb)`, `arr.map(...)`) is not a BDD call and
/// emits nothing.
fn bdd_role(callee: &str) -> Option<BddRole> {
    match callee.split('.').next().unwrap_or(callee) {
        "describe" => Some(BddRole::Suite),
        "it" | "test" => Some(BddRole::Case),
        _ => None,
    }
}

/// #674: the `describe` names enclosing `call`, outermost first.
///
/// A callback's signature is qualified by this chain for the same reason a
/// method is qualified by its class (N1): two suites in one file routinely reuse
/// a case name, and an unqualified `test:works` would collapse them onto one
/// VName. A `describe` whose own name is not a plain literal contributes no link
/// (it has no node to contain anything), so the chain skips it.
fn bdd_suite_chain(call: tree_sitter::Node<'_>, source: &[u8]) -> Vec<String> {
    let mut chain: Vec<String> = Vec::new();
    let mut cur = call.parent();
    while let Some(n) = cur {
        if n.kind() == "call_expression" {
            if let Some(name) = bdd_suite_name(n, source) {
                chain.push(name);
            }
        }
        cur = n.parent();
    }
    chain.reverse();
    chain
}

/// #674: the string-literal name of `call` when it is a `describe(...)` suite.
fn bdd_suite_name(call: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let callee = call.child_by_field_name("function")?;
    if bdd_role(callee.utf8_text(source).ok()?) != Some(BddRole::Suite) {
        return None;
    }
    let literal = call
        .child_by_field_name("arguments")?
        .named_child(0)
        .filter(|n| n.kind() == "string")?;
    if literal.named_child_count() != 1 {
        return None; // escaped or interpolated: not one stable literal
    }
    let name = literal
        .named_child(0)
        .filter(|n| n.kind() == "string_fragment")?
        .utf8_text(source)
        .ok()?
        .trim()
        .to_string();
    (!name.is_empty()).then_some(name)
}

/// The node of the single capture named `want` in `m`, if the match has one.
fn capture_node<'t>(
    m: &tree_sitter::QueryMatch<'_, 't>,
    capture_names: &[String],
    want: &str,
) -> Option<tree_sitter::Node<'t>> {
    m.captures
        .iter()
        .find(|c| {
            capture_names
                .get(c.index as usize)
                .is_some_and(|n| n == want)
        })
        .map(|c| c.node)
}

/// #479: true when a TypeScript/JavaScript file is a test file by path — a
/// `*.test.*` / `*.spec.*` basename, or anything under a `__tests__/` directory
/// (Jest/Vitest conventions). Separator-agnostic so Windows store paths match.
fn ts_is_test_path(vname_path: &str) -> bool {
    let p = vname_path.replace('\\', "/");
    let base = p.rsplit('/').next().unwrap_or(p.as_str());
    base.contains(".test.")
        || base.contains(".spec.")
        || p.contains("/__tests__/")
        || p.starts_with("__tests__/")
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
        current = current.parent()?;
    }
}

/// #757: resolve the enclosing named type for a field/member capture, returning
/// its signature prefix (`"class"` or `"interface"`) and name. Walks up to the
/// first `class`/`interface` declaration; returns `None` for members of an
/// anonymous class expression or an inline object type (`{ x: number }`), which
/// have no named owner to qualify against.
fn find_parent_type(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<(&'static str, String)> {
    let mut current = node.parent()?;
    loop {
        let prefix = match current.kind() {
            "class_declaration" | "class" | "abstract_class_declaration" => Some("class"),
            "interface_declaration" => Some("interface"),
            _ => None,
        };
        if let Some(prefix) = prefix {
            let name = (0..current.child_count())
                .filter_map(|i| current.child(i as u32))
                .find(|child| child.kind() == "type_identifier")
                .and_then(|n| n.utf8_text(source).ok())
                .map(str::to_string)?;
            return Some((prefix, name));
        }
        current = current.parent()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse `src` as `name` and return every `import:` signature it emitted.
    fn imports_of(name: &str, src: &str) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, src).unwrap();
        parse("", &path, name)
            .unwrap()
            .nodes
            .into_iter()
            .filter(|n| n.kind == "import")
            .map(|n| n.vname.signature)
            .collect()
    }

    /// Parse `src` as `name` and return `(signature, kind, test_role)` for
    /// every `test:`/`suite:` node it emitted, in document order.
    fn bdd_nodes(name: &str, src: &str) -> Vec<(String, String, travsr_core::TestRole)> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, src).unwrap();
        parse("", &path, name)
            .unwrap()
            .nodes
            .into_iter()
            .filter(|n| n.kind == "test" || n.kind == "suite")
            .map(|n| (n.vname.signature, n.kind, n.test_role))
            .collect()
    }

    /// #674: a BDD callback gets its own node, named from the string literal and
    /// qualified by the `describe` chain, and #479's evaluator classifies it.
    /// Before this, Phase A emitted no node for a callback body at all, so the
    /// only categorization available was the whole-file path rule.
    #[test]
    fn bdd_callbacks_become_nodes_with_a_test_role() {
        use travsr_core::TestRole::{EntryPoint, Support};
        let got = bdd_nodes(
            "checkout.ts",
            "describe(\"Payments\", () => {\n\
             \x20 it(\"charges the card\", () => {});\n\
             \x20 describe(\"refunds\", () => {\n\
             \x20   test(\"refunds the card\", function () {});\n\
             \x20 });\n\
             });\n",
        );
        assert_eq!(
            got,
            vec![
                ("suite:Payments".to_string(), "suite".to_string(), Support),
                (
                    "test:Payments.charges the card".to_string(),
                    "test".to_string(),
                    EntryPoint
                ),
                (
                    "suite:Payments.refunds".to_string(),
                    "suite".to_string(),
                    Support
                ),
                (
                    "test:Payments.refunds.refunds the card".to_string(),
                    "test".to_string(),
                    EntryPoint
                ),
            ]
        );
    }

    /// #674: a case node is contained by its enclosing suite, not the file, so
    /// the graph carries the `describe` nesting.
    #[test]
    fn bdd_case_is_contained_by_its_suite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.ts");
        std::fs::write(
            &path,
            "describe(\"Payments\", () => {\n  it(\"works\", () => {});\n});\n",
        )
        .unwrap();
        let out = parse("", &path, "c.ts").unwrap();
        let suite = emit::bdd_suite_node("", "c.ts", "Payments").id;
        let case = emit::bdd_test_node("", "c.ts", "Payments.works").id;
        assert!(
            out.edges.iter().any(|e| e.src == suite
                && e.dst == case
                && e.kind == travsr_core::EdgeKind::DefinesBinding),
            "expected suite -> case containment, got {:?}",
            out.edges
        );
    }

    /// #674: a VName signature is identity, so only a name that is stable across
    /// re-runs may become one. A template literal or a computed name is not, so
    /// no node is emitted for it and the callback falls back to the enclosing
    /// scope's `Support` classification.
    #[test]
    fn a_non_literal_test_name_emits_no_node() {
        assert!(bdd_nodes("t.ts", "it(`renders ${n} rows`, () => {});\n").is_empty());
        assert!(bdd_nodes("t2.ts", "it(name, () => {});\n").is_empty());
        assert!(bdd_nodes("t3.ts", "it(\"a \\\"b\\\" c\", () => {});\n").is_empty());
        assert!(bdd_nodes("t4.ts", "it(\"\", () => {});\n").is_empty());
    }

    /// #674: the callee's text is the whole gate. An ordinary call that happens
    /// to take a string and a callback is not a test.
    #[test]
    fn only_bdd_callee_names_become_test_nodes() {
        assert!(bdd_nodes("p.ts", "on(\"click\", () => {});\n").is_empty());
        assert!(bdd_nodes("p2.ts", "items.forEach(\"x\", () => {});\n").is_empty());
        // `it.only` / `test.each` keep the base name.
        assert_eq!(
            bdd_nodes("p3.ts", "it.only(\"works\", () => {});\n")
                .into_iter()
                .map(|(s, _, _)| s)
                .collect::<Vec<_>>(),
            vec!["test:works"]
        );
    }

    /// #674: a callback node lives in its own `test:` namespace, so it can never
    /// shadow a real function definition that shares its name.
    #[test]
    fn a_callback_never_collides_with_a_real_definition() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.ts");
        std::fs::write(
            &path,
            "export function works(): void {}\nit(\"works\", () => {});\n",
        )
        .unwrap();
        let out = parse("", &path, "s.ts").unwrap();
        let sigs: Vec<&str> = out
            .nodes
            .iter()
            .map(|n| n.vname.signature.as_str())
            .collect();
        assert!(sigs.contains(&"fn:works"), "{sigs:?}");
        assert!(sigs.contains(&"test:works"), "{sigs:?}");
        assert_ne!(
            emit::fn_node("", "s.ts", "works").id,
            emit::bdd_test_node("", "s.ts", "works").id
        );
    }

    /// #674 must not regress #479's whole-file path fallback: a declaration in a
    /// `*.test.ts` file that no `describe` encloses is still `Support`.
    #[test]
    fn the_file_path_fallback_still_applies_outside_any_suite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("calibrate.test.ts");
        std::fs::write(&path, "export function setupFixture(): void {}\n").unwrap();
        let out = parse("", &path, "src/calibrate.test.ts").unwrap();
        let helper = out
            .nodes
            .iter()
            .find(|n| n.vname.signature == "fn:setupFixture")
            .expect("helper node");
        assert_eq!(helper.test_role, travsr_core::TestRole::Support);
    }

    /// #610: `require()` is a call expression, so the `import_statement`
    /// pattern never matched it and a CommonJS file emitted no import node at
    /// all — leaving `get_blast_radius` on the required module with nothing to
    /// traverse back through.
    #[test]
    fn commonjs_require_emits_an_import_node() {
        assert_eq!(
            imports_of("jobs.js", "const { Animal } = require(\"./animal\");\n"),
            vec!["import:./animal"]
        );
        // Bare call form, no binding.
        assert_eq!(
            imports_of("side.js", "require(\"./register\");\n"),
            vec!["import:./register"]
        );
        // Package requires are emitted like package ES imports; `link_imports`
        // is what declines to resolve non-relative specifiers.
        assert_eq!(
            imports_of("pkg.js", "const fs = require(\"fs\");\n"),
            vec!["import:fs"]
        );
    }

    /// The query matches any single-string call, so the callee check is what
    /// keeps unrelated calls out of the import set.
    #[test]
    fn only_calls_named_require_become_imports() {
        assert!(
            imports_of("t.js", "describe(\"./animal\", () => {});\n").is_empty(),
            "a test helper taking a string must not register as an import"
        );
        assert!(
            imports_of("t2.js", "console.log(\"./animal\");\n").is_empty(),
            "a member call must not register as an import"
        );
        assert!(
            imports_of("t3.js", "import(\"./animal\");\n").is_empty(),
            "dynamic import() is a different construct and is not handled here"
        );
    }

    /// ES and CommonJS produce the same node shape, so everything downstream
    /// treats them identically.
    #[test]
    fn es_and_commonjs_imports_are_indistinguishable_downstream() {
        assert_eq!(
            imports_of("a.js", "import { Animal } from \"./animal\";\n"),
            imports_of("b.js", "const { Animal } = require(\"./animal\");\n")
        );
    }

    /// A barrel re-export (`export ... from "x"`) carries exactly the same
    /// file dependency as an import, but it is an `export_statement`, so the
    /// import-only pattern never matched it: a barrel emitted no import node,
    /// and every importer of the barrel sat one unresolvable hop away from the
    /// real definition.
    #[test]
    fn re_export_emits_an_import_node() {
        assert_eq!(
            imports_of("index.ts", "export { makeGreeter } from \"./greeter\";\n"),
            vec!["import:./greeter"]
        );
        assert_eq!(
            imports_of("star.ts", "export * from \"./util\";\n"),
            vec!["import:./util"]
        );
        assert_eq!(
            imports_of(
                "dflt.mjs",
                "export { default as scale } from \"./util.mjs\";\n"
            ),
            vec!["import:./util.mjs"]
        );
        // A type-only re-export is the same file dependency.
        assert_eq!(
            imports_of("types.ts", "export type { Greeting } from \"./greeter\";\n"),
            vec!["import:./greeter"]
        );
    }

    /// An `export` with no `from` clause names no other file, so it must not
    /// manufacture an import node.
    #[test]
    fn local_export_emits_no_import_node() {
        assert!(
            imports_of("m.ts", "export const VERSION = \"1\";\n").is_empty(),
            "a local const export is not a dependency"
        );
        assert!(
            imports_of("f.ts", "export function run() {}\n").is_empty(),
            "a local function export is not a dependency"
        );
        assert!(
            imports_of("d.ts", "export default function run() {}\n").is_empty(),
            "a default export is not a dependency"
        );
    }

    /// A re-export must be indistinguishable downstream from a plain import:
    /// same node signature, and the same `depends` edge out of the file node,
    /// which is what `link_imports` later turns into a `resolves-to` hop.
    #[test]
    fn re_export_is_indistinguishable_from_a_plain_import() {
        assert_eq!(
            imports_of("re.ts", "export { A } from \"./a\";\n"),
            imports_of("im.ts", "import { A } from \"./a\";\n")
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("barrel.ts");
        std::fs::write(&path, "export { A } from \"./a\";\n").unwrap();
        let out = parse("", &path, "barrel.ts").unwrap();
        let file_id = out
            .nodes
            .iter()
            .find(|n| n.kind == "file")
            .expect("file node must exist")
            .id;
        let import_id = out
            .nodes
            .iter()
            .find(|n| n.vname.signature == "import:./a")
            .expect("import:./a node must exist")
            .id;
        assert!(
            out.edges.iter().any(|e| e.src == file_id
                && e.dst == import_id
                && e.kind == travsr_core::EdgeKind::Depends),
            "a re-export must emit the same file --depends--> import edge an import does"
        );
    }

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

    #[test]
    fn n4a_top_level_arrow_is_function_locals_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.ts");
        std::fs::write(
            &path,
            "export const handler = () => { const local = 1; return local; };\n\
             const MAX = 42;\n\
             function outer() { const inner = 2; return inner; }\n",
        )
        .unwrap();
        let out = parse("", &path, "m.ts").unwrap();
        let sigs: Vec<(&str, &str)> = out
            .nodes
            .iter()
            .map(|n| (n.kind.as_str(), n.vname.signature.as_str()))
            .collect();
        // Top-level arrow → function, not var.
        assert!(
            sigs.contains(&("function", "fn:handler")),
            "top-level arrow must be a function node; got {sigs:?}"
        );
        // Top-level non-arrow const → variable.
        assert!(
            sigs.contains(&("variable", "var:MAX")),
            "top-level const literal stays a var node; got {sigs:?}"
        );
        // Locals inside function bodies must NOT pollute the graph.
        assert!(
            !sigs
                .iter()
                .any(|(_, s)| *s == "var:local" || *s == "var:inner"),
            "locals must be dropped; got {sigs:?}"
        );
    }

    // #757: class fields (`public_field_definition`) and interface members
    // (`property_signature`) become owner-qualified `field:Owner.name` nodes
    // contained by their type; two owners with a same-named member do not
    // collide.
    #[test]
    fn class_and_interface_fields_are_owner_qualified() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.ts");
        std::fs::write(
            &path,
            "class A {\n  count: number = 0;\n}\n\
             class B {\n  count: number = 0;\n}\n\
             interface I {\n  name: string;\n}\n",
        )
        .unwrap();
        let out = parse("", &path, "m.ts").unwrap();
        let field = |sig: &str| out.nodes.iter().find(|n| n.vname.signature == sig);

        let a = field("field:A.count").expect("field:A.count");
        let b = field("field:B.count").expect("field:B.count");
        assert_eq!(a.kind, "field");
        assert_ne!(
            a.id, b.id,
            "same-named fields on distinct classes must not collide"
        );
        field("field:I.name").expect("interface member field:I.name");

        // Containment: field:A.count parents to class:A, field:I.name to interface:I.
        let class_a = out
            .nodes
            .iter()
            .find(|n| n.vname.signature == "class:A")
            .unwrap()
            .id;
        assert!(
            out.edges.iter().any(|e| e.src == class_a && e.dst == a.id),
            "field:A.count must be contained by class:A"
        );
        let iface = out
            .nodes
            .iter()
            .find(|n| n.vname.signature == "interface:I")
            .unwrap()
            .id;
        let iname = field("field:I.name").unwrap().id;
        assert!(
            out.edges.iter().any(|e| e.src == iface && e.dst == iname),
            "field:I.name must be contained by interface:I"
        );
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

    // L4: a method on an anonymous class expression has no named parent class —
    // it must not emit an orphan `class:<anonymous>` node; its containment edge
    // must parent to the file node instead, like a top-level function.
    #[test]
    fn anonymous_class_expr_method_no_dangling() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("anon_class.ts");
        std::fs::write(&path, "const x = class { write() {} };").unwrap();

        let out = parse("", &path, "anon_class.ts").unwrap();
        assert!(
            !out.nodes
                .iter()
                .any(|n| n.vname.signature == "class:<anonymous>"),
            "must not emit a class:<anonymous> node; got {:?}",
            out.nodes
                .iter()
                .map(|n| n.vname.signature.as_str())
                .collect::<Vec<_>>()
        );
        let file_id = out
            .nodes
            .iter()
            .find(|n| n.kind == "file")
            .expect("file node must exist")
            .id;
        let method_id = out
            .nodes
            .iter()
            .find(|n| n.vname.signature == "method:<anonymous>.write")
            .expect("method:<anonymous>.write node must exist")
            .id;
        assert!(
            out.edges
                .iter()
                .any(|e| e.src == file_id && e.dst == method_id),
            "write method's defines/binding must have src == file node id"
        );
    }

    // L4: same defect for object-literal methods — the parent chain is `object`,
    // never a class, so this must also parent to the file rather than dangle.
    #[test]
    fn object_literal_method_no_dangling() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("anon_obj.ts");
        std::fs::write(&path, "const x = { dispose() {} };").unwrap();

        let out = parse("", &path, "anon_obj.ts").unwrap();
        assert!(
            !out.nodes
                .iter()
                .any(|n| n.vname.signature == "class:<anonymous>"),
            "must not emit a class:<anonymous> node; got {:?}",
            out.nodes
                .iter()
                .map(|n| n.vname.signature.as_str())
                .collect::<Vec<_>>()
        );
        let file_id = out
            .nodes
            .iter()
            .find(|n| n.kind == "file")
            .expect("file node must exist")
            .id;
        let method_id = out
            .nodes
            .iter()
            .find(|n| n.vname.signature == "method:<anonymous>.dispose")
            .expect("method:<anonymous>.dispose node must exist")
            .id;
        assert!(
            out.edges
                .iter()
                .any(|e| e.src == file_id && e.dst == method_id),
            "dispose method's defines/binding must have src == file node id"
        );
    }
}
