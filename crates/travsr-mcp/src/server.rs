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

pub fn run(store: &SqliteStore) -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    for line_result in BufReader::new(stdin.lock()).lines() {
        let line = line_result?;
        if line.trim().is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<RpcRequest>(&line) {
            Ok(req) => handle_request(store, req),
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

fn handle_request(store: &SqliteStore, req: RpcRequest) -> Option<String> {
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
                "capabilities": { "tools": {} },
                "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION }
            }),
        ),

        "tools/list" => ok_response(id, tools_list()),

        "tools/call" => {
            let params = req.params.unwrap_or(serde_json::Value::Null);
            handle_tool_call(store, id, params)
        }

        other => {
            tracing::debug!("unknown method: {other}");
            error_response(id, METHOD_NOT_FOUND, format!("method not found: {other}"))
        }
    };

    Some(resp)
}

fn handle_tool_call(
    store: &SqliteStore,
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
            tools::get_dependencies(store, file)
        }
        "get_callers" => {
            let symbol = args["symbol"].as_str().unwrap_or("");
            tools::get_callers(store, symbol)
        }
        "get_blast_radius" => {
            let file = args["file"].as_str().unwrap_or("");
            tools::get_blast_radius(store, file)
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
            let depth = args["depth"].as_u64().unwrap_or(2).clamp(1, 4) as u8;
            let kind_filter = args["kind_filter"].as_str().unwrap_or("");
            tools::get_graph_json(store, query, direction, depth, kind_filter)
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
                "description": "Return the files and modules that a given file imports.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "file": { "type": "string", "description": "Repo-relative file path to look up dependencies for" }
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
                        "file": { "type": "string", "description": "Repo-relative file path to compute blast radius for" }
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
                "description": "Return a subgraph around a symbol as structured JSON nodes and edges for graph renderers.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Symbol name or partial match (1–200 chars). May be empty when kind_filter is 'file'." },
                        "direction": { "type": "string", "enum": ["deps", "callers", "both"], "description": "Edge direction. Default: both" },
                        "depth": { "type": "integer", "minimum": 1, "maximum": 4, "description": "BFS depth. Default: 2" },
                        "kind_filter": { "type": "string", "enum": ["file", ""], "description": "Restrict nodes to a specific kind. 'file' returns only file nodes and imports edges (project module map). Default: empty (all kinds)." }
                    },
                    "required": ["query"],
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
                "capabilities": { "tools": {} },
                "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION }
            }),
        ),
        "tools/list" => ok_response(id, tools_list_global()),
        "tools/call" => {
            let repos = registry::all_repos().unwrap_or_default();
            let params = req.params.unwrap_or(serde_json::Value::Null);
            handle_tool_call_global(&repos, id, params)
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
            tools::get_blast_radius_global(repos, args["file"].as_str().unwrap_or(""), repo_arg)
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
            let depth = args["depth"].as_u64().unwrap_or(2).clamp(1, 4) as u8;
            let kind_filter = args["kind_filter"].as_str().unwrap_or("");
            tools::get_graph_json_global(repos, query, direction, depth, repo_arg, kind_filter)
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
                "description": "Return a subgraph around a symbol as structured JSON nodes and edges for graph renderers.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Symbol name or partial match (1–200 chars). May be empty when kind_filter is 'file'." },
                        "direction": { "type": "string", "enum": ["deps", "callers", "both"], "description": "Edge direction. Default: both" },
                        "depth": { "type": "integer", "minimum": 1, "maximum": 4, "description": "BFS depth. Default: 2" },
                        "kind_filter": { "type": "string", "enum": ["file", ""], "description": "Restrict nodes to a specific kind. 'file' returns only file nodes and imports edges (project module map). Default: empty (all kinds)." },
                        "repo": { "type": "string", "description": "Repo name from `travsr repos`. Searches all repos if omitted." }
                    },
                    "required": ["query"],
                    "additionalProperties": false
                }
            }
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_TOOLS: &[&str] = &[
        "get_dependencies",
        "get_callers",
        "get_blast_radius",
        "search_symbol",
        "get_repo_map",
        "get_execution_path",
        "get_graph_stats",
        "get_context",
        "get_graph_json",
    ];

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
        let required_map = [
            ("get_dependencies", "file"),
            ("get_callers", "symbol"),
            ("get_blast_radius", "file"),
            ("search_symbol", "name"),
            ("get_execution_path", "source"),
            ("get_context", "query"),
            ("get_graph_json", "query"),
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
}
