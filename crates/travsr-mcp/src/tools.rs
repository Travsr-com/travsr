use std::collections::HashMap;
use std::path::PathBuf;

use travsr_store::{SqliteStore, Store};

use crate::sanitize::{sanitize_for_mcp, validate_mcp_arg};

/// Return the import targets (Depends edges) of the given file path.
/// Empty string when nothing is found — callers must NOT return an RPC error
/// for the no-results case (Tech Lead requirement).
pub fn get_dependencies(store: &SqliteStore, file: &str) -> String {
    // SEC-002: reject path traversal / absolute paths / oversized args.
    if let Err(reason) = validate_mcp_arg(file) {
        tracing::warn!("get_dependencies rejected invalid arg: {reason}");
        return String::new();
    }
    // SEC-001: sanitize raw result before returning to MCP client / LLM.
    sanitize_for_mcp(&get_dependencies_raw(store, file))
}

/// Raw (unsanitized) variant used by global aggregation — sanitization happens
/// once at the aggregation point, not per-store.
fn get_dependencies_raw(store: &SqliteStore, file: &str) -> String {
    let nodes = match store.search_nodes_by_name(file) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("get_dependencies search error: {e}");
            return String::new();
        }
    };

    // Prefer a node whose kind is "file"; fall back to first match.
    let seed = nodes
        .iter()
        .find(|n| n.kind == "file")
        .or_else(|| nodes.first());

    let seed = match seed {
        Some(n) => n,
        None => return String::new(),
    };

    let edges = match store.iter_edges_from(seed.id) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("get_dependencies edge query error: {e}");
            return String::new();
        }
    };

    let mut lines: Vec<String> = Vec::new();
    for edge in edges.iter().filter(|e| e.kind.as_str() == "depends") {
        if let Ok(Some(dst_node)) = store.get_node(edge.dst) {
            lines.push(dst_node.vname.signature.clone());
        }
    }
    lines.join("\n")
}

/// Return all nodes that have an incoming edge to the given symbol, tagged
/// by provenance so both semantic and structural callers are visible.
///
/// Output format:
///   `[call] fn:bar (function) — src/bar.ts`   ← LSIF RefCall (true call site)
///   `[structural] class:Foo (class) — src/foo.ts` ← Tree-sitter DefinesBinding
///
/// Both sets are always returned when present. Tagging allows the LLM to
/// distinguish a true caller from a structural parent (e.g. class→method),
/// and avoids the all-or-nothing footgun where one RefCall would hide all
/// DefinesBinding callers. The precedence policy in #47 will refine this further.
///
/// Empty string when nothing is found.
pub fn get_callers(store: &SqliteStore, symbol: &str) -> String {
    // SEC-002: validate before forwarding to store queries.
    if let Err(reason) = validate_mcp_arg(symbol) {
        tracing::warn!("get_callers rejected invalid arg: {reason}");
        return String::new();
    }
    // SEC-001: sanitize raw result before returning to MCP client / LLM.
    sanitize_for_mcp(&get_callers_raw(store, symbol))
}

/// Raw (unsanitized) variant used by global aggregation.
fn get_callers_raw(store: &SqliteStore, symbol: &str) -> String {
    use travsr_core::EdgeKind;

    let nodes = match store.search_nodes_by_name(symbol) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("get_callers search error: {e}");
            return String::new();
        }
    };

    let seed = match nodes.first() {
        Some(n) => n,
        None => return String::new(),
    };

    let edges = match store.iter_edges_to(seed.id) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("get_callers edge query error: {e}");
            return String::new();
        }
    };

    let mut lines: Vec<String> = Vec::new();

    for edge in &edges {
        let tag = match edge.kind {
            EdgeKind::RefCall => "[call]",
            EdgeKind::DefinesBinding => "[structural]",
            _ => continue,
        };
        if let Ok(Some(src_node)) = store.get_node(edge.src) {
            lines.push(format!(
                "{tag} {} ({}) — {}",
                src_node.vname.signature, src_node.kind, src_node.vname.path
            ));
        }
    }

    lines.join("\n")
}

/// Global variant of `get_dependencies` — searches one named repo or all registered repos.
///
/// When `repo` is `Some`, only that repo's db is queried. When `None`, all
/// registered repos are searched and results are prefixed with `[repo-name]`.
/// Stale registry entries (db file deleted) are skipped silently.
pub fn get_dependencies_global(
    repos: &HashMap<String, PathBuf>,
    file: &str,
    repo: Option<&str>,
) -> String {
    // SEC-002: validate before registry + store queries.
    if let Err(reason) = validate_mcp_arg(file) {
        tracing::warn!("get_dependencies_global rejected invalid arg: {reason}");
        return String::new();
    }
    // Aggregate raw results, prefix per-repo, then sanitize once.
    let raw = collect_global(repos, repo, |store, repo_name, single| {
        let result = get_dependencies_raw(store, file);
        if result.is_empty() || single {
            result
        } else {
            result
                .lines()
                .map(|l| format!("[{repo_name}] {l}"))
                .collect::<Vec<_>>()
                .join("\n")
        }
    });
    // SEC-001: sanitize the fully-aggregated string once.
    sanitize_for_mcp(&raw)
}

/// Global variant of `get_callers` — searches one named repo or all registered repos.
pub fn get_callers_global(
    repos: &HashMap<String, PathBuf>,
    symbol: &str,
    repo: Option<&str>,
) -> String {
    // SEC-002: validate before registry + store queries.
    if let Err(reason) = validate_mcp_arg(symbol) {
        tracing::warn!("get_callers_global rejected invalid arg: {reason}");
        return String::new();
    }
    let raw = collect_global(repos, repo, |store, repo_name, single| {
        let result = get_callers_raw(store, symbol);
        if result.is_empty() || single {
            result
        } else {
            result
                .lines()
                .map(|l| format!("[{repo_name}] {l}"))
                .collect::<Vec<_>>()
                .join("\n")
        }
    });
    // SEC-001: sanitize the fully-aggregated string once.
    sanitize_for_mcp(&raw)
}

fn collect_global(
    repos: &HashMap<String, PathBuf>,
    target_repo: Option<&str>,
    mut f: impl FnMut(&SqliteStore, &str, bool) -> String,
) -> String {
    // SEC-002: validate repo arg before registry lookup.
    if let Some(name) = target_repo {
        if let Err(reason) = validate_mcp_arg(name) {
            tracing::warn!("collect_global rejected invalid repo arg: {reason}");
            return String::new();
        }
    }

    let candidates: Vec<(&str, &PathBuf)> = match target_repo {
        Some(name) => match repos.get_key_value(name) {
            Some((k, v)) => vec![(k.as_str(), v)],
            None => {
                tracing::warn!("repo '{name}' not found in registry");
                return String::new();
            }
        },
        None => repos.iter().map(|(k, v)| (k.as_str(), v)).collect(),
    };

    let single = candidates.len() == 1;
    let mut parts: Vec<String> = Vec::new();

    for (repo_name, db_path) in candidates {
        if !db_path.exists() {
            tracing::debug!("skipping stale registry entry: {}", db_path.display());
            continue;
        }
        match SqliteStore::open(db_path) {
            Ok(store) => {
                let result = f(&store, repo_name, single);
                if !result.is_empty() {
                    parts.push(result);
                }
            }
            Err(e) => tracing::warn!("failed to open {}: {e}", db_path.display()),
        }
    }

    parts.join("\n")
}

// ── get_blast_radius ──────────────────────────────────────────────────────────

/// Return the set of files transitively affected if the given file changes.
///
/// Uses reverse BFS over `DefinesBinding` and `RefCall` edges: starting from
/// every node defined in the file, follows incoming edges to find everything
/// that references or calls those definitions.
///
/// Output format (one line per affected file, sorted):
///   `src/service.ts`
///   `src/controller.ts`
pub fn get_blast_radius(store: &SqliteStore, file: &str) -> String {
    if let Err(reason) = validate_mcp_arg(file) {
        tracing::warn!("get_blast_radius rejected invalid arg: {reason}");
        return String::new();
    }
    sanitize_for_mcp(&get_blast_radius_raw(store, file))
}

fn get_blast_radius_raw(store: &SqliteStore, file: &str) -> String {
    use std::collections::{HashSet, VecDeque};
    use travsr_core::EdgeKind;

    // Hard ceiling: prevents OOM on utility files imported by thousands of callers.
    // Same guard policy as PPR (MAX_SUBGRAPH_NODES = 250_000); tighter here because
    // blast-radius is a UI tool whose output must fit in an MCP response.
    const MAX_BLAST_RADIUS_NODES: usize = 50_000;

    // Find all nodes whose VName path matches the given file.
    let seed_nodes = match store.search_nodes_by_name(file) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("get_blast_radius search error: {e}");
            return String::new();
        }
    };

    if seed_nodes.is_empty() {
        return String::new();
    }

    let mut visited: HashSet<travsr_core::NodeId> = HashSet::new();
    let mut queue: VecDeque<travsr_core::NodeId> = VecDeque::new();
    let mut affected_files: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for node in &seed_nodes {
        if visited.insert(node.id) {
            queue.push_back(node.id);
        }
        // The file itself counts as affected.
        if !node.vname.path.is_empty() {
            affected_files.insert(node.vname.path.clone());
        }
    }

    while let Some(current_id) = queue.pop_front() {
        let incoming = match store.iter_edges_to(current_id) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("get_blast_radius edge error: {e}");
                continue;
            }
        };

        if visited.len() >= MAX_BLAST_RADIUS_NODES {
            tracing::warn!(
                "get_blast_radius hit ceiling ({MAX_BLAST_RADIUS_NODES} nodes) \
                 for file '{file}'; result may be incomplete"
            );
            break;
        }

        for edge in incoming {
            if !matches!(edge.kind, EdgeKind::DefinesBinding | EdgeKind::RefCall) {
                continue;
            }
            if visited.insert(edge.src) {
                queue.push_back(edge.src);
                if let Ok(Some(src_node)) = store.get_node(edge.src) {
                    if !src_node.vname.path.is_empty() {
                        affected_files.insert(src_node.vname.path.clone());
                    }
                }
            }
        }
    }

    affected_files.into_iter().collect::<Vec<_>>().join("\n")
}

/// Global variant of `get_blast_radius`.
pub fn get_blast_radius_global(
    repos: &HashMap<String, PathBuf>,
    file: &str,
    repo: Option<&str>,
) -> String {
    if let Err(reason) = validate_mcp_arg(file) {
        tracing::warn!("get_blast_radius_global rejected invalid arg: {reason}");
        return String::new();
    }
    let raw = collect_global(repos, repo, |store, repo_name, single| {
        let result = get_blast_radius_raw(store, file);
        if result.is_empty() || single {
            result
        } else {
            result
                .lines()
                .map(|l| format!("[{repo_name}] {l}"))
                .collect::<Vec<_>>()
                .join("\n")
        }
    });
    sanitize_for_mcp(&raw)
}

// ── search_symbol ─────────────────────────────────────────────────────────────

/// Find symbol definitions matching a name across the indexed graph.
///
/// Returns matching symbols formatted as:
///   `fn:charge (function) — src/payment.ts`
pub fn search_symbol(store: &SqliteStore, name: &str) -> String {
    if let Err(reason) = validate_mcp_arg(name) {
        tracing::warn!("search_symbol rejected invalid arg: {reason}");
        return String::new();
    }
    sanitize_for_mcp(&search_symbol_raw(store, name))
}

fn search_symbol_raw(store: &SqliteStore, name: &str) -> String {
    // Cap results: prevents self-DoS from wildcard queries (e.g. "a") and limits
    // accidental bulk exfiltration. The store LIKE query has no SQL LIMIT yet —
    // this Rust-side cap is the guard until that is added at the store layer.
    const MAX_SEARCH_RESULTS: usize = 50;

    let nodes = match store.search_nodes_by_name(name) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("search_symbol error: {e}");
            return String::new();
        }
    };

    let lines: Vec<String> = nodes
        .iter()
        .take(MAX_SEARCH_RESULTS)
        .map(|n| format!("{} ({}) — {}", n.vname.signature, n.kind, n.vname.path))
        .collect();
    lines.join("\n")
}

/// Global variant of `search_symbol`.
pub fn search_symbol_global(
    repos: &HashMap<String, PathBuf>,
    name: &str,
    repo: Option<&str>,
) -> String {
    if let Err(reason) = validate_mcp_arg(name) {
        tracing::warn!("search_symbol_global rejected invalid arg: {reason}");
        return String::new();
    }
    let raw = collect_global(repos, repo, |store, repo_name, single| {
        let result = search_symbol_raw(store, name);
        if result.is_empty() || single {
            result
        } else {
            result
                .lines()
                .map(|l| format!("[{repo_name}] {l}"))
                .collect::<Vec<_>>()
                .join("\n")
        }
    });
    sanitize_for_mcp(&raw)
}

// ── get_repo_map ──────────────────────────────────────────────────────────────

/// Return a structural overview of the indexed repository.
///
/// Groups all indexed nodes by file path and renders a tree annotated with
/// symbol counts and the top symbols per file, suitable for LLM consumption:
///
/// ```text
/// src/
///   service.ts  [3 symbols]  fn:charge, class:PaymentService, fn:refund
///   index.ts    [1 symbol]   fn:activate
/// ```
pub fn get_repo_map(store: &SqliteStore) -> String {
    sanitize_for_mcp(&get_repo_map_raw(store))
}

fn get_repo_map_raw(store: &SqliteStore) -> String {
    use std::collections::BTreeMap;

    let nodes = match store.all_nodes() {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("get_repo_map error: {e}");
            return String::new();
        }
    };

    if nodes.is_empty() {
        return String::new();
    }

    // Group nodes by file path.
    let mut by_file: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for node in &nodes {
        let path = if node.vname.path.is_empty() {
            "<unknown>".to_string()
        } else {
            node.vname.path.clone()
        };
        // Only include named symbols (skip bare file nodes with empty signature).
        if !node.vname.signature.is_empty() && node.kind != "file" {
            by_file
                .entry(path)
                .or_default()
                .push(node.vname.signature.clone());
        }
    }

    const MAX_SYMBOLS_PER_FILE: usize = 5;
    let mut lines: Vec<String> = Vec::with_capacity(by_file.len() + 1);

    for (path, mut symbols) in by_file {
        symbols.sort();
        symbols.dedup();
        let count = symbols.len();
        let preview: Vec<&str> = symbols
            .iter()
            .take(MAX_SYMBOLS_PER_FILE)
            .map(|s| s.as_str())
            .collect();
        let suffix = if count > MAX_SYMBOLS_PER_FILE {
            format!(", … (+{})", count - MAX_SYMBOLS_PER_FILE)
        } else {
            String::new()
        };
        lines.push(format!(
            "{}  [{} symbol{}]  {}{}",
            path,
            count,
            if count == 1 { "" } else { "s" },
            preview.join(", "),
            suffix,
        ));
    }

    lines.join("\n")
}

/// Global variant of `get_repo_map`.
pub fn get_repo_map_global(repos: &HashMap<String, PathBuf>, repo: Option<&str>) -> String {
    // repo arg validated inside collect_global.
    let raw = collect_global(repos, repo, |store, repo_name, single| {
        let result = get_repo_map_raw(store);
        if result.is_empty() || single {
            result
        } else {
            result
                .lines()
                .map(|l| format!("[{repo_name}] {l}"))
                .collect::<Vec<_>>()
                .join("\n")
        }
    });
    sanitize_for_mcp(&raw)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// SEC-002 end-to-end: a path-traversal repo arg must be rejected through
    /// the full get_callers_global → collect_global → validate_mcp_arg pipeline.
    /// This exercises the wiring that the validate_mcp_arg unit tests in
    /// sanitize.rs do not cover — a regression here would be invisible to those
    /// unit tests.
    #[test]
    fn get_callers_global_rejects_path_traversal_repo_arg() {
        let repos: HashMap<String, PathBuf> = HashMap::new();
        let result = get_callers_global(&repos, "charge", Some("../evil"));
        // Invalid repo arg must return an empty envelope, not a panic or error.
        assert_eq!(
            result, "<travsr-data></travsr-data>",
            "path traversal in repo arg must be rejected and return empty envelope"
        );
    }

    /// SEC-002 end-to-end: an absolute-path repo arg must also be rejected.
    #[test]
    fn get_dependencies_global_rejects_absolute_repo_arg() {
        let repos: HashMap<String, PathBuf> = HashMap::new();
        let result = get_dependencies_global(&repos, "src/main.ts", Some("/etc/passwd"));
        assert_eq!(
            result, "<travsr-data></travsr-data>",
            "absolute path in repo arg must be rejected and return empty envelope"
        );
    }

    // ── blast radius unit tests ───────────────────────────────────────────────

    fn make_store(
        nodes: &[travsr_core::Node],
        edges: &[(
            travsr_core::NodeId,
            travsr_core::NodeId,
            travsr_core::EdgeKind,
        )],
    ) -> travsr_store::SqliteStore {
        use travsr_core::Edge;
        let mut store = travsr_store::SqliteStore::open_in_memory().unwrap();
        for n in nodes {
            store.put_node(n).unwrap();
        }
        for &(src, dst, kind) in edges {
            store.put_edge(&Edge::new(src, dst, kind)).unwrap();
        }
        store
    }

    fn make_node(path: &str, sig: &str) -> travsr_core::Node {
        use travsr_core::VName;
        travsr_core::Node::new(VName::new("", "", path, "typescript", sig), "function")
    }

    /// A file with no callers — blast radius is just itself.
    #[test]
    fn blast_radius_includes_source_file() {
        let a = make_node("a.ts", "fn:a");
        let store = make_store(std::slice::from_ref(&a), &[]);
        let result = get_blast_radius(&store, "a.ts");
        assert!(
            result.contains("a.ts"),
            "source file must appear in its own blast radius"
        );
    }

    /// B → A (incoming call): blast_radius("a.ts") must include b.ts.
    #[test]
    fn blast_radius_follows_incoming_edges() {
        use travsr_core::EdgeKind;
        let a = make_node("a.ts", "fn:a");
        let b = make_node("b.ts", "fn:b");
        // B calls A — reverse BFS from A should reach B.
        let store = make_store(&[a.clone(), b.clone()], &[(b.id, a.id, EdgeKind::RefCall)]);
        let result = get_blast_radius(&store, "a.ts");
        assert!(result.contains("a.ts"), "source file must be included");
        assert!(
            result.contains("b.ts"),
            "caller file must be included in blast radius"
        );
    }

    /// Cycle A ↔ B — must terminate without infinite loop.
    #[test]
    fn blast_radius_handles_cycle() {
        use travsr_core::EdgeKind;
        let a = make_node("a.ts", "fn:a");
        let b = make_node("b.ts", "fn:b");
        let store = make_store(
            &[a.clone(), b.clone()],
            &[
                (b.id, a.id, EdgeKind::RefCall),
                (a.id, b.id, EdgeKind::RefCall), // cycle
            ],
        );
        // Must not hang; both files reachable.
        let result = get_blast_radius(&store, "a.ts");
        assert!(result.contains("a.ts"));
        assert!(result.contains("b.ts"));
    }

    // ── get_repo_map unit tests ───────────────────────────────────────────────

    /// Two nodes in different files must produce two entries.
    #[test]
    fn get_repo_map_groups_by_file() {
        let a = make_node("src/a.ts", "fn:a");
        let b = make_node("src/b.ts", "fn:b");
        let store = make_store(&[a, b], &[]);
        let result = get_repo_map(&store);
        assert!(result.contains("src/a.ts"), "a.ts must appear in repo map");
        assert!(result.contains("src/b.ts"), "b.ts must appear in repo map");
    }

    /// File-kind nodes must not appear as symbols in the map.
    #[test]
    fn get_repo_map_excludes_file_kind_nodes() {
        use travsr_core::VName;
        let file_node = travsr_core::Node::new(
            VName::new("", "", "src/a.ts", "typescript", "src/a.ts"),
            "file",
        );
        let fn_node = make_node("src/a.ts", "fn:a");
        let store = make_store(&[file_node, fn_node], &[]);
        let result = get_repo_map(&store);
        // Should list "src/a.ts" as a file entry.
        assert!(result.contains("src/a.ts"));
        // The "file" kind node's signature must not appear as a symbol entry.
        // It should show [1 symbol] (only fn:a), not [2 symbols].
        assert!(
            result.contains("[1 symbol]"),
            "file-kind node must be excluded from symbol count"
        );
    }
}
