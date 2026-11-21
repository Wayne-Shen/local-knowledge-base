use crate::Api;
use anyhow::Result;
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};

const LEGACY_VERSION: &str = "2025-11-25";
const PROTOCOL_VERSION: &str = "2026-07-28";

pub fn run(api: &Api) -> Result<()> {
    let stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    let mut initialized = false;
    for line in stdin.lines() {
        let line = line?;
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(request) => dispatch(api, request, &mut initialized),
            Err(_) => Some(error(Value::Null, -32700, "Parse error")),
        };
        if let Some(response) = response {
            serde_json::to_writer(&mut stdout, &response)?;
            stdout.write_all(b"\n")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

fn dispatch(api: &Api, request: Value, initialized: &mut bool) -> Option<Value> {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request["method"].as_str();
    if request["jsonrpc"] != "2.0"
        || method.is_none()
        || !(id.is_null() || id.is_number() || id.is_string())
    {
        return Some(error(Value::Null, -32600, "Invalid request"));
    }
    request.get("id")?;
    let method = method.unwrap();
    let params = &request["params"];
    let version = params["_meta"]["io.modelcontextprotocol/protocolVersion"].as_str();
    let modern = version == Some(PROTOCOL_VERSION);
    if let Some(version) = version
        && version != PROTOCOL_VERSION
        && method != "server/discover"
    {
        return Some(json!({"jsonrpc": "2.0", "id": id, "error": {
            "code": -32022, "message": "Unsupported protocol version",
            "data": {"supported": [PROTOCOL_VERSION, LEGACY_VERSION], "requested": version}
        }}));
    }
    let mut result = match method {
        "server/discover" => {
            json!({"supportedVersions": [PROTOCOL_VERSION], "capabilities": {"tools": {}}})
        }
        "initialize" => {
            *initialized = true;
            let version = match params["protocolVersion"].as_str() {
                Some(version @ ("2025-03-26" | "2025-06-18" | LEGACY_VERSION)) => version,
                _ => LEGACY_VERSION,
            };
            json!({
                "protocolVersion": version,
                "capabilities": {"tools": {}},
                "serverInfo": server_info(),
            })
        }
        "ping" => json!({}),
        _ if !*initialized && !modern => {
            return Some(error(id, -32002, "Initialize the MCP session first"));
        }
        "tools/list" => tools(),
        "tools/call" => match params["name"].as_str() {
            Some("kb_search") | Some("kb_ask") => {
                let endpoint = if params["name"] == "kb_search" {
                    "search"
                } else {
                    "ask"
                };
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                if !args.is_object() {
                    return Some(error(id, -32602, "Tool arguments must be an object"));
                }
                match api.call(endpoint, Some(&args)) {
                    Ok(value) => json!({
                        "content": [{"type": "text", "text": value.to_string()}],
                        "structuredContent": value,
                    }),
                    Err(error) => json!({
                        "isError": true,
                        "content": [{"type": "text", "text": format!("{error:#}")}],
                    }),
                }
            }
            _ => return Some(error(id, -32602, "Unknown tool")),
        },
        _ => return Some(error(id, -32601, "Method not found")),
    };
    if modern || method == "server/discover" {
        result["resultType"] = json!("complete");
        result["_meta"] = json!({"io.modelcontextprotocol/serverInfo": server_info()});
    }
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn server_info() -> Value {
    json!({"name": "local-knowledge-base", "version": env!("CARGO_PKG_VERSION")})
}

fn tools() -> Value {
    json!({"tools": [
        {
            "name": "kb_search",
            "description": "Search the local archive for relevant passages and original file links.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 30},
                    "keyword_only": {"type": "boolean"}
                },
                "required": ["query"],
                "additionalProperties": false
            },
            "annotations": {"readOnlyHint": true}
        },
        {
            "name": "kb_ask",
            "description": "Answer a question with sources using the knowledge base's configured model routing.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "question": {"type": "string"},
                    "keyword_only": {"type": "boolean"}
                },
                "required": ["question"],
                "additionalProperties": false
            }
        }
    ]})
}

fn error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}
