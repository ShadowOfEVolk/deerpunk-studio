//! MCP JSON-RPC dispatch shared by the streamable-HTTP and stdio transports.
//! Handles `initialize`, `notifications/initialized`, `tools/list`, `tools/call`.

use serde_json::{json, Value};

use crate::inspector::Inspector;
use crate::tools::{run_tool, tool_defs};

pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
pub const SERVER_NAME: &str = "deerpunk-mcp";

/// Handle one JSON-RPC request; `None` for notifications (or to suppress output).
pub fn handle_request(inspector: &SharedInspector, req: &Value) -> Option<Value> {
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(json!({}));
    let resp = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
    let err_resp = |code: i64, message: &str| json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}});

    match method {
        "initialize" => Some(resp(json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {"tools": {}},
            "serverInfo": {
                "name": SERVER_NAME,
                "version": env!("CARGO_PKG_VERSION"),
            },
            "instructions": "MCP proxy into the running deerpunk engine inspector.",
        }))),
        "tools/list" => Some(resp(json!({
            "tools": tool_defs().iter().map(|t| json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": t.input_schema,
            })).collect::<Vec<_>>(),
        }))),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            // Inspector calls block; keep them off the async worker threads.
            let inspector = inspector.clone_handle();
            let result = std::thread::spawn(move || run_tool(inspector.get(), &name, &args))
                .join()
                .unwrap_or_else(|_| Err("tool worker panicked".to_string()));
            Some(resp(match result {
                Ok(v) => json!({
                    "content": [{"type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default()}],
                    "isError": false,
                }),
                Err(e) => {
                    // Clean JSON error object, per contract, for engine-down cases.
                    json!({
                        "content": [{"type": "text", "text": json!({"error": e}).to_string()}],
                        "isError": true,
                    })
                }
            }))
        }
        "ping" => Some(resp(json!({}))),
        _ if req.get("id").is_none() => None, // unknown notification: ignore
        _ => Some(err_resp(-32601, &format!("method not found: {method}"))),
    }
}

/// A cheap shared handle so the dispatcher can move ownership into a worker thread.
pub struct SharedInspector(std::sync::Arc<Inspector>);

impl Clone for SharedInspector {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl SharedInspector {
    pub fn new(inspector: Inspector) -> Self {
        Self(std::sync::Arc::new(inspector))
    }

    pub fn get(&self) -> &Inspector {
        &self.0
    }

    fn clone_handle(&self) -> Self {
        self.clone()
    }
}
