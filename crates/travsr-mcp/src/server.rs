//! MCP stdio event loop.
//!
//! Reads newline-delimited JSON-RPC 2.0 messages from stdin, dispatches them,
//! and writes responses to stdout. All diagnostics go to stderr via tracing
//! so stdout stays clean for the MCP client.

use std::io::{BufRead, BufReader, Write};

use crate::protocol::{INVALID_PARAMS, METHOD_NOT_FOUND, PARSE_ERROR};
use crate::tools;
use crate::{protocol::RpcRequest, PROTOCOL_VERSION, SERVER_NAME, SERVER_VERSION};
use travsr_store::SqliteStore;

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

    let text = match tool_name {
        "get_dependencies" => {
            let file = args["file"].as_str().unwrap_or("");
            tools::get_dependencies(store, file)
        }
        "get_callers" => {
            let symbol = args["symbol"].as_str().unwrap_or("");
            tools::get_callers(store, symbol)
        }
        other => {
            return error_response(id, INVALID_PARAMS, format!("unknown tool: {other}"));
        }
    };

    ok_response(
        id,
        serde_json::json!({ "content": [{ "type": "text", "text": text }] }),
    )
}

fn tools_list() -> serde_json::Value {
    serde_json::json!({
        "tools": [
            {
                "name": "get_dependencies",
                "description": "Return the files and modules that a given file imports.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "file": {
                            "type": "string",
                            "description": "File path to look up dependencies for"
                        }
                    },
                    "required": ["file"]
                }
            },
            {
                "name": "get_callers",
                "description": "Return all graph nodes that have an incoming edge to the given symbol.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "symbol": {
                            "type": "string",
                            "description": "Symbol name to find callers of"
                        }
                    },
                    "required": ["symbol"]
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
