use std::collections::HashMap;
use std::path::PathBuf;

use travsr_core::{Node as CoreNode, NodeId};
use travsr_retrieval::{
    context_candidates, knapsack, token_cost, EdgeFilter, OpenFilter, MAX_CONTEXT_BUDGET,
    TOKEN_CHARS_PER_TOKEN,
};
use travsr_store::{SqliteStore, Store};

use crate::sanitize::{
    sanitize_for_mcp, sanitize_mcp_body_with_limit, validate_mcp_arg, wrap_envelope,
};

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

// ── get_graph_stats ───────────────────────────────────────────────────────────

/// Return accurate node and edge counts directly from the SQLite store.
///
/// Output format (newline-separated key-value pairs):
///   `nodes: 2121`
///   `edges: 8432`
///
/// Always returns a non-empty string — callers can check `nodes: 0` for an
/// empty graph. No sanitization needed: the output contains no user data.
pub fn get_graph_stats(store: &SqliteStore) -> String {
    let nodes = match store.node_count() {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("get_graph_stats node_count error: {e}");
            0
        }
    };
    let edges = match store.edge_count() {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("get_graph_stats edge_count error: {e}");
            0
        }
    };
    format!("nodes: {nodes}\nedges: {edges}")
}

/// Global variant of `get_graph_stats` — sums across all registered repos.
///
/// Input validation for `repo` is performed by `collect_global`; do not bypass that helper.
pub fn get_graph_stats_global(repos: &HashMap<String, PathBuf>, repo: Option<&str>) -> String {
    let mut total_nodes: u64 = 0;
    let mut total_edges: u64 = 0;
    // DEBT(cloud-launch): counts must be filtered to caller's EdgeFilter scope before SSE ships
    collect_global(repos, repo, |store, _repo_name, _single| {
        total_nodes += match store.node_count() {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("get_graph_stats_global node_count error: {e}");
                0
            }
        };
        total_edges += match store.edge_count() {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("get_graph_stats_global edge_count error: {e}");
                0
            }
        };
        String::new() // accumulation done via captured mutables; return value unused
    });
    format!("nodes: {total_nodes}\nedges: {total_edges}")
}

// ── get_execution_path ────────────────────────────────────────────────────────

/// Find a traversal path from `source` symbol to `sink` symbol through the graph.
///
/// Uses PCST (Prize-Collecting Steiner Tree) approximation when a path exists,
/// falls back to BFS depth-3 on timeout (> 80ms) or disconnected graphs.
///
/// Output format (one line per node on path):
///   `fn:charge (function) — src/payment.ts`
///   `fn:processPayment (function) — src/processor.ts`
///
/// # SEC P0
/// Returns empty string for both "symbol not found" and "symbol access denied".
/// These cases are indistinguishable to the caller (prevents existence oracle).
pub fn get_execution_path(store: &SqliteStore, source: &str, sink: &str) -> String {
    get_execution_path_with_filter(store, source, sink, &OpenFilter)
}

/// Authenticated variant — applies RBAC filter at traversal time.
/// Wired to session context in S16 when `SessionStore` is integrated into the server loop.
// DEBT(travsr-199): wire into server.rs dispatch when SessionStore lands in S16.
#[allow(dead_code)]
pub(crate) fn get_execution_path_authed(
    store: &SqliteStore,
    source: &str,
    sink: &str,
    filter: &dyn EdgeFilter,
) -> String {
    get_execution_path_with_filter(store, source, sink, filter)
}

fn get_execution_path_with_filter(
    store: &SqliteStore,
    source: &str,
    sink: &str,
    filter: &dyn EdgeFilter,
) -> String {
    if let Err(reason) = validate_mcp_arg(source) {
        tracing::warn!("get_execution_path rejected invalid source arg: {reason}");
        return String::new();
    }
    if let Err(reason) = validate_mcp_arg(sink) {
        tracing::warn!("get_execution_path rejected invalid sink arg: {reason}");
        return String::new();
    }
    sanitize_for_mcp(&get_execution_path_raw(store, source, sink, filter))
}

fn get_execution_path_raw(
    store: &SqliteStore,
    source: &str,
    sink: &str,
    filter: &dyn EdgeFilter,
) -> String {
    // SEC P0: resolve source and sink; treat "not found" == "access denied" identically.
    let src_node = match store.search_nodes_by_name(source) {
        Ok(n) => n
            .into_iter()
            .find(|n| filter.allow(n.id, n.id, Some(n.vname.corpus.as_str()))),
        Err(e) => {
            tracing::warn!("get_execution_path source search error: {e}");
            return String::new();
        }
    };

    let sink_node = match store.search_nodes_by_name(sink) {
        Ok(n) => n
            .into_iter()
            .find(|n| filter.allow(n.id, n.id, Some(n.vname.corpus.as_str()))),
        Err(e) => {
            tracing::warn!("get_execution_path sink search error: {e}");
            return String::new();
        }
    };

    // SEC P0: both outcomes (not found and access denied) produce the same empty result.
    let (src, snk) = match (src_node, sink_node) {
        (Some(a), Some(b)) => (a, b),
        _ => return String::new(),
    };

    let path = match travsr_retrieval::pcst_path(store, src.id, snk.id, filter, 4096) {
        Ok(nodes) => nodes,
        Err(e) => {
            tracing::warn!("get_execution_path pcst error: {e}");
            return String::new();
        }
    };

    if path.is_empty() {
        return String::new();
    }

    let lines: Vec<String> = path
        .iter()
        .map(|n| format!("{} ({}) — {}", n.vname.signature, n.kind, n.vname.path))
        .collect();
    lines.join("\n")
}

/// Global variant of `get_execution_path` — searches one named repo or all registered repos.
///
/// `filter` is applied at traversal time. Pass `&OpenFilter` for unauthenticated local mode;
/// pass `&session.filter()` in authenticated mode (S16). Do NOT hardcode `&OpenFilter` at
/// call sites — the caller owns the auth context.
pub fn get_execution_path_global(
    repos: &HashMap<String, PathBuf>,
    source: &str,
    sink: &str,
    repo: Option<&str>,
    filter: &dyn EdgeFilter,
) -> String {
    if let Err(reason) = validate_mcp_arg(source) {
        tracing::warn!("get_execution_path_global rejected invalid source: {reason}");
        return String::new();
    }
    if let Err(reason) = validate_mcp_arg(sink) {
        tracing::warn!("get_execution_path_global rejected invalid sink: {reason}");
        return String::new();
    }
    let raw = collect_global(repos, repo, |store, repo_name, single| {
        let result = get_execution_path_raw(store, source, sink, filter);
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

// ── get_context ───────────────────────────────────────────────────────────────

/// Retrieve the most relevant context for `query` within `token_budget` tokens.
///
/// Pipeline: validate → seed lookup (RBAC-filtered) → PPR → get_nodes →
/// knapsack → format → sanitize → append footer → wrap envelope.
///
/// # SEC P0
/// Returns identical output for "not found" and "access denied" to prevent
/// existence oracle attacks. Seeds are filtered through the `OpenFilter` so
/// callers cannot distinguish missing vs denied symbols.
pub fn get_context(store: &SqliteStore, query: &str, token_budget: usize) -> String {
    get_context_with_filter(store, query, token_budget, &OpenFilter)
}

/// Authenticated variant — applies RBAC filter at seed lookup and node fetch.
#[allow(dead_code)]
pub(crate) fn get_context_authed(
    store: &SqliteStore,
    query: &str,
    token_budget: usize,
    filter: &dyn EdgeFilter,
) -> String {
    get_context_with_filter(store, query, token_budget, filter)
}

/// Raw variant — returns body without envelope. Used by global aggregation to
/// prevent double-sanitization when multiple stores are aggregated before wrapping.
pub(crate) fn get_context_raw(store: &SqliteStore, query: &str, token_budget: usize) -> String {
    get_context_body(store, query, token_budget, &OpenFilter)
}

fn get_context_with_filter(
    store: &SqliteStore,
    query: &str,
    token_budget: usize,
    filter: &dyn EdgeFilter,
) -> String {
    let body = get_context_body(store, query, token_budget, filter);
    wrap_envelope(&body)
}

fn get_context_body(
    store: &SqliteStore,
    query: &str,
    token_budget: usize,
    filter: &dyn EdgeFilter,
) -> String {
    // SEC-002: validate before any store access.
    if let Err(reason) = validate_mcp_arg(query) {
        tracing::warn!("get_context rejected invalid query arg: {reason}");
        return String::new();
    }
    // Defense-in-depth budget guard (RFC-010 §3.3).
    if token_budget > MAX_CONTEXT_BUDGET {
        return "token_budget exceeds maximum allowed value".to_string();
    }
    if token_budget == 0 {
        return String::new();
    }

    // Seed lookup: up to 5 seeds matching query, RBAC-filtered (SEC P0).
    let seeds: Vec<NodeId> = match store.search_nodes_by_name(query) {
        Ok(nodes) => nodes
            .into_iter()
            .filter(|n| filter.allow(n.id, n.id, Some(n.vname.corpus.as_str())))
            .take(5)
            .map(|n| n.id)
            .collect(),
        Err(e) => {
            tracing::warn!("get_context seed search error: {e}");
            return String::new();
        }
    };

    // SEC P0: identical response for "not found" and "access denied".
    if seeds.is_empty() {
        return format!("No symbols matching '{query}' found in the graph.");
    }

    // PPR over the seed set.
    let ppr_scores = match travsr_retrieval::ppr(store, &seeds, context_candidates()) {
        Ok(scores) => scores,
        Err(e) => {
            tracing::warn!("get_context ppr error: {e}");
            return String::new();
        }
    };

    if ppr_scores.is_empty() {
        return format!("No symbols matching '{query}' found in the graph.");
    }

    // Build score map for keyed join (prevents node/score misalignment).
    let score_map: HashMap<NodeId, f32> = ppr_scores.iter().cloned().collect();
    let node_ids: Vec<NodeId> = ppr_scores.into_iter().map(|(id, _)| id).collect();

    // Batch-fetch nodes.
    let fetched = match store.get_nodes(&node_ids) {
        Ok(nodes) => nodes,
        Err(e) => {
            tracing::warn!("get_context get_nodes error: {e}");
            return String::new();
        }
    };

    // Post-filter fetched nodes through RBAC and join with scores.
    let items: Vec<(CoreNode, f32)> = fetched
        .into_iter()
        .filter(|n| filter.allow(n.id, n.id, Some(n.vname.corpus.as_str())))
        .filter_map(|n| score_map.get(&n.id).map(|&s| (n, s)))
        .collect();

    if items.is_empty() {
        return format!("No symbols matching '{query}' found in the graph.");
    }

    // Knapsack selection.
    let selected = knapsack(items, token_budget);
    let n_nodes = selected.len();
    let total_tokens: usize = selected.iter().map(token_cost).sum();

    // Format body lines.
    let lines: Vec<String> = selected
        .iter()
        .map(|n| {
            format!(
                "{} ({}) — {} [package: {}]",
                n.vname.signature, n.kind, n.vname.path, n.package
            )
        })
        .collect();
    let body = lines.join("\n");

    // Sanitize body (no envelope yet — footer is appended after).
    let sanitized = sanitize_mcp_body_with_limit(
        &body,
        (token_budget * TOKEN_CHARS_PER_TOKEN * 2).min(1_024_000),
    );

    // Append footer then wrap — footer is always present, never truncated.
    format!("{sanitized}\n\n[{n_nodes} nodes, ~{total_tokens} tokens]")
}

/// Global variant of `get_context` — searches one named repo or all registered repos.
pub fn get_context_global(
    repos: &HashMap<String, PathBuf>,
    query: &str,
    token_budget: usize,
    repo: Option<&str>,
) -> String {
    if let Err(reason) = validate_mcp_arg(query) {
        tracing::warn!("get_context_global rejected invalid query: {reason}");
        return wrap_envelope("");
    }
    if token_budget > MAX_CONTEXT_BUDGET {
        return wrap_envelope("token_budget exceeds maximum allowed value");
    }
    let raw = collect_global(repos, repo, |store, repo_name, single| {
        let result = get_context_raw(store, query, token_budget);
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
    wrap_envelope(&raw)
}

// ── get_graph_json ────────────────────────────────────────────────────────────

/// Max nodes returned by `get_graph_json` to keep MCP payloads manageable.
const MAX_GRAPH_JSON_NODES: usize = 200;

/// Return a subgraph around `query` as structured JSON for graph renderers.
///
/// BFS from seed node(s) matching `query`, respecting `direction` and `depth`.
/// Returns `{"nodes":[...],"edges":[...]}`.
/// Unlike prose tools, output is NOT sanitized — it is structured JSON consumed
/// by the VS Code graph panel, not forwarded to an LLM as freetext.
pub fn get_graph_json(
    store: &SqliteStore,
    query: &str,
    direction: &str,
    depth: u8,
    kind_filter: &str,
) -> String {
    // Only "" (all kinds) and "file" are valid kind_filter values.
    if !matches!(kind_filter, "" | "file") {
        tracing::warn!("get_graph_json rejected unknown kind_filter: {kind_filter}");
        return "{}".to_string();
    }
    // Empty query is valid when kind_filter=="file" (returns full import graph).
    if !(query.is_empty() && kind_filter == "file") {
        if let Err(reason) = validate_mcp_arg(query) {
            tracing::warn!("get_graph_json rejected invalid arg: {reason}");
            return "{}".to_string();
        }
    }
    let depth = depth.clamp(1, 4);
    get_graph_json_raw(store, query, direction, depth, kind_filter)
}

fn edge_kind_str(kind: &travsr_core::EdgeKind) -> &'static str {
    use travsr_core::EdgeKind;
    match kind {
        EdgeKind::DefinesBinding => "defines",
        EdgeKind::RefCall => "calls",
        EdgeKind::Depends => "imports",
        EdgeKind::ResolvesTo => "resolves-to",
        EdgeKind::Exports => "exports",
        EdgeKind::RefImports => "ref/imports",
        EdgeKind::IsImplementation => "is-implementation",
        EdgeKind::Overrides => "overrides",
        EdgeKind::FFICall => "ffi/call",
    }
}

/// Unique JSON id for a node.
/// File nodes all share `signature == "file"`, so we use `path` (prefixed with
/// corpus when non-empty) to keep Cytoscape ids distinct across repos.
fn node_json_id(node: &CoreNode) -> String {
    if node.kind == "file" {
        if node.vname.corpus.is_empty() {
            node.vname.path.clone()
        } else {
            format!("{}:{}", node.vname.corpus, node.vname.path)
        }
    } else {
        node.vname.signature.clone()
    }
}

/// Short display label — basename for files, full signature for everything else.
fn node_json_label(node: &CoreNode) -> &str {
    if node.kind == "file" {
        node.vname
            .path
            .rsplit('/')
            .next()
            .unwrap_or(&node.vname.path)
    } else {
        &node.vname.signature
    }
}

fn get_graph_json_raw(
    store: &SqliteStore,
    query: &str,
    direction: &str,
    depth: u8,
    kind_filter: &str,
) -> String {
    use std::collections::{HashSet, VecDeque};

    // File mode with empty query: search broadly by path separator to seed all file nodes.
    // DEBT(travsr): replace "." sentinel with an explicit all-files store query
    let search_term = if kind_filter == "file" && query.is_empty() {
        "."
    } else {
        query
    };
    let seed_nodes_raw = match store.search_nodes_by_name(search_term) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("get_graph_json search error: {e}");
            return r#"{"nodes":[],"edges":[]}"#.to_string();
        }
    };

    // Filter seeds to the requested kind when kind_filter is set.
    let seed_nodes: Vec<_> = if !kind_filter.is_empty() {
        seed_nodes_raw
            .into_iter()
            .filter(|n| n.kind == kind_filter)
            .collect()
    } else {
        seed_nodes_raw
    };

    if seed_nodes.is_empty() {
        return r#"{"nodes":[],"edges":[]}"#.to_string();
    }

    // (NodeId, hop_distance)
    let mut visited: HashSet<NodeId> = HashSet::new();
    let mut queue: VecDeque<(NodeId, u8)> = VecDeque::new();
    let mut nodes_out: Vec<serde_json::Value> = Vec::new();
    let mut edges_out: Vec<serde_json::Value> = Vec::new();
    let mut edge_seen: HashSet<(NodeId, NodeId, &'static str)> = HashSet::new();

    for node in &seed_nodes {
        if visited.insert(node.id) {
            queue.push_back((node.id, 0));
        }
    }

    while let Some((current_id, hop)) = queue.pop_front() {
        let node = match store.get_node(current_id) {
            Ok(Some(n)) => n,
            _ => continue,
        };

        // Skip nodes that don't match the kind filter.
        if !kind_filter.is_empty() && node.kind != kind_filter {
            continue;
        }

        let score = {
            let raw = 0.7_f64.powi(i32::from(hop));
            (raw * 1000.0).round() / 1000.0
        };
        let mut node_obj = serde_json::json!({
            "id":      node_json_id(&node),
            "label":   node_json_label(&node),
            "kind":    node.kind,
            "path":    node.vname.path,
            "package": node.package,
            "score":   score,
            "line":    node.line,
        });
        if hop == 0 {
            node_obj["root"] = serde_json::Value::Bool(true);
        }
        nodes_out.push(node_obj);

        if nodes_out.len() >= MAX_GRAPH_JSON_NODES {
            tracing::warn!(
                "get_graph_json hit node cap ({MAX_GRAPH_JSON_NODES}) for query '{query}'"
            );
            continue;
        }

        if hop >= depth {
            continue;
        }

        // File mode: the import schema uses a two-hop chain
        //   file --[Depends]--> import_node --[ResolvesTo]--> file
        // Look through the intermediary to emit direct file→file edges.
        if kind_filter == "file" {
            use travsr_core::EdgeKind;
            if direction == "deps" || direction == "both" {
                if let Ok(dep_edges) = store.iter_edges_from(current_id) {
                    for dep in &dep_edges {
                        if !matches!(dep.kind, EdgeKind::Depends) {
                            continue;
                        }
                        if let Ok(res_edges) = store.iter_edges_from(dep.dst) {
                            for res in &res_edges {
                                if !matches!(res.kind, EdgeKind::ResolvesTo) {
                                    continue;
                                }
                                if let Ok(Some(target)) = store.get_node(res.dst) {
                                    if target.kind != "file" {
                                        continue;
                                    }
                                    if edge_seen.insert((current_id, target.id, "imports")) {
                                        edges_out.push(serde_json::json!({
                                            "source": node_json_id(&node),
                                            "target": node_json_id(&target),
                                            "kind":   "imports",
                                        }));
                                    }
                                    if visited.insert(target.id) {
                                        queue.push_back((target.id, hop + 1));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if direction == "callers" || direction == "both" {
                // Reverse: who imports this file?
                //   importer_file --[Depends]--> import_node --[ResolvesTo]--> current_file
                if let Ok(rev_res_edges) = store.iter_edges_to(current_id) {
                    for rev_res in &rev_res_edges {
                        if !matches!(rev_res.kind, EdgeKind::ResolvesTo) {
                            continue;
                        }
                        if let Ok(rev_dep_edges) = store.iter_edges_to(rev_res.src) {
                            for rev_dep in &rev_dep_edges {
                                if !matches!(rev_dep.kind, EdgeKind::Depends) {
                                    continue;
                                }
                                if let Ok(Some(source)) = store.get_node(rev_dep.src) {
                                    if source.kind != "file" {
                                        continue;
                                    }
                                    if edge_seen.insert((source.id, current_id, "imports")) {
                                        edges_out.push(serde_json::json!({
                                            "source": node_json_id(&source),
                                            "target": node_json_id(&node),
                                            "kind":   "imports",
                                        }));
                                    }
                                    if visited.insert(source.id) {
                                        queue.push_back((source.id, hop + 1));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            continue; // skip normal edge traversal
        }

        // Normal traversal (symbol mode)
        if direction == "deps" || direction == "both" {
            if let Ok(edges) = store.iter_edges_from(current_id) {
                for edge in &edges {
                    let kind_s = edge_kind_str(&edge.kind);
                    if edge_seen.insert((edge.src, edge.dst, kind_s)) {
                        if let Ok(Some(dst)) = store.get_node(edge.dst) {
                            edges_out.push(serde_json::json!({
                                "source": node_json_id(&node),
                                "target": node_json_id(&dst),
                                "kind":   kind_s,
                            }));
                        }
                    }
                    if visited.insert(edge.dst) {
                        queue.push_back((edge.dst, hop + 1));
                    }
                }
            }
        }

        if direction == "callers" || direction == "both" {
            if let Ok(edges) = store.iter_edges_to(current_id) {
                for edge in &edges {
                    let kind_s = edge_kind_str(&edge.kind);
                    if edge_seen.insert((edge.src, edge.dst, kind_s)) {
                        if let Ok(Some(src)) = store.get_node(edge.src) {
                            edges_out.push(serde_json::json!({
                                "source": node_json_id(&src),
                                "target": node_json_id(&node),
                                "kind":   kind_s,
                            }));
                        }
                    }
                    if visited.insert(edge.src) {
                        queue.push_back((edge.src, hop + 1));
                    }
                }
            }
        }
    }

    match serde_json::to_string(&serde_json::json!({
        "nodes": nodes_out,
        "edges": edges_out,
    })) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("get_graph_json serialization error: {e}");
            "{}".to_string()
        }
    }
}

/// Global variant of `get_graph_json` — merges subgraphs across repos, deduping by node id.
pub fn get_graph_json_global(
    repos: &HashMap<String, PathBuf>,
    query: &str,
    direction: &str,
    depth: u8,
    repo: Option<&str>,
    kind_filter: &str,
) -> String {
    if !(query.is_empty() && kind_filter == "file") {
        if let Err(reason) = validate_mcp_arg(query) {
            tracing::warn!("get_graph_json_global rejected invalid arg: {reason}");
            return "{}".to_string();
        }
    }
    let depth = depth.clamp(1, 4);

    let candidates: Vec<(&str, &PathBuf)> = match repo {
        Some(name) => {
            if let Err(reason) = validate_mcp_arg(name) {
                tracing::warn!("get_graph_json_global rejected invalid repo arg: {reason}");
                return "{}".to_string();
            }
            match repos.get_key_value(name) {
                Some((k, v)) => vec![(k.as_str(), v)],
                None => {
                    tracing::warn!("repo '{name}' not found in registry");
                    return r#"{"nodes":[],"edges":[]}"#.to_string();
                }
            }
        }
        None => repos.iter().map(|(k, v)| (k.as_str(), v)).collect(),
    };

    let mut all_nodes: Vec<serde_json::Value> = Vec::new();
    let mut all_edges: Vec<serde_json::Value> = Vec::new();
    let mut seen_node_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut seen_edge_keys: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();

    for (_repo_name, db_path) in candidates {
        if !db_path.exists() {
            continue;
        }
        match SqliteStore::open(db_path) {
            Ok(store) => {
                let raw = get_graph_json_raw(&store, query, direction, depth, kind_filter);
                let parsed: serde_json::Value = match serde_json::from_str(&raw) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                for node in parsed["nodes"].as_array().into_iter().flatten() {
                    let id = node["id"].as_str().unwrap_or("").to_string();
                    if !id.is_empty() && seen_node_ids.insert(id) {
                        all_nodes.push(node.clone());
                    }
                }
                for edge in parsed["edges"].as_array().into_iter().flatten() {
                    let src = edge["source"].as_str().unwrap_or("").to_string();
                    let tgt = edge["target"].as_str().unwrap_or("").to_string();
                    if !src.is_empty() && seen_edge_keys.insert((src, tgt)) {
                        all_edges.push(edge.clone());
                    }
                }
            }
            Err(e) => tracing::warn!("failed to open {}: {e}", db_path.display()),
        }
    }

    match serde_json::to_string(&serde_json::json!({
        "nodes": all_nodes,
        "edges": all_edges,
    })) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("get_graph_json_global serialization error: {e}");
            "{}".to_string()
        }
    }
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

    // ── get_graph_stats unit tests ───────────────────────────────────────────

    #[test]
    fn get_graph_stats_empty_graph() {
        let store = make_store(&[], &[]);
        assert_eq!(get_graph_stats(&store), "nodes: 0\nedges: 0");
    }

    #[test]
    fn get_graph_stats_counts_match_store() {
        use travsr_core::EdgeKind;
        let a = make_node("a.ts", "fn:a");
        let b = make_node("b.ts", "fn:b");
        let store = make_store(&[a.clone(), b.clone()], &[(a.id, b.id, EdgeKind::Depends)]);
        assert_eq!(get_graph_stats(&store), "nodes: 2\nedges: 1");
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
    fn get_graph_json_includes_line_for_symbol_nodes() {
        use travsr_core::{Node, VName};
        let mut store = travsr_store::SqliteStore::open_in_memory().unwrap();
        let sym = Node::new(
            VName::new("test", "", "src/foo.ts", "typescript", "fn:bar"),
            "function",
        )
        .with_line(42);
        let file = Node::new(
            VName::new("test", "", "src/foo.ts", "typescript", "file"),
            "file",
        );
        store.put_node(&sym).unwrap();
        store.put_node(&file).unwrap();

        let json = get_graph_json(&store, "fn:bar", "both", 1, "");
        assert!(
            json.contains("\"line\":42"),
            "symbol node must carry line in JSON: {json}"
        );

        let file_json = get_graph_json(&store, "src/foo.ts", "both", 1, "file");
        assert!(
            file_json.contains("\"line\":null"),
            "file node must have null line in JSON: {file_json}"
        );
    }

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
