// SYNC: keep in sync with crates/travsr-indexer/src/phase_b_rust.rs (TODO: extract to shared crate)
//! Native Rust Phase B — zero external-tool dependencies.
//!
//! Sources of edges:
//!   1. Cargo.toml parsing → `Depends` edges between crate nodes
//!   2. Tree-sitter call-site query → `RefCall` edges between functions
//!
//! Accuracy: name-based resolution without a type system. Correct for direct
//! calls; approximate for trait-dispatched generics. Always better than zero
//! edges (the outcome when rust-analyzer is absent). When rust-analyzer is
//! available the caller merges LSIF output on top for higher fidelity.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use streaming_iterator::StreamingIterator as _;
use travsr_core::{Edge, EdgeKind, Node, ScipRef, UnresolvedCall, VName};
use tree_sitter::{Parser, Query, QueryCursor};

// ── Tree-sitter query ─────────────────────────────────────────────────────────

/// Captures three call-site patterns:
///   `call.fn`     — bare identifier: `foo()`
///   `call.method` — field call: `self.process()` / `obj.method()`
///   `call.scoped` — scoped path: `Type::new()` / `std::mem::swap()`
const CALL_QUERY: &str = "
(call_expression function: (identifier) @call.fn)
(call_expression function: (field_expression field: (field_identifier) @call.method))
(call_expression function: (scoped_identifier name: (identifier) @call.scoped))
";

/// Common names that generate enormous noise (ubiquitous in every Rust crate) and
/// provide no meaningful cross-crate signal. Drop them entirely — no edge, no
/// UnresolvedCall.
const NOISE_NAMES: &[&str] = &[
    "new",
    "from",
    "into",
    "clone",
    "default",
    "fmt",
    "drop",
    "iter",
    "next",
    "unwrap",
    "expect",
    "ok",
    "err",
    "map",
    "and_then",
    "unwrap_or",
    "collect",
    "push",
    "len",
    "is_empty",
];

// ── Public API ────────────────────────────────────────────────────────────────

/// Native Rust Phase B output: `(nodes, edges, unresolved_calls, refs)`.
/// `refs` carries same-file call occurrences (issue #299) for `edge_sites`.
pub type NativePhaseB = (Vec<Node>, Vec<Edge>, Vec<UnresolvedCall>, Vec<ScipRef>);

/// Extract native Phase B edges for a Rust corpus rooted at `root`.
///
/// Returns `(nodes, edges, unresolved_calls, refs)`:
///   - crate nodes + `Depends` edges from Cargo.toml dependency graph
///   - `Depends`/structural edges (same-file call edges are carried as `refs`)
///   - `UnresolvedCall`s for cross-crate calls that cannot be anchored locally
///   - `ScipRef` occurrence records for same-file and struct/enum scoped calls
///     (issue #299): carry the call-site line so the store's
///     `write_scip_attributed_batch` records `edge_sites` rows, giving
///     `find_references` exact `path:line` for Rust.
///
/// Note: travsr-analysis does not have daemon plumbing, so `unresolved_calls`
/// are returned to the caller but are NOT resolved against the store here.
///
/// When `files` is `Some`, the caller supplies pre-walked `(abs_path, vname_path)`
/// pairs from the daemon's Phase A walk (P6 — #329); the extractor uses them
/// directly and skips its own directory walk. Pass `None` to fall back to
/// `collect_source_files`.
pub fn extract_native_phase_b(
    corpus: &str,
    root: &Path,
    files: Option<&[(PathBuf, String)]>,
) -> anyhow::Result<NativePhaseB> {
    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();
    let mut unresolved: Vec<UnresolvedCall> = Vec::new();
    let mut refs: Vec<ScipRef> = Vec::new();

    // Pass 1: crate dependency graph via Cargo.toml
    match extract_cargo_deps(corpus, root) {
        Ok((dep_nodes, dep_edges)) => {
            nodes.extend(dep_nodes);
            edges.extend(dep_edges);
        }
        Err(e) => tracing::debug!(err = %e, "cargo dep extraction skipped"),
    }

    // Pass 2: call-site edges via tree-sitter
    let language = tree_sitter::Language::new(tree_sitter_rust::LANGUAGE);
    let query = match Query::new(&language, CALL_QUERY) {
        Ok(q) => q,
        Err(e) => {
            tracing::warn!(err = %e, "rust call-site query compile failed — skipping Phase B calls");
            return Ok((nodes, edges, unresolved, refs));
        }
    };

    // Use the daemon's pre-walked file list when available (P6 — #329); fall back
    // to a local walk for old daemons and the `travsr index` CLI path.
    let walked;
    let file_pairs: &[(PathBuf, String)] = match files {
        Some(f) => f,
        None => {
            walked = collect_source_files(root, &["rs"]);
            &walked
        }
    };

    for (abs_path, vname_path) in file_pairs {
        match extract_file_call_edges(corpus, abs_path, vname_path, &language, &query) {
            Ok((file_unresolved, file_refs)) => {
                unresolved.extend(file_unresolved);
                refs.extend(file_refs);
            }
            Err(e) => {
                tracing::debug!(err = %e, path = %abs_path.display(), "rust call extraction skipped")
            }
        }
    }

    // Dedup — same pattern as Phase A
    nodes.sort_unstable_by_key(|n| n.id);
    nodes.dedup_by_key(|n| n.id);
    edges.sort_unstable_by_key(|e| (e.src, e.dst));
    edges.dedup_by(|a, b| a.src == b.src && a.dst == b.dst && a.kind == b.kind);
    // Dedup on (src, callee_sig, caller_line) so two distinct call sites of the
    // same callee from one caller both survive — find_references (#299) needs
    // every occurrence line, not just the first.
    unresolved.sort_unstable_by(|a, b| {
        a.src
            .0
            .cmp(&b.src.0)
            .then(a.callee_sig.cmp(&b.callee_sig))
            .then(a.caller_line.cmp(&b.caller_line))
    });
    unresolved.dedup_by(|a, b| {
        a.src == b.src && a.callee_sig == b.callee_sig && a.caller_line == b.caller_line
    });
    refs.sort_unstable_by(|a, b| {
        a.caller_path
            .cmp(&b.caller_path)
            .then(a.caller_line.cmp(&b.caller_line))
            .then(a.callee_id.0.cmp(&b.callee_id.0))
    });
    refs.dedup_by(|a, b| {
        a.caller_path == b.caller_path
            && a.caller_line == b.caller_line
            && a.callee_id == b.callee_id
    });

    Ok((nodes, edges, unresolved, refs))
}

// ── Cargo.toml dependency graph ───────────────────────────────────────────────

fn extract_cargo_deps(corpus: &str, root: &Path) -> anyhow::Result<(Vec<Node>, Vec<Edge>)> {
    let root_cargo = root.join("Cargo.toml");
    if !root_cargo.exists() {
        return Ok((vec![], vec![]));
    }

    let root_text = std::fs::read_to_string(&root_cargo).context("reading root Cargo.toml")?;
    let root_doc: toml::Value = root_text.parse().context("parsing root Cargo.toml")?;

    // Collect all member Cargo.toml paths
    let cargo_paths: Vec<PathBuf> = if let Some(members) = root_doc
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
    {
        members
            .iter()
            .filter_map(|m| m.as_str())
            .flat_map(|member| {
                if member.contains('*') {
                    // Simple glob: enumerate direct children of prefix dir
                    let prefix = member.trim_end_matches("/*").trim_end_matches('*');
                    let base = root.join(prefix);
                    if let Ok(rd) = std::fs::read_dir(base) {
                        rd.flatten()
                            .map(|e| e.path().join("Cargo.toml"))
                            .filter(|p| p.exists())
                            .collect()
                    } else {
                        vec![]
                    }
                } else {
                    let p = root.join(member).join("Cargo.toml");
                    if p.exists() {
                        vec![p]
                    } else {
                        vec![]
                    }
                }
            })
            .collect()
    } else {
        vec![root_cargo]
    };

    // Parse each Cargo.toml: (pkg_name, rel_cargo_path, dep_names)
    let pkg_data: Vec<(String, String, Vec<String>)> = cargo_paths
        .iter()
        .filter_map(|cargo_path| {
            let text = std::fs::read_to_string(cargo_path).ok()?;
            let doc: toml::Value = text.parse().ok()?;
            let pkg_name = doc
                .get("package")
                .and_then(|p| p.get("name"))
                .and_then(|n| n.as_str())?
                .to_string();
            let rel = cargo_path
                .strip_prefix(root)
                .unwrap_or(cargo_path)
                .to_string_lossy()
                .replace('\\', "/");
            let mut dep_names: Vec<String> = Vec::new();
            for section in &["dependencies", "dev-dependencies", "build-dependencies"] {
                if let Some(deps) = doc.get(section).and_then(|d| d.as_table()) {
                    dep_names.extend(deps.keys().cloned());
                }
            }
            Some((pkg_name, rel, dep_names))
        })
        .collect();

    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();

    for (pkg_name, rel_path, dep_names) in &pkg_data {
        let pkg_node = Node::new(
            VName::new(
                corpus,
                "",
                rel_path.as_str(),
                "rust",
                format!("crate:{pkg_name}"),
            ),
            "crate",
        );
        let pkg_id = pkg_node.id;
        nodes.push(pkg_node);

        for dep_name in dep_names {
            let dep_rel = pkg_data
                .iter()
                .find(|(n, _, _)| n == dep_name)
                .map(|(_, p, _)| p.as_str())
                .unwrap_or("");
            let dep_node = Node::new(
                VName::new(corpus, "", dep_rel, "rust", format!("crate:{dep_name}")),
                "crate",
            );
            let dep_id = dep_node.id;
            nodes.push(dep_node);
            edges.push(Edge::new(pkg_id, dep_id, EdgeKind::Depends));
        }
    }

    Ok((nodes, edges))
}

// ── Call-site extraction ──────────────────────────────────────────────────────

fn extract_file_call_edges(
    corpus: &str,
    abs_path: &Path,
    vname_path: &str,
    language: &tree_sitter::Language,
    query: &Query,
) -> anyhow::Result<(Vec<UnresolvedCall>, Vec<ScipRef>)> {
    let source =
        std::fs::read(abs_path).with_context(|| format!("reading {}", abs_path.display()))?;

    let mut parser = Parser::new();
    parser
        .set_language(language)
        .context("loading Rust grammar")?;
    let tree = match parser.parse(&source, None) {
        Some(t) => t,
        None => return Ok((vec![], vec![])),
    };

    let cap_names: Vec<String> = query
        .capture_names()
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut cursor = QueryCursor::new();
    let mut iter = cursor.matches(query, tree.root_node(), source.as_slice());

    let mut unresolved: Vec<UnresolvedCall> = Vec::new();
    // #299 R1: the Rust extractor no longer guesses method-call callee ids
    // (that produced orphaned edge_sites). Method / associated calls are now
    // emitted as UnresolvedCall and resolved against the real node table by the
    // daemon, so this file-local ScipRef vec stays empty and is kept only for
    // the shared return shape.
    let refs: Vec<ScipRef> = Vec::new();

    while let Some(m) = iter.next() {
        for &cap in m.captures {
            let Some(cap_name) = cap_names.get(cap.index as usize) else {
                continue;
            };
            let Ok(callee_name) = cap.node.utf8_text(source.as_slice()) else {
                continue;
            };
            // Skip short names that are likely noise (closures, etc.)
            if callee_name.len() < 2 {
                continue;
            }

            // 1-based call-site line (issue #299). tree-sitter rows are 0-based.
            let occ_line = cap.node.start_position().row.saturating_add(1) as u32;

            let Some((caller_fn, caller_impl)) = find_enclosing_fn(cap.node, source.as_slice())
            else {
                continue;
            };

            let caller_id = match &caller_impl {
                Some(t) => VName::new(
                    corpus,
                    "",
                    vname_path,
                    "rust",
                    format!("fn:{t}.{caller_fn}"),
                )
                .id(),
                None => VName::new(corpus, "", vname_path, "rust", format!("fn:{caller_fn}")).id(),
            };

            match cap_name.as_str() {
                // #299 R1: a method call `recv.method()` targets the receiver's
                // type, which is not knowable syntactically (the enclosing impl
                // is usually NOT the receiver's type — `zoo.add()` inside
                // `Zoo::announce_all`, `a.describe()` where `a: Box<dyn Animal>`).
                // Guessing `fn:{enclosing_impl}.{method}` produced a callee id
                // that matched no node → orphaned edge_sites. Emit an
                // UnresolvedCall keyed on the bare method name and let the daemon
                // resolve it against the real global node table (exact `fn:method`
                // or unique `fn:Type.method` leaf match), which knows every file.
                "call.method" if !NOISE_NAMES.contains(&callee_name) => {
                    unresolved.push(UnresolvedCall {
                        src: caller_id,
                        callee_sig: format!("fn:{callee_name}"),
                        hint_crate: None,
                        caller_line: occ_line,
                    });
                }
                "call.scoped" => {
                    // Extract qualifying segment from the scoped path parent node
                    let qual_raw = cap
                        .node
                        .parent()
                        .and_then(|p| p.child_by_field_name("path"))
                        .and_then(|path_node| path_node.utf8_text(source.as_slice()).ok())
                        .and_then(|t| t.split("::").last().map(str::to_string))
                        .filter(|s| !s.is_empty() && s != callee_name);

                    match qual_raw {
                        Some(ref qual) if qual.starts_with(|c: char| c.is_uppercase()) => {
                            // Uppercase qualifier → associated call `Type::method()`
                            // (e.g. `Zoo::new()`, `Dog::new()`). The type may be defined
                            // in another file, so a path-bound guessed id orphaned.
                            // Emit an UnresolvedCall with the precise qualified sig and
                            // let the daemon match `fn:{Type}.{method}` across all files.
                            if !NOISE_NAMES.contains(&callee_name) {
                                unresolved.push(UnresolvedCall {
                                    src: caller_id,
                                    callee_sig: format!("fn:{qual}.{callee_name}"),
                                    hint_crate: None,
                                    caller_line: occ_line,
                                });
                            }
                        }
                        Some(ref qual) => {
                            // Lowercase qualifier → likely a crate/module path; emit UnresolvedCall
                            if !NOISE_NAMES.contains(&callee_name) {
                                unresolved.push(UnresolvedCall {
                                    src: caller_id,
                                    callee_sig: format!("fn:{callee_name}"),
                                    hint_crate: Some(qual.clone()),
                                    caller_line: occ_line,
                                });
                            }
                        }
                        None => {
                            // No qualifier found — treat as bare call
                            if !NOISE_NAMES.contains(&callee_name) {
                                unresolved.push(UnresolvedCall {
                                    src: caller_id,
                                    callee_sig: format!("fn:{callee_name}"),
                                    hint_crate: None,
                                    caller_line: occ_line,
                                });
                            }
                        }
                    }
                }
                // Bare identifier — cannot anchor callee to a file; emit UnresolvedCall.
                // Note: travsr-analysis does not have daemon plumbing, so UnresolvedCalls
                // are returned to the caller but NOT resolved against the store here.
                "call.fn" if !NOISE_NAMES.contains(&callee_name) => {
                    unresolved.push(UnresolvedCall {
                        src: caller_id,
                        callee_sig: format!("fn:{callee_name}"),
                        hint_crate: None,
                        caller_line: occ_line,
                    });
                }
                _ => {}
            }
        }
    }

    // #299 R1: recover calls that live inside macro invocations
    // (`println!("{}", a.describe())`). tree-sitter keeps a macro's arguments as
    // an unparsed `token_tree`, so the call-expression query above never sees
    // them — yet these are real reference sites. Scan token_trees for the call
    // shape and emit UnresolvedCalls (daemon unique-match resolution guards the
    // false positives an untyped token scan can produce).
    extract_macro_calls(
        tree.root_node(),
        source.as_slice(),
        corpus,
        vname_path,
        &mut unresolved,
    );

    Ok((unresolved, refs))
}

/// Walk the tree and, inside every macro `token_tree`, recover call sites of the
/// form `name ( … )` — a `name` identifier immediately followed by a nested
/// `token_tree`. Classify by the token preceding `name`: `.` → method call,
/// `::` → associated call (with the qualifier), otherwise a bare function call.
/// Each becomes an [`UnresolvedCall`] resolved later against the real node table.
fn extract_macro_calls(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    corpus: &str,
    vname_path: &str,
    out: &mut Vec<UnresolvedCall>,
) {
    if node.kind() == "token_tree" {
        let mut c = node.walk();
        let children: Vec<tree_sitter::Node<'_>> = node.children(&mut c).collect();
        for i in 1..children.len() {
            // A nested token_tree is a call's argument list.
            if children[i].kind() != "token_tree" {
                continue;
            }
            let name_node = children[i - 1];
            if !matches!(name_node.kind(), "identifier" | "field_identifier") {
                continue;
            }
            let Ok(callee_name) = name_node.utf8_text(source) else {
                continue;
            };
            if callee_name.len() < 2 || NOISE_NAMES.contains(&callee_name) {
                continue;
            }
            let occ_line = name_node.start_position().row.saturating_add(1) as u32;
            let Some((caller_fn, caller_impl)) = find_enclosing_fn(name_node, source) else {
                continue;
            };
            let caller_id = match &caller_impl {
                Some(t) => VName::new(
                    corpus,
                    "",
                    vname_path,
                    "rust",
                    format!("fn:{t}.{caller_fn}"),
                )
                .id(),
                None => VName::new(corpus, "", vname_path, "rust", format!("fn:{caller_fn}")).id(),
            };
            // Classify by the token before the name.
            let prev = (i >= 2).then(|| children[i - 2].kind());
            let callee_sig = match prev {
                Some("::") => {
                    // Associated call `Type::method` — recover the qualifier.
                    let qual = (i >= 3)
                        .then(|| children[i - 3])
                        .filter(|n| n.kind() == "identifier")
                        .and_then(|n| n.utf8_text(source).ok());
                    match qual {
                        Some(q) => format!("fn:{q}.{callee_name}"),
                        None => format!("fn:{callee_name}"),
                    }
                }
                // `.` (method) or anything else (bare call) → resolve by name.
                _ => format!("fn:{callee_name}"),
            };
            out.push(UnresolvedCall {
                src: caller_id,
                callee_sig,
                hint_crate: None,
                caller_line: occ_line,
            });
        }
    }

    let mut c = node.walk();
    for child in node.children(&mut c) {
        extract_macro_calls(child, source, corpus, vname_path, out);
    }
}

/// Walk up the tree-sitter AST to find the nearest enclosing `function_item`.
/// Returns `(fn_name, Option<impl_type>)` or `None` when outside any function.
fn find_enclosing_fn(
    node: tree_sitter::Node<'_>,
    source: &[u8],
) -> Option<(String, Option<String>)> {
    let mut cur = node.parent()?;
    loop {
        match cur.kind() {
            "function_item" => {
                let fn_name = cur
                    .child_by_field_name("name")?
                    .utf8_text(source)
                    .ok()?
                    .to_string();
                let impl_type = find_parent_impl_type(cur, source);
                return Some((fn_name, impl_type));
            }
            "source_file" => return None,
            _ => {}
        }
        cur = cur.parent()?;
    }
}

/// Walk up from `node` to find the nearest enclosing `impl_item` type name.
/// Returns the implementing type (e.g. `"Worker"` for `impl Trait for Worker`).
/// Stops at `mod_item` boundaries.
fn find_parent_impl_type(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let mut cur = node.parent()?;
    loop {
        match cur.kind() {
            "impl_item" => {
                let type_node = cur.child_by_field_name("type")?;
                return match type_node.kind() {
                    "type_identifier" => type_node.utf8_text(source).ok().map(str::to_string),
                    "generic_type" => (0..type_node.child_count())
                        .filter_map(|i| type_node.child(i as u32))
                        .find(|c| c.kind() == "type_identifier")
                        .and_then(|c| c.utf8_text(source).ok().map(str::to_string)),
                    _ => None,
                };
            }
            "mod_item" => return None,
            _ => {}
        }
        cur = cur.parent()?;
    }
}

// ── File walker ───────────────────────────────────────────────────────────────

/// Collect (abs_path, repo-relative vname_path) for all source files with
/// the given extensions under `root`. Skips `target/`, `node_modules/`, hidden dirs.
pub(crate) fn collect_source_files(root: &Path, exts: &[&str]) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    walk(root, root, exts, &mut out);
    out
}

fn walk(root: &Path, dir: &Path, exts: &[&str], out: &mut Vec<(PathBuf, String)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            if matches!(name, "target" | "node_modules" | ".git") {
                continue;
            }
            walk(root, &path, exts, out);
        } else if path.is_file() {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if exts.contains(&ext) {
                let vname_path = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((path, vname_path));
            }
        }
    }
}
