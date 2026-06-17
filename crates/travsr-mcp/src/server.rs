//! MCP stdio event loop.
//!
//! Reads newline-delimited JSON-RPC 2.0 messages from stdin, dispatches them,
//! and writes responses to stdout. All diagnostics go to stderr via tracing
//! so stdout stays clean for the MCP client.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use crate::protocol::{INVALID_PARAMS, METHOD_NOT_FOUND, PARSE_ERROR};
use crate::tools;
use crate::{protocol::RpcRequest, PROTOCOL_VERSION, SERVER_NAME, SERVER_VERSION};
use travsr_retrieval::OpenFilter;
use travsr_store::{registry, SqliteStore};

pub fn run(store: &mut SqliteStore) -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    for line_result in BufReader::new(stdin.lock()).lines() {
        let line = line_result?;
        if line.trim().is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<RpcRequest>(&line) {
            Ok(req) => handle_request(&mut *store, req),
            Err(e) => {
                tracing::debug!("JSON parse error: {e}");
                Some(error_response(
                    serde_json::Value::Null,
                    PARSE_ERROR,
                    format!("parse error: {e}"),
                ))
            }
        };

        if let Some(resp) = response {
            writeln!(out, "{resp}")?;
            out.flush()?;
        }
    }
    Ok(())
}

/// Capabilities advertised in the `initialize` response. The `prompts`
/// capability is only declared when `mcp-sampling` is enabled, so that a
/// spec-compliant client can discover and call the `prompts/*` endpoints that
/// feature exposes; without it those endpoints are unreachable from a
/// conformant host.
fn server_capabilities() -> serde_json::Value {
    #[cfg(feature = "mcp-sampling")]
    {
        serde_json::json!({ "tools": {}, "prompts": {} })
    }
    #[cfg(not(feature = "mcp-sampling"))]
    {
        serde_json::json!({ "tools": {} })
    }
}

fn handle_request(store: &mut SqliteStore, req: RpcRequest) -> Option<String> {
    // JSON-RPC notifications have no id — must never receive a response.
    let id = match req.id {
        Some(id) => id,
        None => {
            tracing::debug!("notification received: {}", req.method);
            return None;
        }
    };

    let resp = match req.method.as_str() {
        "initialize" => ok_response(
            id,
            serde_json::json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": server_capabilities(),
                "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION }
            }),
        ),

        "tools/list" => ok_response(id, tools_list()),

        "tools/call" => {
            let params = req.params.unwrap_or(serde_json::Value::Null);
            handle_tool_call(store, id, params)
        }

        #[cfg(feature = "mcp-sampling")]
        "prompts/list" => ok_response(id, prompts_list()),

        #[cfg(feature = "mcp-sampling")]
        "prompts/get" => {
            let name = req
                .params
                .as_ref()
                .and_then(|p| p["name"].as_str())
                .unwrap_or("");
            handle_prompts_get(id, name)
        }

        other => {
            tracing::debug!("unknown method: {other}");
            error_response(id, METHOD_NOT_FOUND, format!("method not found: {other}"))
        }
    };

    Some(resp)
}

fn handle_tool_call(
    store: &mut SqliteStore,
    id: serde_json::Value,
    params: serde_json::Value,
) -> String {
    let tool_name = match params["name"].as_str() {
        Some(n) => n,
        None => return error_response(id, INVALID_PARAMS, "missing tool name".into()),
    };
    let args = &params["arguments"];

    let _span = tracing::info_span!("mcp.tool_call", tool = tool_name).entered();
    let text = match tool_name {
        "get_dependencies" => {
            let file = args["file"].as_str().unwrap_or("");
            let transitive = args["transitive"].as_bool().unwrap_or(false);
            let depth = args["depth"].as_u64().unwrap_or(3).clamp(1, 10) as u32;
            tools::get_dependencies(store, file, transitive, depth)
        }
        "get_callers" => {
            let symbol = args["symbol"].as_str().unwrap_or("");
            tools::get_callers(store, symbol)
        }
        "get_blast_radius" => {
            let file = args["file"].as_str().unwrap_or("");
            let mode = match args["analysis"].as_str().unwrap_or("tree-sitter") {
                "semantic" => tools::AnalysisMode::Semantic,
                _ => tools::AnalysisMode::TreeSitter,
            };
            tools::get_blast_radius(store, file, mode)
        }
        "get_lang_status" => {
            let file = args["file"].as_str().unwrap_or("");
            tools::get_lang_status(store, file)
        }
        "search_symbol" => {
            let name = args["name"].as_str().unwrap_or("");
            tools::search_symbol(store, name)
        }
        "get_repo_map" => tools::get_repo_map(store),
        "get_graph_stats" => tools::get_graph_stats(store),
        "get_execution_path" => {
            let source = args["source"].as_str().unwrap_or("");
            let sink = args["sink"].as_str().unwrap_or("");
            tools::get_execution_path(store, source, sink)
        }
        "get_context" => {
            let query = args["query"].as_str().unwrap_or("");
            let token_budget = args["token_budget"].as_u64().unwrap_or(4096) as usize;
            tools::get_context(store, query, token_budget)
        }
        "get_graph_json" => {
            let query = args["query"].as_str().unwrap_or("");
            let direction = args["direction"].as_str().unwrap_or("both");
            // McpClient sends Record<string,string> so depth arrives as a JSON
            // string ("3"), not a number. Try numeric first for direct callers,
            // then fall back to string parsing for the VS Code extension path.
            let depth = args["depth"]
                .as_u64()
                .or_else(|| args["depth"].as_str().and_then(|s| s.parse::<u64>().ok()))
                .unwrap_or(2)
                .clamp(1, 4) as u8;
            let kind_filter = args["kind_filter"].as_str().unwrap_or("");
            // #318 O6: optional token budget (additive arg). 0 = unlimited,
            // preserving pre-#318 behaviour when the arg is absent.
            let token_budget = args["token_budget"]
                .as_u64()
                .or_else(|| {
                    args["token_budget"]
                        .as_str()
                        .and_then(|s| s.parse::<u64>().ok())
                })
                .unwrap_or(0) as usize;
            // #319 P3: LOD repo-map overview mode + package drill path_prefix.
            let mode = args["mode"].as_str().unwrap_or("");
            let path_prefix = args["path_prefix"].as_str().unwrap_or("");
            tools::get_graph_json(
                store,
                &tools::GraphJsonParams {
                    query,
                    direction,
                    depth,
                    kind_filter,
                    token_budget,
                    mode,
                    path_prefix,
                },
            )
        }
        // RFC-012 A2 F1: dynamic synonym management. Single-repo (stdio) only —
        // see `handle_tool_call_global` for the multi-repo rejection.
        "synonym_add" => tools::synonym_add(
            store,
            args["term"].as_str().unwrap_or(""),
            args["alias"].as_str().unwrap_or(""),
        ),
        "synonym_set" => tools::synonym_set(
            store,
            args["term"].as_str().unwrap_or(""),
            args["aliases"].as_str().unwrap_or(""),
        ),
        "synonym_remove" => tools::synonym_remove(
            store,
            args["term"].as_str().unwrap_or(""),
            args["alias"].as_str().unwrap_or(""),
        ),
        "synonym_remove_term" => {
            tools::synonym_remove_term(store, args["term"].as_str().unwrap_or(""))
        }
        "synonym_list" => tools::synonym_list(store),
        "synonym_reset" => tools::synonym_reset(store),
        // VSCODE-247: global-registry management for the "Registered repos" webview.
        "repos_list" => tools::repos_list(),
        "repos_prune" => tools::repos_prune(),
        "repos_remove" => tools::repos_remove(args["name"].as_str().unwrap_or("")),
        "repo_languages" => tools::repo_languages(store),
        "get_snippets" => {
            let symbols = args["symbols"].as_str().unwrap_or("");
            let token_budget = args["token_budget"]
                .as_u64()
                .unwrap_or(tools::SNIPPET_DEFAULT_BUDGET as u64)
                as usize;
            tools::get_snippets(store, symbols, token_budget)
        }
        other => {
            return error_response(id, INVALID_PARAMS, format!("unknown tool: {other}"));
        }
    };

    // tool_calls_total=1 is a log-based counter field for tracing subscribers.
    // TODO(travsr-060): replace with otel Counter metric for proper OTLP aggregation.
    tracing::info!(
        tool = tool_name,
        tool_calls_total = 1u64,
        "mcp.tool_call complete"
    );
    ok_response(
        id,
        serde_json::json!({ "content": [{ "type": "text", "text": text }] }),
    )
}

fn tools_list() -> serde_json::Value {
    serde_json::json!({
        "_schemaVersion": "1.1.0",
        "tools": [
            {
                "name": "get_dependencies",
                "description": "Return the files and modules that a given file imports. Set transitive=true to follow `depends` edges recursively up to `depth` hops.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "file": { "type": "string", "description": "Repo-relative file path to look up dependencies for" },
                        "transitive": { "type": "boolean", "description": "Follow dependency edges recursively. Default: false (direct imports only)." },
                        "depth": { "type": "integer", "minimum": 1, "maximum": 10, "description": "Max hops when transitive=true. Default: 3." }
                    },
                    "required": ["file"],
                    "additionalProperties": false
                }
            },
            {
                "name": "get_callers",
                "description": "Return all graph nodes that have an incoming edge to the given symbol.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "symbol": { "type": "string", "description": "Symbol name to find callers of (partial match supported)" }
                    },
                    "required": ["symbol"],
                    "additionalProperties": false
                }
            },
            {
                "name": "get_blast_radius",
                "description": "Return the set of files transitively affected if the given file changes.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "file": { "type": "string", "description": "Repo-relative file path to compute blast radius for" },
                        "analysis": { "type": "string", "enum": ["tree-sitter", "semantic"], "description": "Edge mode: 'tree-sitter' (default, structural) or 'semantic' (RefCall only, requires Phase B)." }
                    },
                    "required": ["file"],
                    "additionalProperties": false
                }
            },
            {
                "name": "get_lang_status",
                "description": "Return whether semantic (Phase B) analysis is available for the language of the given file, and an install hint if not. Returns JSON.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "file": { "type": "string", "description": "Repo-relative file path to detect language for" }
                    },
                    "required": ["file"],
                    "additionalProperties": false
                }
            },
            {
                "name": "search_symbol",
                "description": "Find symbol definitions matching a name across the indexed graph. Accepts exact symbol names, partial matches, and natural-language queries (e.g. 'auth session validation', 'mcp dispatch tool call'). Query normalisation is deterministic: no model or API key required.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Symbol name or natural-language query (1–200 chars). Partial and NL queries are supported." }
                    },
                    "required": ["name"],
                    "additionalProperties": false
                }
            },
            {
                "name": "get_repo_map",
                "description": "Return a structural overview of the indexed repository.",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }
            },
            {
                "name": "get_execution_path",
                "description": "Find a traversal path from source symbol to sink symbol through the code graph using PCST.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "source": { "type": "string", "description": "Source symbol name (partial match supported)" },
                        "sink": { "type": "string", "description": "Sink symbol name (partial match supported)" }
                    },
                    "required": ["source", "sink"],
                    "additionalProperties": false
                }
            },
            {
                "name": "get_graph_stats",
                "description": "Return accurate node and edge counts from the indexed graph.",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }
            },
            {
                "name": "get_context",
                "description": "Retrieve the most relevant context for a query within a token budget using PPR + 0-1 knapsack. Accepts symbol names and natural-language queries (e.g. 'where is the auth session validated?'). A three-layer heuristic normaliser (T0 stopword strip + synonym expansion + L2-A vocabulary-grounded expansion) translates NL to FTS seeds deterministically — no model or API key required.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Symbol name or natural-language query (1–200 chars)" },
                        "token_budget": { "type": "integer", "description": "Hard token budget (100–32000). Defaults to 4096 if omitted." }
                    },
                    "required": ["query"],
                    "additionalProperties": false
                }
            },
            {
                "name": "get_graph_json",
                "description": "Return a subgraph around a symbol as structured JSON nodes and edges for graph renderers. Pass mode='overview' with no query for the repo-map LOD tile layout.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Symbol name or partial match (1–200 chars). May be empty when kind_filter is 'file' or mode is 'overview'." },
                        "direction": { "type": "string", "enum": ["deps", "callers", "both"], "description": "Edge direction. Default: both" },
                        "depth": { "type": "integer", "minimum": 1, "maximum": 4, "description": "BFS depth. Default: 2" },
                        "kind_filter": { "type": "string", "enum": ["file", ""], "description": "Restrict nodes to a specific kind. 'file' returns only file nodes and imports edges (project module map). Default: empty (all kinds)." },
                        "token_budget": { "type": "integer", "description": "Cap the payload to roughly this many tokens (0 or omitted = unlimited). Truncation is reported via truncated_by_budget." },
                        "mode": { "type": "string", "enum": ["", "overview"], "description": "'overview' returns synthetic package-level tile nodes sized by file count plus cross-package import edges. Combine with path_prefix to drill into a package." },
                        "path_prefix": { "type": "string", "description": "When mode='overview', scope to files under this path prefix (e.g. 'src/components/'). Returns file nodes inside the prefix plus ghost-port package nodes for cross-boundary deps." }
                    },
                    "required": [],
                    "additionalProperties": false
                }
            },
            {
                "name": "synonym_list",
                "description": "List all active query-synonym pairs (term => alias) from the per-repo dynamic synonym table. Stdio (single-repo) sessions only.",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }
            },
            {
                "name": "synonym_add",
                "description": "Add one alias for a term to the dynamic synonym table (200-row cap). Stdio (single-repo) sessions only.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "term": { "type": "string", "description": "The query term to expand" },
                        "alias": { "type": "string", "description": "An alias the term should also match" }
                    },
                    "required": ["term", "alias"],
                    "additionalProperties": false
                }
            },
            {
                "name": "synonym_set",
                "description": "Replace ALL aliases for a term atomically. Stdio (single-repo) sessions only.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "term": { "type": "string", "description": "The query term to expand" },
                        "aliases": { "type": "string", "description": "Comma-separated alias list. Replaces all existing aliases for the term." }
                    },
                    "required": ["term", "aliases"],
                    "additionalProperties": false
                }
            },
            {
                "name": "synonym_remove",
                "description": "Remove a single (term, alias) synonym pair. Stdio (single-repo) sessions only.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "term": { "type": "string", "description": "The query term" },
                        "alias": { "type": "string", "description": "The alias to remove from the term" }
                    },
                    "required": ["term", "alias"],
                    "additionalProperties": false
                }
            },
            {
                "name": "synonym_remove_term",
                "description": "Remove ALL aliases for a term. Stdio (single-repo) sessions only.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "term": { "type": "string", "description": "The term whose aliases should all be removed" }
                    },
                    "required": ["term"],
                    "additionalProperties": false
                }
            },
            {
                "name": "synonym_reset",
                "description": "Reset the dynamic synonym table to the built-in static defaults. Stdio (single-repo) sessions only.",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }
            },
            {
                "name": "repos_list",
                "description": "List globally registered repos as TSV (name, db_path, exists). Stdio (single-repo) sessions only.",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }
            },
            {
                "name": "repos_prune",
                "description": "Remove registry entries whose graph.db no longer exists. Stdio (single-repo) sessions only.",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }
            },
            {
                "name": "repos_remove",
                "description": "Remove a single repo from the registry by name. Stdio (single-repo) sessions only.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Registry key (repo name) to remove" }
                    },
                    "required": ["name"],
                    "additionalProperties": false
                }
            },
            {
                "name": "repo_languages",
                "description": "Return per-language node counts indexed in this repo as TSV (language, count), sorted by count descending.",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }
            },
            {
                "name": "get_snippets",
                "description": "Return tailored code snippets for one or more symbols by name. Accepts the symbol names returned by get_context, get_callers, and search_symbol. Kind-aware extraction: functions/methods → up to 40 lines; classes/structs/impls → up to 15 lines (header + fields only); interfaces/traits/enums → up to 60 lines. Leading docblocks are stripped. Respects a token budget — symbols are included in request order until the budget is reached. Use this after any graph-navigation tool to read the actual code without opening files.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "symbols": {
                            "type": "string",
                            "description": "Newline- or comma-separated list of symbol names (e.g. 'PaymentService.charge\\nMAX_RETRIES'). Partial matches accepted — the closest matching non-file symbol is used for each name."
                        },
                        "token_budget": {
                            "type": "integer",
                            "description": "Hard token cap across all returned snippets. Default: 2000. Higher values return more symbols."
                        },
                        "repo": {
                            "type": "string",
                            "description": "Restrict to a specific registered repo by name (global / multi-repo mode only)."
                        }
                    },
                    "required": ["symbols"],
                    "additionalProperties": false
                }
            }
        ]
    })
}

fn ok_response(id: serde_json::Value, result: serde_json::Value) -> String {
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

fn error_response(id: serde_json::Value, code: i32, message: String) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
    .to_string()
}

// ── Global mode (all registered repos) ───────────────────────────────────────

/// Run the MCP stdio server in global mode.
///
/// Serves all repos registered in `~/.travsr/registry.json`. The registry is
/// re-read on every `tools/call` so repos added via `travsr init` after startup
/// are picked up live without restarting the server.
pub fn run_global() -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    for line_result in BufReader::new(stdin.lock()).lines() {
        let line = line_result?;
        if line.trim().is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<RpcRequest>(&line) {
            Ok(req) => handle_request_global(req),
            Err(e) => {
                tracing::debug!("JSON parse error: {e}");
                Some(error_response(
                    serde_json::Value::Null,
                    PARSE_ERROR,
                    format!("parse error: {e}"),
                ))
            }
        };

        if let Some(resp) = response {
            writeln!(out, "{resp}")?;
            out.flush()?;
        }
    }
    Ok(())
}

fn handle_request_global(req: RpcRequest) -> Option<String> {
    let id = match req.id {
        Some(id) => id,
        None => {
            tracing::debug!("notification received: {}", req.method);
            return None;
        }
    };

    let resp = match req.method.as_str() {
        "initialize" => ok_response(
            id,
            serde_json::json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": server_capabilities(),
                "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION }
            }),
        ),
        "tools/list" => ok_response(id, tools_list_global()),
        "tools/call" => {
            let repos = registry::all_repos().unwrap_or_default();
            let params = req.params.unwrap_or(serde_json::Value::Null);
            handle_tool_call_global(&repos, id, params)
        }

        #[cfg(feature = "mcp-sampling")]
        "prompts/list" => ok_response(id, prompts_list()),

        #[cfg(feature = "mcp-sampling")]
        "prompts/get" => {
            let name = req
                .params
                .as_ref()
                .and_then(|p| p["name"].as_str())
                .unwrap_or("");
            handle_prompts_get(id, name)
        }

        other => {
            tracing::debug!("unknown method: {other}");
            error_response(id, METHOD_NOT_FOUND, format!("method not found: {other}"))
        }
    };

    Some(resp)
}

fn handle_tool_call_global(
    repos: &HashMap<String, PathBuf>,
    id: serde_json::Value,
    params: serde_json::Value,
) -> String {
    let tool_name = match params["name"].as_str() {
        Some(n) => n,
        None => return error_response(id, INVALID_PARAMS, "missing tool name".into()),
    };
    let args = &params["arguments"];
    let repo_arg = args["repo"].as_str();

    let _span = tracing::info_span!("mcp.tool_call", tool = tool_name, global = true).entered();
    let text = match tool_name {
        "get_dependencies" => {
            tools::get_dependencies_global(repos, args["file"].as_str().unwrap_or(""), repo_arg)
        }
        "get_callers" => {
            tools::get_callers_global(repos, args["symbol"].as_str().unwrap_or(""), repo_arg)
        }
        "get_blast_radius" => {
            let mode = match args["analysis"].as_str().unwrap_or("tree-sitter") {
                "semantic" => tools::AnalysisMode::Semantic,
                _ => tools::AnalysisMode::TreeSitter,
            };
            tools::get_blast_radius_global(
                repos,
                args["file"].as_str().unwrap_or(""),
                repo_arg,
                mode,
            )
        }
        "get_lang_status" => {
            tools::get_lang_status_global(repos, args["file"].as_str().unwrap_or(""), repo_arg)
        }
        "search_symbol" => {
            tools::search_symbol_global(repos, args["name"].as_str().unwrap_or(""), repo_arg)
        }
        "get_repo_map" => tools::get_repo_map_global(repos, repo_arg),
        "get_graph_stats" => tools::get_graph_stats_global(repos, repo_arg),
        "get_execution_path" => {
            let source = args["source"].as_str().unwrap_or("");
            let sink = args["sink"].as_str().unwrap_or("");
            tools::get_execution_path_global(repos, source, sink, repo_arg, &OpenFilter)
        }
        "get_context" => {
            let query = args["query"].as_str().unwrap_or("");
            let token_budget = args["token_budget"].as_u64().unwrap_or(4096) as usize;
            tools::get_context_global(repos, query, token_budget, repo_arg)
        }
        "get_graph_json" => {
            let query = args["query"].as_str().unwrap_or("");
            let direction = args["direction"].as_str().unwrap_or("both");
            let depth = args["depth"]
                .as_u64()
                .or_else(|| args["depth"].as_str().and_then(|s| s.parse::<u64>().ok()))
                .unwrap_or(2)
                .clamp(1, 4) as u8;
            let kind_filter = args["kind_filter"].as_str().unwrap_or("");
            let mode = args["mode"].as_str().unwrap_or("");
            let path_prefix = args["path_prefix"].as_str().unwrap_or("");
            tools::get_graph_json_global(
                repos,
                repo_arg,
                &tools::GraphJsonParams {
                    query,
                    direction,
                    depth,
                    kind_filter,
                    token_budget: 0,
                    mode,
                    path_prefix,
                },
            )
        }
        // Synonym tools mutate a single repo's table; ambiguous across the global
        // registry. Reject cleanly rather than silently no-op or fall through to
        // "unknown tool".
        "get_snippets" => {
            let symbols = args["symbols"].as_str().unwrap_or("");
            let token_budget = args["token_budget"]
                .as_u64()
                .unwrap_or(tools::SNIPPET_DEFAULT_BUDGET as u64)
                as usize;
            tools::get_snippets_global(repos, symbols, token_budget, repo_arg)
        }
        "synonym_add"
        | "synonym_set"
        | "synonym_remove"
        | "synonym_remove_term"
        | "synonym_list"
        | "synonym_reset" => {
            return error_response(
                id,
                INVALID_PARAMS,
                "synonym tools require a single-repo (stdio) session".into(),
            );
        }
        "repos_list" | "repos_prune" | "repos_remove" => {
            return error_response(
                id,
                INVALID_PARAMS,
                "repos tools require a single-repo (stdio) session".into(),
            );
        }
        other => return error_response(id, INVALID_PARAMS, format!("unknown tool: {other}")),
    };

    // tool_calls_total=1 is a log-based counter field for tracing subscribers.
    // TODO(travsr-060): replace with otel Counter metric for proper OTLP aggregation.
    tracing::info!(
        tool = tool_name,
        tool_calls_total = 1u64,
        "mcp.tool_call complete"
    );
    ok_response(
        id,
        serde_json::json!({ "content": [{ "type": "text", "text": text }] }),
    )
}

fn tools_list_global() -> serde_json::Value {
    serde_json::json!({
        "_schemaVersion": "1.1.0",
        "tools": [
            {
                "name": "get_dependencies",
                "description": "Return the files and modules that a given file imports.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "file": { "type": "string", "description": "Repo-relative file path to look up" },
                        "repo": { "type": "string", "description": "Repo name from `travsr repos`. Searches all repos if omitted." }
                    },
                    "required": ["file"],
                    "additionalProperties": false
                }
            },
            {
                "name": "get_callers",
                "description": "Return all graph nodes that have an incoming edge to the given symbol.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "symbol": { "type": "string", "description": "Symbol name to find callers of (partial match supported)" },
                        "repo": { "type": "string", "description": "Repo name from `travsr repos`. Searches all repos if omitted." }
                    },
                    "required": ["symbol"],
                    "additionalProperties": false
                }
            },
            {
                "name": "get_blast_radius",
                "description": "Return the set of files transitively affected if the given file changes.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "file": { "type": "string", "description": "Repo-relative file path to compute blast radius for" },
                        "analysis": { "type": "string", "enum": ["tree-sitter", "semantic"], "description": "Edge mode: 'tree-sitter' (default, structural) or 'semantic' (RefCall only, requires Phase B)." },
                        "repo": { "type": "string", "description": "Repo name from `travsr repos`. Searches all repos if omitted." }
                    },
                    "required": ["file"],
                    "additionalProperties": false
                }
            },
            {
                "name": "get_lang_status",
                "description": "Return whether semantic (Phase B) analysis is available for the language of the given file, and an install hint if not. Returns JSON.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "file": { "type": "string", "description": "Repo-relative file path to detect language for" },
                        "repo": { "type": "string", "description": "Repo name from `travsr repos`. Searches all repos if omitted." }
                    },
                    "required": ["file"],
                    "additionalProperties": false
                }
            },
            {
                "name": "search_symbol",
                "description": "Find symbol definitions matching a name across the indexed graph. Accepts exact symbol names, partial matches, and natural-language queries (e.g. 'auth session validation'). No model or API key required.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Symbol name or natural-language query (1–200 chars). Partial and NL queries are supported." },
                        "repo": { "type": "string", "description": "Repo name from `travsr repos`. Searches all repos if omitted." }
                    },
                    "required": ["name"],
                    "additionalProperties": false
                }
            },
            {
                "name": "get_repo_map",
                "description": "Return a structural overview of the indexed repository.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "repo": { "type": "string", "description": "Repo name from `travsr repos`. Uses current repo if omitted." }
                    },
                    "additionalProperties": false
                }
            },
            {
                "name": "get_execution_path",
                "description": "Find a traversal path from source symbol to sink symbol through the code graph using PCST.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "source": { "type": "string", "description": "Source symbol name (partial match supported)" },
                        "sink": { "type": "string", "description": "Sink symbol name (partial match supported)" },
                        "repo": { "type": "string", "description": "Repo name from `travsr repos`. Searches all repos if omitted." }
                    },
                    "required": ["source", "sink"],
                    "additionalProperties": false
                }
            },
            {
                "name": "get_graph_stats",
                "description": "Return accurate node and edge counts from the indexed graph.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "repo": { "type": "string", "description": "Repo name from `travsr repos`. Sums all repos if omitted." }
                    },
                    "additionalProperties": false
                }
            },
            {
                "name": "get_context",
                "description": "Retrieve the most relevant context for a query within a token budget using PPR + 0-1 knapsack. Accepts symbol names and natural-language queries (e.g. 'where is the auth session validated?'). T0 + L2-A heuristic normaliser translates NL to FTS seeds deterministically — no model or API key required.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Symbol name or natural-language query (1–200 chars)" },
                        "token_budget": { "type": "integer", "description": "Hard token budget (100–32000). Defaults to 4096 if omitted." },
                        "repo": { "type": "string", "description": "Repo name from `travsr repos`. Searches all repos if omitted." }
                    },
                    "required": ["query"],
                    "additionalProperties": false
                }
            },
            {
                "name": "get_graph_json",
                "description": "Return a subgraph around a symbol as structured JSON nodes and edges for graph renderers. Pass mode='overview' with no query for the repo-map LOD tile layout.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Symbol name or partial match (1–200 chars). May be empty when kind_filter is 'file' or mode is 'overview'." },
                        "direction": { "type": "string", "enum": ["deps", "callers", "both"], "description": "Edge direction. Default: both" },
                        "depth": { "type": "integer", "minimum": 1, "maximum": 4, "description": "BFS depth. Default: 2" },
                        "kind_filter": { "type": "string", "enum": ["file", ""], "description": "Restrict nodes to a specific kind. 'file' returns only file nodes and imports edges (project module map). Default: empty (all kinds)." },
                        "repo": { "type": "string", "description": "Repo name from `travsr repos`. Searches all repos if omitted." },
                        "mode": { "type": "string", "enum": ["", "overview"], "description": "'overview' returns synthetic package-level tile nodes sized by file count plus cross-package import edges." },
                        "path_prefix": { "type": "string", "description": "When mode='overview', scope to files under this path prefix. Returns file nodes inside the prefix plus ghost-port package nodes for cross-boundary deps." }
                    },
                    "required": [],
                    "additionalProperties": false
                }
            },
            {
                "name": "get_snippets",
                "description": "Return tailored code snippets for one or more symbols by name. Accepts the symbol names returned by get_context, get_callers, and search_symbol. Kind-aware extraction: functions/methods → up to 40 lines; classes/structs/impls → up to 15 lines (header + fields only); interfaces/traits/enums → up to 60 lines. Leading docblocks are stripped. Respects a token budget — symbols are included in request order until the budget is reached. Use this after any graph-navigation tool to read the actual code without opening files.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "symbols": {
                            "type": "string",
                            "description": "Newline- or comma-separated list of symbol names (e.g. 'PaymentService.charge\\nMAX_RETRIES'). Partial matches accepted — the closest matching non-file symbol is used for each name."
                        },
                        "token_budget": {
                            "type": "integer",
                            "description": "Hard token cap across all returned snippets. Default: 2000. Higher values return more symbols."
                        },
                        "repo": {
                            "type": "string",
                            "description": "Restrict to a specific registered repo by name."
                        }
                    },
                    "required": ["symbols"],
                    "additionalProperties": false
                }
            }
        ]
    })
}

// ── L2-D: MCP sampling borrow (RFC-012 A2 F3, feature = "mcp-sampling") ────────
// The daemon is passive: it returns a prompt template string with a {{query}}
// placeholder. The host LLM fills the placeholder and calls tools/call itself.
// The daemon never calls the LLM. Security review required before cloud deploy.

#[cfg(feature = "mcp-sampling")]
fn prompts_list() -> serde_json::Value {
    serde_json::json!({
        "prompts": [{
            "name": "search_query_rewrite",
            "description": "Rewrite a natural-language query into a concise \
                            symbol-oriented search term for Travsr's graph index.",
            "arguments": [{
                "name": "query",
                "description": "The original user query",
                "required": true
            }]
        }]
    })
}

#[cfg(feature = "mcp-sampling")]
fn handle_prompts_get(id: serde_json::Value, name: &str) -> String {
    match name {
        "search_query_rewrite" => ok_response(
            id,
            serde_json::json!({
                "description": "Rewrite this query for Travsr symbol search",
                "messages": [{
                    "role": "user",
                    "content": {
                        "type": "text",
                        "text": "Rewrite the following query into a short (1–4 word) \
                                 code-symbol search term that would appear in function \
                                 names, class names, or file names:\n\n{{query}}"
                    }
                }]
            }),
        ),
        _ => error_response(id, INVALID_PARAMS, format!("unknown prompt: {name}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_TOOLS: &[&str] = &[
        "get_dependencies",
        "get_callers",
        "get_blast_radius",
        "get_lang_status",
        "search_symbol",
        "get_repo_map",
        "get_execution_path",
        "get_graph_stats",
        "get_context",
        "get_graph_json",
        "get_snippets",
    ];

    /// Tools exposed only on the stdio (single-repo) server — never in the global
    /// registry path. Synonyms (RFC-012 A2 F1) target one repo's table; repos
    /// management (VSCODE-247) mutates the global registry and is meaningless to
    /// duplicate across the per-repo global fan-out.
    const STDIO_ONLY_TOOLS: &[&str] = &[
        "synonym_list",
        "synonym_add",
        "synonym_set",
        "synonym_remove",
        "synonym_remove_term",
        "synonym_reset",
        "repos_list",
        "repos_prune",
        "repos_remove",
    ];

    #[test]
    fn stdio_tools_list_contains_synonym_tools() {
        let list = tools_list();
        let tools = list["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        for expected in STDIO_ONLY_TOOLS {
            assert!(names.contains(expected), "tools/list missing: {expected}");
        }
    }

    #[test]
    fn global_tools_list_excludes_synonym_tools() {
        let list = tools_list_global();
        let tools = list["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        for forbidden in STDIO_ONLY_TOOLS {
            assert!(
                !names.contains(forbidden),
                "tools_list_global must not expose stdio-only tool: {forbidden}"
            );
        }
    }

    #[test]
    fn synonym_tools_require_correct_fields() {
        let list = tools_list();
        let tools = list["tools"].as_array().unwrap();
        let required_map = [
            ("synonym_add", &["term", "alias"][..]),
            ("synonym_set", &["term", "aliases"][..]),
            ("synonym_remove", &["term", "alias"][..]),
            ("synonym_remove_term", &["term"][..]),
            ("repos_remove", &["name"][..]),
        ];
        for (tool_name, fields) in required_map {
            let tool = tools
                .iter()
                .find(|t| t["name"].as_str() == Some(tool_name))
                .unwrap_or_else(|| panic!("tool '{tool_name}' not found"));
            let required = tool["inputSchema"]["required"].as_array().unwrap();
            for field in fields {
                assert!(
                    required.iter().any(|r| r.as_str() == Some(field)),
                    "tool '{tool_name}' must have '{field}' in required"
                );
            }
        }
    }

    #[test]
    fn tools_list_contains_all_tools() {
        let list = tools_list();
        let tools = list["tools"].as_array().expect("tools must be an array");
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        for expected in ALL_TOOLS {
            assert!(names.contains(expected), "tools/list missing: {expected}");
        }
    }

    #[test]
    fn tools_list_global_contains_all_tools() {
        let list = tools_list_global();
        let tools = list["tools"].as_array().expect("tools must be an array");
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        for expected in ALL_TOOLS {
            assert!(
                names.contains(expected),
                "tools_list_global missing: {expected}"
            );
        }
    }

    #[test]
    fn every_tool_has_input_schema_with_type_object() {
        for list in [tools_list(), tools_list_global()] {
            let tools = list["tools"].as_array().unwrap();
            for tool in tools {
                let name = tool["name"].as_str().unwrap();
                let schema = &tool["inputSchema"];
                assert_eq!(
                    schema["type"].as_str(),
                    Some("object"),
                    "tool '{name}' inputSchema must have type=object"
                );
                assert!(
                    schema["properties"].is_object(),
                    "tool '{name}' inputSchema must have a properties object"
                );
            }
        }
    }

    #[test]
    fn required_tools_have_correct_required_fields() {
        // get_graph_json is intentionally excluded: query is optional when mode="overview".
        let required_map = [
            ("get_dependencies", "file"),
            ("get_callers", "symbol"),
            ("get_blast_radius", "file"),
            ("search_symbol", "name"),
            ("get_execution_path", "source"),
            ("get_context", "query"),
        ];
        for list in [tools_list(), tools_list_global()] {
            let tools = list["tools"].as_array().unwrap();
            for (tool_name, required_field) in required_map {
                let tool = tools
                    .iter()
                    .find(|t| t["name"].as_str() == Some(tool_name))
                    .unwrap_or_else(|| panic!("tool '{tool_name}' not found"));
                let required = tool["inputSchema"]["required"]
                    .as_array()
                    .unwrap_or_else(|| panic!("tool '{tool_name}' must have required array"));
                assert!(
                    required.iter().any(|r| r.as_str() == Some(required_field)),
                    "tool '{tool_name}' must have '{required_field}' in required"
                );
            }
        }
    }

    #[test]
    fn schema_version_is_semver() {
        for list in [tools_list(), tools_list_global()] {
            let version = list["_schemaVersion"]
                .as_str()
                .expect("_schemaVersion must be present");
            let parts: Vec<&str> = version.split('.').collect();
            assert_eq!(
                parts.len(),
                3,
                "_schemaVersion must be X.Y.Z, got: {version}"
            );
            for part in &parts {
                part.parse::<u32>()
                    .unwrap_or_else(|_| panic!("semver part must be numeric: {part}"));
            }
        }
    }

    #[test]
    fn all_schemas_have_additional_properties_false() {
        for list in [tools_list(), tools_list_global()] {
            let tools = list["tools"].as_array().unwrap();
            for tool in tools {
                let name = tool["name"].as_str().unwrap();
                assert_eq!(
                    tool["inputSchema"]["additionalProperties"].as_bool(),
                    Some(false),
                    "tool '{name}' inputSchema must set additionalProperties=false"
                );
            }
        }
    }

    #[test]
    fn initialize_advertises_capabilities() {
        let caps = server_capabilities();
        assert!(
            caps["tools"].is_object(),
            "initialize must always advertise the tools capability"
        );
        // The prompts capability is present iff mcp-sampling is compiled in.
        assert_eq!(
            caps["prompts"].is_object(),
            cfg!(feature = "mcp-sampling"),
            "prompts capability must be advertised exactly when mcp-sampling is enabled"
        );
    }

    #[cfg(feature = "mcp-sampling")]
    #[test]
    fn prompts_list_exposes_search_query_rewrite() {
        let list = prompts_list();
        let prompts = list["prompts"].as_array().expect("prompts must be array");
        let names: Vec<&str> = prompts
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect();
        assert!(
            names.contains(&"search_query_rewrite"),
            "prompts/list must expose search_query_rewrite"
        );
    }

    #[cfg(feature = "mcp-sampling")]
    #[test]
    fn prompts_get_returns_template_with_placeholder() {
        let resp = handle_prompts_get(serde_json::json!(1), "search_query_rewrite");
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        let text = v["result"]["messages"][0]["content"]["text"]
            .as_str()
            .expect("template text must be present");
        assert!(
            text.contains("{{query}}"),
            "prompt template must contain the {{{{query}}}} placeholder"
        );
        // Passive contract: the daemon returns a template, never an LLM call result.
        assert!(v["error"].is_null(), "valid prompt must not error");
    }

    #[cfg(feature = "mcp-sampling")]
    #[test]
    fn prompts_get_unknown_name_is_invalid_params() {
        let resp = handle_prompts_get(serde_json::json!(2), "does_not_exist");
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(
            v["error"]["code"].as_i64(),
            Some(INVALID_PARAMS as i64),
            "unknown prompt name must return INVALID_PARAMS"
        );
    }

    #[cfg(feature = "mcp-sampling")]
    #[test]
    fn global_handler_routes_prompts_list() {
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::json!(7)),
            method: "prompts/list".into(),
            params: None,
        };
        let resp = handle_request_global(req).expect("request must produce a response");
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert!(
            v["result"]["prompts"].is_array(),
            "prompts/list must route through the global handler when mcp-sampling is on"
        );
    }
}
