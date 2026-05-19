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
