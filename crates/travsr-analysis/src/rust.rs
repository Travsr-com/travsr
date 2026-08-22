use std::path::Path;

use anyhow::Context as _;
use streaming_iterator::StreamingIterator as _;
use travsr_core::{Node, VName};
use tree_sitter::{Parser, Query, QueryCursor};

use crate::emit;
use crate::ffi::{FfiMarker, FfiMarkerKind};
use crate::ParseOutput;

/// Reject files larger than this before reading them into memory.
const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024; // 10 MB

/// Kill the Tree-sitter parse if it hasn't finished within this window.
const PARSE_TIMEOUT_MICROS: u64 = 5_000_000; // 5 seconds

const _: () = {
    assert!(PARSE_TIMEOUT_MICROS >= 1_000_000);
    assert!(PARSE_TIMEOUT_MICROS <= 30_000_000);
};

// Captures: fn, struct, enum, trait, impl, mod (inline + file), const, static, use.
// Phase A (Sprint 8): structural definitions only; semantic call/ref edges
// are deferred to Phase B (Sprint 9, rust-analyzer LSIF).
//
// use_declaration is captured as a whole node (@use.decl) and walked
// recursively by extract_use_paths() to handle all use-tree variants:
//   use foo::bar;          → use:foo::bar
//   use foo::{bar, baz};   → use:foo::bar, use:foo::baz
//   use foo::bar as Baz;   → use:foo::bar  (alias ignored for graph identity)
//   use foo::*;            → use:foo::*    (wildcard; ResolvesTo skips these)
//
// mod_item is captured as @mod.name for both inline modules and file-system
// module declarations. The parse() match arm checks for the presence of a
// `body` field at runtime: no body → file-module node; has body → module node.
const QUERIES: &str = "
(function_item name: (identifier) @fn.name)
(struct_item name: (type_identifier) @struct.name)
(enum_item name: (type_identifier) @enum.name)
(trait_item name: (type_identifier) @trait.name)
(impl_item type: (type_identifier) @impl.name)
(impl_item type: (generic_type (type_identifier) @impl.name))
(mod_item name: (identifier) @mod.name)
(const_item name: (identifier) @const.name)
(static_item name: (identifier) @static.name)
(type_item name: (type_identifier) @type.name)
(union_item name: (type_identifier) @union.name)
(field_declaration name: (field_identifier) @field.name)
(use_declaration) @use.decl
";

/// Parse `abs_path` and emit graph records using `vname_path` as the stable
/// VName path (repo-relative, forward-slash).
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

    let language = tree_sitter::Language::new(tree_sitter_rust::LANGUAGE);
    let file_node = rust_file_node(corpus, vname_path);
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
        .context("loading Rust grammar")?;

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

    let query = Query::new(&language, QUERIES).context("compiling Rust tree-sitter query")?;

    let capture_names: Vec<String> = query
        .capture_names()
        .iter()
        .map(|s| s.to_string())
        .collect();

    let mut cursor = QueryCursor::new();
    let mut iter = cursor.matches(&query, tree.root_node(), source.as_slice());

    // #479: collect @test.entry/@test.scope line spans during the walk.
    let mut test_signals = crate::test_role::TestSignals::default();

    // G2: walk up from the name identifier to the enclosing declaration item.
    // `impl.name` may need two hops (identifier → generic_type → impl_item),
    // so we walk up to 3 levels before falling back to the capture's own line.
    let decl_end_line = |node: tree_sitter::Node<'_>| -> u32 {
        let mut cur = node;
        for _ in 0..3 {
            match cur.parent() {
                Some(p) => {
                    if matches!(
                        p.kind(),
                        "function_item"
                            | "struct_item"
                            | "enum_item"
                            | "trait_item"
                            | "impl_item"
                            | "mod_item"
                            | "const_item"
                            | "static_item"
                            | "type_item"
                            | "union_item"
                    ) {
                        return p.end_position().row as u32 + 1;
                    }
                    cur = p;
                }
                None => break,
            }
        }
        node.start_position().row as u32 + 1
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
                "fn.name" => {
                    // Functions inside impl blocks become methods; the parent impl
                    // type is the namespace so signatures are `method:TypeName.method` (N1).
                    let end_line = decl_end_line(capture.node);
                    let parent = find_parent_container(capture.node, source.as_slice());
                    let (node, src_id) = if let Some((container, is_trait)) = parent {
                        // N4c: a default method inside a `trait_item` is a method of
                        // the trait (`method:Trait.name`), parented to the trait node,
                        // not leaked to a file-level `fn:`.
                        let parent_id = if is_trait {
                            rust_trait_node(corpus, vname_path, &container).id
                        } else {
                            rust_impl_node(corpus, vname_path, &container).id
                        };
                        let n = rust_method_node(corpus, vname_path, &container, text)
                            .with_line(line)
                            .with_end_line(end_line);
                        (n, parent_id)
                    } else {
                        let n = rust_fn_node(corpus, vname_path, text)
                            .with_line(line)
                            .with_end_line(end_line);
                        (n, file_id)
                    };
                    output.edges.push(emit::defines_edge(src_id, node.id));
                    output.nodes.push(node);
                    // #479: a `#[test]`/`#[bench]`/`#[*::test]` fn is an entry point.
                    if capture
                        .node
                        .parent()
                        .is_some_and(|f| rust_fn_is_test_entry(f, source.as_slice()))
                    {
                        let r = capture.node.start_position().row;
                        test_signals.push_entry_span(r, r);
                    }
                }
                "struct.name" => {
                    let node = rust_struct_node(corpus, vname_path, text)
                        .with_line(line)
                        .with_end_line(decl_end_line(capture.node));
                    output.edges.push(emit::defines_edge(file_id, node.id));
                    output.nodes.push(node);
                }
                "enum.name" => {
                    let node = rust_enum_node(corpus, vname_path, text)
                        .with_line(line)
                        .with_end_line(decl_end_line(capture.node));
                    output.edges.push(emit::defines_edge(file_id, node.id));
                    output.nodes.push(node);
                }
                "trait.name" => {
                    let node = rust_trait_node(corpus, vname_path, text)
                        .with_line(line)
                        .with_end_line(decl_end_line(capture.node));
                    output.edges.push(emit::defines_edge(file_id, node.id));
                    output.nodes.push(node);
                }
                "impl.name" => {
                    let node = rust_impl_node(corpus, vname_path, text)
                        .with_line(line)
                        .with_end_line(decl_end_line(capture.node));
                    output.edges.push(emit::defines_edge(file_id, node.id));
                    output.nodes.push(node);
                }
                "mod.name" => {
                    // Distinguish `mod foo;` (file declaration, no body) from
                    // `mod foo { … }` (inline module, has body) at the AST level.
                    // capture.node is the `identifier` from `(mod_item name: (identifier))`,
                    // so its parent is always the enclosing `mod_item` node.
                    let has_body = capture
                        .node
                        .parent()
                        .and_then(|p| p.child_by_field_name("body"))
                        .is_some();
                    let node = if has_body {
                        // Inline module — structural container.
                        rust_mod_node(corpus, vname_path, text)
                            .with_line(line)
                            .with_end_line(decl_end_line(capture.node))
                    } else {
                        // File-system module declaration.
                        // link_imports_rust() resolves this to foo.rs / foo/mod.rs.
                        rust_filemod_node(corpus, vname_path, text).with_line(line)
                    };
                    output.edges.push(emit::defines_edge(file_id, node.id));
                    output.nodes.push(node);
                    // #479: `#[cfg(test)] mod { … }` — the whole module is a scope,
                    // so its members (helpers) classify as test support.
                    if let Some(mod_item) = capture.node.parent() {
                        if rust_mod_is_cfg_test(mod_item, source.as_slice()) {
                            test_signals.push_scope_span(
                                mod_item.start_position().row,
                                mod_item.end_position().row,
                            );
                        }
                    }
                }
                "const.name" => {
                    let node = rust_const_node(corpus, vname_path, text)
                        .with_line(line)
                        .with_end_line(decl_end_line(capture.node));
                    output.edges.push(emit::defines_edge(file_id, node.id));
                    output.nodes.push(node);
                }
                "static.name" => {
                    let node = rust_static_node(corpus, vname_path, text)
                        .with_line(line)
                        .with_end_line(decl_end_line(capture.node));
                    output.edges.push(emit::defines_edge(file_id, node.id));
                    output.nodes.push(node);
                }
                "type.name" => {
                    // Associated types (`type Item = X;` inside an impl or trait
                    // body) are projections, not standalone type definitions —
                    // two impls in one file would emit colliding file-level
                    // `type:Item` VNames, and SCIP namespaces them differently
                    // so G1 unification could never match them. Skip entirely.
                    if has_impl_or_trait_ancestor(capture.node) {
                        continue;
                    }
                    let node = rust_type_node(corpus, vname_path, text)
                        .with_line(line)
                        .with_end_line(decl_end_line(capture.node));
                    output.edges.push(emit::defines_edge(file_id, node.id));
                    output.nodes.push(node);
                }
                "union.name" => {
                    let node = rust_union_node(corpus, vname_path, text)
                        .with_line(line)
                        .with_end_line(decl_end_line(capture.node));
                    output.edges.push(emit::defines_edge(file_id, node.id));
                    output.nodes.push(node);
                }
                "field.name" => {
                    // #757: named struct fields become `field:Owner.name` nodes,
                    // owner-qualified (collision-free per Invariant #1) and
                    // contained by their struct — mirroring the Go path
                    // (`field:Struct.name`). Only named `struct` fields are in
                    // scope for v1: `rust_struct_field_owner` returns `None` for
                    // tuple structs (no `field_declaration`), enum-variant fields
                    // (owner would be the variant, not the enum), and union fields.
                    let Some(owner) = rust_struct_field_owner(capture.node, source.as_slice())
                    else {
                        continue;
                    };
                    let struct_id = rust_struct_node(corpus, vname_path, &owner).id;
                    let node = rust_field_node(corpus, vname_path, &owner, text)
                        .with_line(line)
                        .with_end_line(line);
                    output.edges.push(emit::defines_edge(struct_id, node.id));
                    output.nodes.push(node);
                }
                "use.decl" => {
                    // Walk the full use-tree to extract every leaf path, including
                    // grouped imports (`use std::{fmt, io}`) and renames.
                    if let Some(arg) = capture.node.child_by_field_name("argument") {
                        let mut paths = Vec::new();
                        extract_use_paths(arg, "", source.as_slice(), &mut paths);
                        for path in paths {
                            let node = rust_use_node(corpus, vname_path, &path);
                            output.edges.push(emit::depends_edge(file_id, node.id));
                            output.nodes.push(node);
                        }
                    }
                }
                _ => {}
            }
        } // for &capture in m.captures
    }

    // Dedup: a type with both `impl T` and `impl Trait for T` emits the same
    // `impl:T` node twice. Canonicalise in-memory before returning so callers
    // (and the store's put_node) don't perform redundant writes.
    output.nodes.sort_unstable_by_key(|n| n.id);
    output.nodes.dedup_by_key(|n| n.id);
    output.edges.sort_unstable_by_key(|e| (e.src, e.dst));
    output
        .edges
        .dedup_by(|a, b| a.src == b.src && a.dst == b.dst && a.kind == b.kind);

    // #479: language-agnostic post-pass sets test_role from the collected signals.
    crate::test_role::apply_test_roles(&test_signals, &mut output.nodes);

    // Collect FFI markers for cross-language edge resolution (RFC-005).
    let ffi_markers = collect_ffi_markers(corpus, vname_path, &source, &tree);
    output.ffi_markers = ffi_markers;

    Ok(output)
}

/// Returns true if any ancestor of `node` is an `impl_item` or `trait_item`
/// body. Used to skip associated types (`type Item = X;` inside impl/trait
/// blocks) which are projections, not standalone type definitions.
/// Note: `type Item;` without a value parses as `associated_type` (not
/// `type_item`) per tree-sitter-rust node-types.json, so it never reaches
/// the `type.name` capture in the first place.
fn has_impl_or_trait_ancestor(node: tree_sitter::Node<'_>) -> bool {
    let mut current = node.parent();
    while let Some(n) = current {
        if matches!(n.kind(), "impl_item" | "trait_item") {
            return true;
        }
        current = n.parent();
    }
    false
}

/// #757: resolve the owning struct name for a `field_identifier` capture, or
/// `None` when the field is not a named `struct` member (v1 scope). In
/// tree-sitter-rust a named struct field is exactly:
/// `field_identifier → field_declaration → field_declaration_list → struct_item`.
/// Tuple structs use `ordered_field_declaration_list` (no `field_declaration`,
/// so the capture never fires), and enum-variant / union fields resolve to an
/// owner node that is not a `struct_item`, so they are skipped here.
fn rust_struct_field_owner(name_node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let field_decl = name_node.parent()?; // field_declaration
    let list = field_decl.parent()?; // field_declaration_list
    let owner = list.parent()?; // struct_item (or enum_variant / union_item)
    if owner.kind() != "struct_item" {
        return None;
    }
    let name = owner
        .child_by_field_name("name")?
        .utf8_text(source)
        .ok()?
        .trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_owned())
    }
}

/// #479: the `#[…]` attributes attached to `item` — its immediately-preceding
/// `attribute_item` siblings (skipping doc comments between them and the item).
///
/// In tree-sitter-rust an outer attribute is a *sibling* preceding its item, not
/// a child, so this cannot be a nested tree-sitter query pattern; the association
/// is done here in the collector. Each entry is `(path, args)` where `path` is
/// the attribute path text (`"test"`, `"tokio::test"`, `"cfg"`) and `args` is the
/// raw token-tree text if present (`"(test)"`).
fn rust_preceding_attrs(
    item: tree_sitter::Node<'_>,
    source: &[u8],
) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    let mut sib = item.prev_sibling();
    while let Some(s) = sib {
        match s.kind() {
            "attribute_item" => {
                if let Some(attr) = s.named_child(0).filter(|a| a.kind() == "attribute") {
                    let path = attr
                        .named_child(0)
                        .and_then(|p| p.utf8_text(source).ok())
                        .unwrap_or("")
                        .to_string();
                    let args = attr
                        .child_by_field_name("arguments")
                        .and_then(|a| a.utf8_text(source).ok())
                        .map(str::to_string);
                    out.push((path, args));
                }
                sib = s.prev_sibling();
            }
            // Doc comments may sit between the attributes and the item.
            "line_comment" | "block_comment" => sib = s.prev_sibling(),
            _ => break,
        }
    }
    out
}

/// #479: true when `fn_item` carries a test-runner attribute — `#[test]`,
/// `#[bench]`, or any `<path>::test` (`#[tokio::test]`, `#[async_std::test]`, …).
/// Attribute-decisive: a production fn merely *named* `test_*` never matches.
fn rust_fn_is_test_entry(fn_item: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    fn_item.kind() == "function_item"
        && rust_preceding_attrs(fn_item, source)
            .iter()
            .any(|(path, _)| path == "test" || path == "bench" || path.ends_with("::test"))
}

/// #479: true when `mod_item` is gated by `#[cfg(test)]` (also matches
/// `#[cfg(test, …)]`), so its whole body is a test scope. `#[cfg(feature = "test")]`
/// is deliberately not matched — the `test` cfg token must stand alone.
fn rust_mod_is_cfg_test(mod_item: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    mod_item.kind() == "mod_item"
        && rust_preceding_attrs(mod_item, source)
            .iter()
            .any(|(path, args)| {
                path == "cfg"
                    && args.as_deref().is_some_and(|a| {
                        a.trim_start_matches('(')
                            .trim_end_matches(')')
                            .split(',')
                            .any(|t| t.trim() == "test")
                    })
            })
}

/// Walk up the AST from `node` to find the nearest enclosing `impl_item`.
/// Returns the enclosing type container for a function, as `(name, is_trait)`.
///
/// For `impl Processor for Worker` returns `("Worker", false)` — the implementing
/// **type**, not the trait name. For a default method inside `trait Speak`
/// returns `("Speak", true)` (N4c). Stops at `mod_item` — functions inside a
/// module but outside any impl/trait are free functions, not methods.
fn find_parent_container(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<(String, bool)> {
    let mut current = node.parent()?;
    loop {
        match current.kind() {
            "impl_item" => {
                // `type:` field is the implementing type (e.g. `Worker` in
                // `impl Processor for Worker`); `trait:` is the trait name.
                let type_node = current.child_by_field_name("type")?;
                let name = match type_node.kind() {
                    "type_identifier" => type_node.utf8_text(source).ok().map(str::to_string),
                    "generic_type" => {
                        // e.g. `impl<T> Container<T>` — base name is first type_identifier child
                        (0..type_node.child_count())
                            .filter_map(|i| type_node.child(i as u32))
                            .find(|c| c.kind() == "type_identifier")
                            .and_then(|c| c.utf8_text(source).ok().map(str::to_string))
                    }
                    _ => None,
                };
                return name.map(|n| (n, false));
            }
            // N4c: a default method (`function_item` with a body) inside a trait
            // belongs to the trait — `method:Trait.name`, not a file-level `fn:`.
            "trait_item" => {
                let name = current.child_by_field_name("name")?;
                return name.utf8_text(source).ok().map(|n| (n.to_string(), true));
            }
            // Don't cross a module boundary — functions inside a mod are free functions.
            "mod_item" => return None,
            _ => {}
        }
        current = current.parent()?;
    }
}

// ── Use-tree walker ───────────────────────────────────────────────────────────

/// Recursively walk a `use_declaration` argument subtree and collect every
/// leaf path as a `::`-separated string.
///
/// Handles all use-clause variants present in tree-sitter-rust 0.21:
/// - `identifier`         → `foo`
/// - `scoped_identifier`  → `foo::bar`
/// - `use_as_clause`      → `foo::bar` (alias stripped; graph identity = original path)
/// - `use_list`           → recurse into each item with the same prefix
/// - `scoped_use_list`    → extend prefix, recurse into list
/// - `use_wildcard`       → `foo::*` (included; ResolvesTo resolution skips wildcards)
///
/// `self` inside a `use_list` (re-export of the current module) is skipped.
fn extract_use_paths(
    node: tree_sitter::Node<'_>,
    prefix: &str,
    source: &[u8],
    out: &mut Vec<String>,
) {
    match node.kind() {
        "identifier" => {
            let name = match node.utf8_text(source) {
                Ok(t) if t != "self" => t,
                _ => return,
            };
            out.push(if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}::{name}")
            });
        }
        "scoped_identifier" => {
            // utf8_text gives the full path including `::`, e.g. `std::fmt`
            let text = match node.utf8_text(source) {
                Ok(t) => t,
                Err(_) => return,
            };
            out.push(if prefix.is_empty() {
                text.to_owned()
            } else {
                format!("{prefix}::{text}")
            });
        }
        "use_as_clause" => {
            // `use foo::bar as Baz` — index by the original path, not the local alias.
            if let Some(path_node) = node.child_by_field_name("path") {
                extract_use_paths(path_node, prefix, source, out);
            }
        }
        "use_list" => {
            for i in 0..node.child_count() {
                let Some(child) = node.child(i as u32) else {
                    continue;
                };
                match child.kind() {
                    "{" | "}" | "," => {}
                    _ => extract_use_paths(child, prefix, source, out),
                }
            }
        }
        "scoped_use_list" => {
            // `use foo::{bar, baz}` — extend prefix with the path segment,
            // then recurse into the list.
            let path_text = node
                .child_by_field_name("path")
                .and_then(|p| p.utf8_text(source).ok())
                .unwrap_or("");
            let new_prefix = match (prefix.is_empty(), path_text.is_empty()) {
                (true, _) => path_text.to_owned(),
                (_, true) => prefix.to_owned(),
                _ => format!("{prefix}::{path_text}"),
            };
            if let Some(list) = node.child_by_field_name("list") {
                extract_use_paths(list, &new_prefix, source, out);
            }
        }
        "use_wildcard" => {
            // `use foo::*` — capture the full wildcard text so the store
            // records a Depends edge; ResolvesTo resolution skips `::*` paths.
            let text = match node.utf8_text(source) {
                Ok(t) => t,
                Err(_) => return,
            };
            out.push(if prefix.is_empty() {
                text.to_owned()
            } else {
                format!("{prefix}::{text}")
            });
        }
        _ => {}
    }
}

// ── Node constructors ─────────────────────────────────────────────────────────

fn rust_file_node(corpus: &str, path: &str) -> Node {
    Node::new(rust_vname(corpus, path, "file"), "file")
}

fn rust_fn_node(corpus: &str, path: &str, name: &str) -> Node {
    Node::new(rust_vname(corpus, path, &format!("fn:{name}")), "function")
}

fn rust_method_node(corpus: &str, path: &str, impl_type: &str, method: &str) -> Node {
    // N1: canonical `method:Type.name` (was `fn:Type.method`). Free functions
    // stay `fn:name`; see rust_fn_node.
    Node::new(
        rust_vname(corpus, path, &format!("method:{impl_type}.{method}")),
        "method",
    )
}

fn rust_struct_node(corpus: &str, path: &str, name: &str) -> Node {
    Node::new(
        rust_vname(corpus, path, &format!("struct:{name}")),
        "struct",
    )
}

/// #757: a named struct field. Owner-qualified (`field:Owner.name`) so two
/// structs with a same-named field never collide, mirroring the Go path.
fn rust_field_node(corpus: &str, path: &str, owner: &str, field: &str) -> Node {
    Node::new(
        rust_vname(corpus, path, &format!("field:{owner}.{field}")),
        "field",
    )
}

fn rust_enum_node(corpus: &str, path: &str, name: &str) -> Node {
    Node::new(rust_vname(corpus, path, &format!("enum:{name}")), "enum")
}

fn rust_trait_node(corpus: &str, path: &str, name: &str) -> Node {
    Node::new(rust_vname(corpus, path, &format!("trait:{name}")), "trait")
}

fn rust_impl_node(corpus: &str, path: &str, name: &str) -> Node {
    Node::new(rust_vname(corpus, path, &format!("impl:{name}")), "impl")
}

fn rust_mod_node(corpus: &str, path: &str, name: &str) -> Node {
    Node::new(rust_vname(corpus, path, &format!("mod:{name}")), "module")
}

fn rust_filemod_node(corpus: &str, path: &str, name: &str) -> Node {
    Node::new(
        rust_vname(corpus, path, &format!("filemod:{name}")),
        "file-module",
    )
}

fn rust_const_node(corpus: &str, path: &str, name: &str) -> Node {
    Node::new(
        rust_vname(corpus, path, &format!("const:{name}")),
        "constant",
    )
}

fn rust_static_node(corpus: &str, path: &str, name: &str) -> Node {
    Node::new(
        rust_vname(corpus, path, &format!("static:{name}")),
        "static",
    )
}

fn rust_type_node(corpus: &str, path: &str, name: &str) -> Node {
    Node::new(rust_vname(corpus, path, &format!("type:{name}")), "type")
}

fn rust_union_node(corpus: &str, path: &str, name: &str) -> Node {
    // Unions are struct-like aggregates — `struct:` keeps the G1 matcher's
    // class-candidate list closed (SCIP marks them `#`).
    Node::new(rust_vname(corpus, path, &format!("struct:{name}")), "union")
}

fn rust_use_node(corpus: &str, path: &str, use_path: &str) -> Node {
    Node::new(
        rust_vname(corpus, path, &format!("use:{use_path}")),
        "import",
    )
}

fn rust_vname(corpus: &str, path: &str, signature: &str) -> VName {
    VName::new(corpus, "", path, "rust", signature)
}

// ── FFI marker collection (RFC-005) ──────────────────────────────────────────

/// Best-effort demangle of a JNI native-method name (the part of the mangled
/// symbol after the `Java_` prefix, before any overload signature). The
/// package/class/method boundary is genuinely ambiguous without the Java
/// side (all three are `_`-separated), so this keeps the existing
/// last-segment heuristic for that boundary but correctly unescapes JNI's
/// underscore-escaping (`_1` -> `_`, `_2` -> `;`, `_3` -> `[`,
/// `_0XXXX` -> unicode) so an escaped underscore inside the method name is
/// not mistaken for a segment delimiter. Fails closed (`None`) on anything
/// that does not look like an unambiguous bare method name.
fn jni_demangle_method(rest: &str) -> Option<String> {
    // Drop the overload-signature suffix (`__<argsig>`) before demangling.
    let rest = rest.split("__").next().unwrap_or(rest);

    let chars: Vec<char> = rest.chars().collect();
    let mut segments: Vec<String> = vec![String::new()];
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '_' {
            match chars.get(i + 1) {
                Some('1') => {
                    segments.last_mut().unwrap().push('_');
                    i += 2;
                }
                Some('2') => {
                    segments.last_mut().unwrap().push(';');
                    i += 2;
                }
                Some('3') => {
                    segments.last_mut().unwrap().push('[');
                    i += 2;
                }
                Some('0') if chars.len() >= i + 6 => {
                    let hex: String = chars[i + 2..i + 6].iter().collect();
                    let code = u32::from_str_radix(&hex, 16).ok()?;
                    segments.last_mut().unwrap().push(char::from_u32(code)?);
                    i += 6;
                }
                _ => {
                    // A real, unescaped underscore: package/class/method boundary.
                    segments.push(String::new());
                    i += 1;
                }
            }
        } else {
            segments.last_mut().unwrap().push(chars[i]);
            i += 1;
        }
    }

    // Require package + class + method (>= 3 segments) so a bare `Java_foo`
    // is not misdetected as a JNI bridge.
    if segments.len() < 3 {
        return None;
    }
    let method = segments.last().unwrap();
    // `;`/`[` are signature-escape artifacts (`_2`/`_3`) that should never
    // appear in a bare method name; their presence means the split landed
    // somewhere unexpected, so bail rather than emit a junk name.
    if method.is_empty() || method.contains(';') || method.contains('[') {
        return None;
    }
    Some(method.clone())
}

/// Walk the AST looking for #[napi] and #[pyfunction] attribute items on functions,
/// emitting FfiMarker records for the ffi_resolver.
pub fn collect_ffi_markers(
    corpus: &str,
    vname_path: &str,
    source: &[u8],
    tree: &tree_sitter::Tree,
) -> Vec<FfiMarker> {
    let mut markers = Vec::new();
    walk_for_ffi_attrs(corpus, vname_path, source, tree.root_node(), &mut markers);
    markers
}

fn walk_for_ffi_attrs(
    corpus: &str,
    vname_path: &str,
    source: &[u8],
    node: tree_sitter::Node<'_>,
    out: &mut Vec<FfiMarker>,
) {
    if node.kind() == "function_item" {
        // Collect only the attribute_items immediately preceding this function.
        // Reset on any intervening function_item so prior functions' attrs are not inherited.
        let mut attrs: Vec<String> = Vec::new();
        if let Some(parent) = node.parent() {
            let mut running: Vec<String> = Vec::new();
            let mut cursor = parent.walk();
            for child in parent.children(&mut cursor) {
                if child.id() == node.id() {
                    attrs = running;
                    break;
                }
                if child.kind() == "attribute_item" {
                    if let Ok(text) = child.utf8_text(source) {
                        running.push(text.to_string());
                    }
                } else if child.kind() == "function_item" {
                    // Previous function consumed its attrs — start fresh.
                    running.clear();
                }
            }
        }

        let fn_name_node = node.child_by_field_name("name");
        let fn_name = fn_name_node
            .and_then(|n| n.utf8_text(source).ok())
            .unwrap_or("")
            .to_string();
        if fn_name.is_empty() {
            return;
        }

        // Determine fn node_id using the same VName scheme as rust_fn_node.
        let vname =
            travsr_core::VName::new(corpus, "", vname_path, "rust", format!("fn:{fn_name}"));
        let node_id = vname.id();

        // L6: JNI native implementation — identified by the `Java_<pkg>_<Class>_
        // <method>` naming the JNI spec mandates (typically `#[no_mangle] pub
        // extern "system"/"C" fn Java_...`), not an attribute like napi/pyo3.
        // The last `_`-delimited segment is a heuristic, not a real demangle: the
        // package/class/method boundary is ambiguous without the Java side, since
        // all three are `_`-separated. `jni_demangle_method` truncates the overload
        // signature and unescapes `_1`/`_2`/`_3` in the extracted segment, but a
        // method name containing an escaped underscore (`_1`) still can't be told
        // apart from a package/class boundary here, so this remains fail-closed:
        // ambiguous or unescapable input yields `None` rather than a junk name.
        if let Some(java_method) = fn_name.strip_prefix("Java_").and_then(jni_demangle_method) {
            if let Some(m) = FfiMarker::try_new(
                node_id,
                FfiMarkerKind::JniCall,
                fn_name.clone(),
                Some(java_method),
                None,
                None::<String>,
                corpus,
            ) {
                out.push(m);
            }
        }

        for attr in &attrs {
            let attr_clean = attr.trim();

            // #[napi] or #[napi(...)]
            if attr_clean.contains("napi") {
                let bound_name = extract_attr_string_arg(attr_clean, "js_name");
                if let Some(m) = FfiMarker::try_new(
                    node_id,
                    FfiMarkerKind::NapiExport,
                    fn_name.clone(),
                    bound_name,
                    None,
                    None::<String>,
                    corpus,
                ) {
                    out.push(m);
                }
            }
            // #[pyfunction] or #[pyo3(...)]
            else if attr_clean.contains("pyfunction") || attr_clean.contains("pyo3") {
                let bound_name = extract_attr_string_arg(attr_clean, "name");
                if let Some(m) = FfiMarker::try_new(
                    node_id,
                    FfiMarkerKind::PyO3Export,
                    fn_name.clone(),
                    bound_name,
                    None,
                    None::<String>,
                    corpus,
                ) {
                    out.push(m);
                }
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_for_ffi_attrs(corpus, vname_path, source, child, out);
    }
}

/// Extract `key = "value"` from an attribute string like `#[napi(js_name = "greet")]`.
fn extract_attr_string_arg(attr: &str, key: &str) -> Option<String> {
    let needle = format!(r#"{key} = ""#);
    let start = attr.find(&needle)? + needle.len();
    let rest = &attr[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use travsr_core::EdgeKind;

    fn fixture_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust/simple.rs")
    }

    // #757: named struct fields emit owner-qualified `field:Owner.name` nodes
    // contained by their struct, mirroring the Go path.
    #[test]
    fn named_struct_fields_emit_field_nodes_and_containment() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.rs");
        std::fs::write(&path, b"struct S {\n    a: u32,\n    pub b: String,\n}\n").unwrap();
        let out = parse("", &path, "s.rs").unwrap();

        let field = |sig: &str| out.nodes.iter().find(|n| n.vname.signature == sig);
        let a = field("field:S.a").expect("field:S.a node");
        let b = field("field:S.b").expect("field:S.b node");
        assert_eq!(a.kind, "field");
        assert_eq!(b.kind, "field");

        let struct_id = out
            .nodes
            .iter()
            .find(|n| n.vname.signature == "struct:S")
            .expect("struct:S node")
            .id;
        assert!(
            out.edges
                .iter()
                .any(|e| e.src == struct_id && e.dst == a.id && e.kind == EdgeKind::DefinesBinding),
            "expected struct:S → field:S.a containment edge"
        );
    }

    #[test]
    fn tuple_struct_emits_no_field_node() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.rs");
        std::fs::write(&path, b"struct T(u32, String);\n").unwrap();
        let out = parse("", &path, "t.rs").unwrap();
        assert!(
            !out.nodes.iter().any(|n| n.kind == "field"),
            "tuple struct fields have no field_declaration, so no field node"
        );
    }

    #[test]
    fn two_structs_same_field_name_no_collision() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.rs");
        std::fs::write(
            &path,
            b"struct A {\n    n: u8,\n}\nstruct B {\n    n: u8,\n}\n",
        )
        .unwrap();
        let out = parse("", &path, "c.rs").unwrap();
        let a = out
            .nodes
            .iter()
            .find(|n| n.vname.signature == "field:A.n")
            .expect("field:A.n");
        let b = out
            .nodes
            .iter()
            .find(|n| n.vname.signature == "field:B.n")
            .expect("field:B.n");
        assert_ne!(a.id, b.id, "owner-qualified fields must be distinct nodes");
    }

    #[test]
    fn enum_variant_and_union_fields_are_out_of_scope() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("e.rs");
        std::fs::write(
            &path,
            b"enum E {\n    V { x: u32 },\n}\nunion U {\n    f: u32,\n}\n",
        )
        .unwrap();
        let out = parse("", &path, "e.rs").unwrap();
        // v1 scope is named struct fields only; enum-variant and union members
        // resolve to a non-`struct_item` owner and are skipped.
        assert!(
            !out.nodes.iter().any(|n| n.kind == "field"),
            "no field nodes for enum-variant / union members in v1, got {:?}",
            out.nodes
                .iter()
                .filter(|n| n.kind == "field")
                .map(|n| &n.vname.signature)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_role_classifies_rust() {
        use travsr_core::TestRole;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thing.rs");
        std::fs::write(
            &path,
            b"pub fn calibrate() {}\n\
              // production code with a test-ish name, must stay None:\n\
              pub fn test_connection_pool() {}\n\
              #[cfg(test)]\n\
              mod tests {\n\
              use super::*;\n\
              fn helper() {}\n\
              #[test]\n\
              fn calibrate_works() { helper(); }\n\
              #[tokio::test]\n\
              async fn async_calibrate() {}\n\
              }\n",
        )
        .unwrap();
        let out = parse("", &path, "thing.rs").unwrap();
        let role = |sig: &str| {
            out.nodes
                .iter()
                .find(|n| n.vname.signature == sig)
                .unwrap_or_else(|| {
                    panic!(
                        "missing node {sig}, have: {:?}",
                        out.nodes
                            .iter()
                            .map(|n| &n.vname.signature)
                            .collect::<Vec<_>>()
                    )
                })
                .test_role
        };
        // #[test] / #[tokio::test] fns are entry points.
        assert_eq!(role("fn:calibrate_works"), TestRole::EntryPoint);
        assert_eq!(role("fn:async_calibrate"), TestRole::EntryPoint);
        // helper inside #[cfg(test)] mod is support.
        assert_eq!(role("fn:helper"), TestRole::Support);
        // ordinary production code is None.
        assert_eq!(role("fn:calibrate"), TestRole::None);
        // adversarial: production fn with a test-ish name and no attribute → None.
        assert_eq!(role("fn:test_connection_pool"), TestRole::None);
    }

    #[test]
    fn oversized_file_returns_err() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.rs");
        let big = vec![b'a'; (MAX_FILE_BYTES + 1) as usize];
        std::fs::write(&path, &big).unwrap();
        let result = parse("", &path, "big.rs");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("too large"));
    }

    #[test]
    fn malformed_rust_still_emits_file_node() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.rs");
        std::fs::write(&path, b"}{struct(){fn").unwrap();
        let out = parse("", &path, "bad.rs").unwrap();
        assert!(!out.nodes.is_empty());
        assert_eq!(out.nodes[0].kind, "file");
    }

    #[test]
    fn type_alias_and_union_emitted() {
        // RFC-014 #317: SCIP marks `type X = Y` and `union U` as `#` type
        // symbols — Phase A must emit G1-matchable nodes for them.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("types.rs");
        std::fs::write(
            &path,
            b"pub type Result2 = std::result::Result<(), String>;\npub union Bits { i: i32, f: f32 }\n",
        )
        .unwrap();
        let out = parse("", &path, "types.rs").unwrap();
        assert!(
            out.nodes
                .iter()
                .any(|n| n.vname.signature == "type:Result2" && n.kind == "type"),
            "expected type:Result2"
        );
        assert!(
            out.nodes
                .iter()
                .any(|n| n.vname.signature == "struct:Bits" && n.kind == "union"),
            "expected struct:Bits union node"
        );
    }

    #[test]
    fn associated_types_in_impl_blocks_are_not_emitted() {
        // Two impls with `type Item = ...` must not emit colliding file-level
        // `type:Item` nodes; only the standalone `type Alias = u32;` counts.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("assoc.rs");
        std::fs::write(
            &path,
            b"struct A;\nstruct B;\n\
              impl Iterator for A { type Item = u8; fn next(&mut self) -> Option<u8> { None } }\n\
              impl Iterator for B { type Item = u16; fn next(&mut self) -> Option<u16> { None } }\n\
              type Alias = u32;\n",
        )
        .unwrap();
        let out = parse("", &path, "assoc.rs").unwrap();
        let type_nodes: Vec<&str> = out
            .nodes
            .iter()
            .filter(|n| n.kind == "type")
            .map(|n| n.vname.signature.as_str())
            .collect();
        assert_eq!(
            type_nodes,
            vec!["type:Alias"],
            "expected exactly one type node (type:Alias), no type:Item"
        );
    }

    #[test]
    fn associated_type_with_default_in_trait_is_not_emitted() {
        // `type Item = u32;` (with default) inside a trait body parses as
        // `type_item`, not `associated_type` — must also be skipped.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trait_assoc.rs");
        std::fs::write(&path, b"trait T { type Item = u32; }\n").unwrap();
        let out = parse("", &path, "trait_assoc.rs").unwrap();
        assert!(
            !out.nodes.iter().any(|n| n.kind == "type"),
            "trait associated type default must not emit a type node"
        );
    }

    #[test]
    fn n4c_default_trait_method_belongs_to_trait_not_file() {
        // N4c: a default method with a body inside a trait must resolve to
        // `method:Trait.name` parented to the trait node, not leak to a
        // file-level `fn:`. A same-named impl override stays distinct (Invariant #1).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trait_default.rs");
        std::fs::write(
            &path,
            b"trait Speak { fn speak(&self) { println!(\"...\"); } }\n\
              struct Dog;\n\
              impl Speak for Dog { fn speak(&self) { println!(\"woof\"); } }\n",
        )
        .unwrap();
        let out = parse("", &path, "trait_default.rs").unwrap();
        let sigs: Vec<&str> = out
            .nodes
            .iter()
            .map(|n| n.vname.signature.as_str())
            .collect();
        assert!(
            sigs.contains(&"method:Speak.speak"),
            "trait default method must be method:Speak.speak, got {sigs:?}"
        );
        assert!(
            sigs.contains(&"method:Dog.speak"),
            "impl override must be method:Dog.speak, got {sigs:?}"
        );
        assert!(
            !sigs.contains(&"fn:speak"),
            "default trait method must not leak to file-level fn:speak, got {sigs:?}"
        );

        // Containment: trait:Speak → method:Speak.speak (parented to the trait).
        let trait_id = out
            .nodes
            .iter()
            .find(|n| n.vname.signature == "trait:Speak")
            .unwrap()
            .id;
        let method_id = out
            .nodes
            .iter()
            .find(|n| n.vname.signature == "method:Speak.speak")
            .unwrap()
            .id;
        assert!(
            out.edges.iter().any(|e| e.src == trait_id
                && e.dst == method_id
                && e.kind == EdgeKind::DefinesBinding),
            "expected trait:Speak → method:Speak.speak DefinesBinding edge"
        );
    }

    #[test]
    fn parse_file_with_vname_uses_vname_path() {
        let out = parse("", &fixture_path(), "custom/path.rs").unwrap();
        for node in &out.nodes {
            assert_eq!(node.vname.path, "custom/path.rs");
        }
    }

    #[test]
    fn language_field_is_rust() {
        let out = parse("", &fixture_path(), "simple.rs").unwrap();
        for node in &out.nodes {
            assert_eq!(node.vname.language, "rust");
        }
    }

    #[test]
    fn golden_simple_rs_emits_expected_node_kinds() {
        let out = parse("", &fixture_path(), "simple.rs").unwrap();
        let kinds: Vec<&str> = out.nodes.iter().map(|n| n.kind.as_str()).collect();

        assert!(kinds.contains(&"file"), "missing file node");
        assert!(kinds.contains(&"function"), "missing function node");
        assert!(kinds.contains(&"method"), "missing method node");
        assert!(kinds.contains(&"struct"), "missing struct node");
        assert!(kinds.contains(&"enum"), "missing enum node");
        assert!(kinds.contains(&"trait"), "missing trait node");
        assert!(kinds.contains(&"impl"), "missing impl node");
        assert!(kinds.contains(&"module"), "missing module node (inline)");
        assert!(
            kinds.contains(&"file-module"),
            "missing file-module node (mod foo;)"
        );
        assert!(kinds.contains(&"constant"), "missing constant node");
        assert!(kinds.contains(&"static"), "missing static node");
        assert!(kinds.contains(&"import"), "missing import node");
    }

    #[test]
    fn golden_simple_rs_top_level_fn_has_correct_signature() {
        let out = parse("", &fixture_path(), "simple.rs").unwrap();
        assert!(
            out.nodes
                .iter()
                .any(|n| n.vname.signature == "fn:run" && n.kind == "function"),
            "expected fn:run function node"
        );
    }

    #[test]
    fn golden_simple_rs_method_has_impl_qualified_signature() {
        let out = parse("", &fixture_path(), "simple.rs").unwrap();
        assert!(
            out.nodes
                .iter()
                .any(|n| n.vname.signature == "method:Worker.new" && n.kind == "method"),
            "expected method:Worker.new method node"
        );
    }

    #[test]
    fn golden_simple_rs_impl_for_trait_method_is_attributed_to_type() {
        let out = parse("", &fixture_path(), "simple.rs").unwrap();
        assert!(
            out.nodes
                .iter()
                .any(|n| n.vname.signature == "method:Worker.process" && n.kind == "method"),
            "expected method:Worker.process, impl Trait for Type must use the implementing type"
        );
    }

    #[test]
    fn golden_simple_rs_defines_binding_edges() {
        let out = parse("", &fixture_path(), "simple.rs").unwrap();
        let file_id = out.nodes.iter().find(|n| n.kind == "file").unwrap().id;

        // file → struct:Config
        let struct_id = out
            .nodes
            .iter()
            .find(|n| n.vname.signature == "struct:Config")
            .unwrap()
            .id;
        assert!(
            out.edges.iter().any(|e| e.src == file_id
                && e.dst == struct_id
                && e.kind == EdgeKind::DefinesBinding),
            "expected file → struct:Config DefinesBinding edge"
        );

        // impl:Worker → method:Worker.new
        let impl_id = out
            .nodes
            .iter()
            .find(|n| n.vname.signature == "impl:Worker")
            .unwrap()
            .id;
        let method_id = out
            .nodes
            .iter()
            .find(|n| n.vname.signature == "method:Worker.new")
            .unwrap()
            .id;
        assert!(
            out.edges.iter().any(|e| e.src == impl_id
                && e.dst == method_id
                && e.kind == EdgeKind::DefinesBinding),
            "expected impl:Worker → method:Worker.new DefinesBinding edge"
        );
    }

    #[test]
    fn golden_simple_rs_use_has_depends_edge() {
        let out = parse("", &fixture_path(), "simple.rs").unwrap();
        let file_id = out.nodes.iter().find(|n| n.kind == "file").unwrap().id;
        let use_node = out.nodes.iter().find(|n| n.kind == "import").unwrap();
        assert!(
            out.edges
                .iter()
                .any(|e| e.src == file_id && e.dst == use_node.id && e.kind == EdgeKind::Depends),
            "expected file → use Depends edge"
        );
    }

    #[test]
    fn grouped_use_emits_individual_import_nodes() {
        // simple.rs has `use std::{collections::HashMap, io}` — both paths
        // must produce separate import nodes.
        let out = parse("", &fixture_path(), "simple.rs").unwrap();
        assert!(
            out.nodes
                .iter()
                .any(|n| n.vname.signature == "use:std::collections::HashMap"),
            "expected use:std::collections::HashMap from grouped import"
        );
        assert!(
            out.nodes.iter().any(|n| n.vname.signature == "use:std::io"),
            "expected use:std::io from grouped import"
        );
    }

    #[test]
    fn file_module_declaration_emits_file_module_node() {
        // simple.rs has `mod helpers;` (no body) — must emit kind "file-module".
        let out = parse("", &fixture_path(), "simple.rs").unwrap();
        assert!(
            out.nodes
                .iter()
                .any(|n| n.kind == "file-module" && n.vname.signature == "filemod:helpers"),
            "expected file-module node for `mod helpers;`"
        );
    }

    #[test]
    fn inline_module_still_emits_module_kind() {
        // `pub mod utils { … }` has a body — must still emit kind "module".
        let out = parse("", &fixture_path(), "simple.rs").unwrap();
        assert!(
            out.nodes
                .iter()
                .any(|n| n.kind == "module" && n.vname.signature == "mod:utils"),
            "expected module node for inline `pub mod utils {{ … }}`"
        );
    }

    #[test]
    fn impl_nodes_are_not_duplicated_for_bare_and_trait_impls() {
        // `simple.rs` has `impl Worker { ... }` AND `impl Processor for Worker`.
        // Both fire `impl.name` → `impl:Worker`. After dedup, exactly one node.
        let out = parse("", &fixture_path(), "simple.rs").unwrap();
        let count = out
            .nodes
            .iter()
            .filter(|n| n.vname.signature == "impl:Worker")
            .count();
        assert_eq!(count, 1, "impl:Worker must appear exactly once after dedup");
    }

    #[test]
    fn signature_prefix_is_rust_not_typescript() {
        let out = parse("", &fixture_path(), "simple.rs").unwrap();
        let fn_node = out
            .nodes
            .iter()
            .find(|n| n.kind == "function")
            .expect("expected at least one function node");
        assert!(
            fn_node.vname.signature.starts_with("fn:"),
            "Rust function signature must start with fn:"
        );
        assert_eq!(fn_node.vname.language, "rust");
    }

    // L6: a JNI native implementation function (`Java_<pkg>_<Class>_<method>`)
    // must emit a JniCall marker whose effective name is the bare Java method
    // name, so it string-matches a JniExport's local_name in the resolver.
    #[test]
    fn jni_native_impl_emits_jnicall() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jni_bridge.rs");
        std::fs::write(
            &path,
            br#"#[no_mangle]
pub extern "system" fn Java_com_example_Foo_bar() {}
"#,
        )
        .unwrap();
        let out = parse("", &path, "jni_bridge.rs").unwrap();
        let marker = out
            .ffi_markers
            .iter()
            .find(|m| m.kind == FfiMarkerKind::JniCall)
            .expect("expected a JniCall marker");
        assert_eq!(
            marker.effective_name(),
            "bar",
            "effective_name must be the bare Java method name (after the last _)"
        );
    }

    // A plain Rust function whose name merely contains underscores must not be
    // misdetected as a JNI bridge.
    #[test]
    fn non_jni_function_emits_no_jnicall() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plain.rs");
        std::fs::write(&path, b"pub fn do_the_thing() {}\n").unwrap();
        let out = parse("", &path, "plain.rs").unwrap();
        assert!(
            !out.ffi_markers
                .iter()
                .any(|m| m.kind == FfiMarkerKind::JniCall),
            "a non-Java_-prefixed function must not emit a JniCall marker"
        );
    }

    // Overloaded native: JNI appends `__<argsig>` for overload resolution.
    // The demangler must drop the signature suffix, not treat it as part of
    // the method name.
    #[test]
    fn jni_overloaded_native_emits_bare_method_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jni_overload.rs");
        std::fs::write(
            &path,
            br#"#[no_mangle]
pub extern "system" fn Java_com_example_Foo_bar__ILjava_lang_String_2() {}
"#,
        )
        .unwrap();
        let out = parse("", &path, "jni_overload.rs").unwrap();
        let marker = out
            .ffi_markers
            .iter()
            .find(|m| m.kind == FfiMarkerKind::JniCall)
            .expect("expected a JniCall marker");
        assert_eq!(
            marker.effective_name(),
            "bar",
            "overload signature suffix must be dropped from the method name"
        );
    }

    // Underscored method: JNI escapes a literal `_` in an identifier as `_1`.
    // The demangler must unescape it back to a real underscore rather than
    // treating it as a segment boundary.
    #[test]
    fn jni_underscored_method_name_is_unescaped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jni_underscored.rs");
        std::fs::write(
            &path,
            br#"#[no_mangle]
pub extern "system" fn Java_com_example_Foo_do_1work() {}
"#,
        )
        .unwrap();
        let out = parse("", &path, "jni_underscored.rs").unwrap();
        let marker = out
            .ffi_markers
            .iter()
            .find(|m| m.kind == FfiMarkerKind::JniCall)
            .expect("expected a JniCall marker");
        assert_eq!(
            marker.effective_name(),
            "do_work",
            "an escaped underscore (_1) in the method name must be unescaped, not treated as a boundary"
        );
    }

    // A bare `Java_foo` (no package/class segments) must not emit a marker:
    // without at least three `_`-separated segments there is no method
    // boundary to anchor on, so this stays fail-closed.
    #[test]
    fn jni_bare_java_prefix_emits_no_jnicall() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("java_foo.rs");
        std::fs::write(&path, b"pub fn Java_foo() {}\n").unwrap();
        let out = parse("", &path, "java_foo.rs").unwrap();
        assert!(
            !out.ffi_markers
                .iter()
                .any(|m| m.kind == FfiMarkerKind::JniCall),
            "Java_foo has no package/class/method segments and must not emit a JniCall marker"
        );
    }
}
