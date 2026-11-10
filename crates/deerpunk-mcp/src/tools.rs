//! MCP tool surface: definitions and handlers that proxy inspector RPCs.
//!
//! The engine inspector protocol (v1) exposes `inspector.initialize` and
//! `<collection>.list` / `<collection>.get`; it has no cvar RPCs, so no cvar
//! tools are offered here.

use serde_json::{json, Value};

use crate::inspector::Inspector;

const ITEM_CAP: usize = 200;

pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
}

pub fn tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "inspector_status",
            description: "Connection status of the deerpunk engine inspector and its session info.",
            input_schema: json!({"type": "object", "properties": {}}),
        },
        ToolDef {
            name: "collections_list",
            description: "List collection descriptors exposed by the engine inspector.",
            input_schema: json!({"type": "object", "properties": {}}),
        },
        ToolDef {
            name: "collection_items",
            description:
                "List rows of a collection (e.g. `entities`). Result is capped at 200 rows.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "collection": {"type": "string", "description": "Collection id from collections_list"},
                    "limit": {"type": "integer", "description": "Max rows to return (1..=200)"}
                },
                "required": ["collection"]
            }),
        },
    ]
}

/// Run a tool; `Err` means the engine/inspector is unreachable.
pub fn run_tool(inspector: &Inspector, name: &str, args: &Value) -> Result<Value, String> {
    match name {
        "inspector_status" => {
            // Probe so status reflects liveness rather than the last use.
            let _ = inspector.call("inspector.initialize", json!({"proto": 1}));
            let session = inspector.session();
            Ok(json!({
                "connected": inspector.is_connected(),
                "addr": inspector.addr(),
                "server": session.as_ref().map(|s| s.server.as_str()).unwrap_or(""),
                "server_version": session.as_ref().and_then(|s| s.server_version.clone()),
                "protocol": 1,
            }))
        }
        "collections_list" => {
            let result = inspector.call("inspector.initialize", json!({"proto": 1}))?;
            let mut collections = result.get("collections").cloned().unwrap_or(Value::Null);
            if collections.is_null() {
                collections = Value::Array(vec![]);
            }
            Ok(json!({
                "server": result.get("server").cloned().unwrap_or(Value::Null),
                "server_version": result.get("serverVersion").cloned().unwrap_or(Value::Null),
                "collections": collections,
            }))
        }
        "collection_items" => {
            let Some(col) = args.get("collection").and_then(Value::as_str) else {
                return Err("missing required argument: collection".into());
            };
            let limit = match args.get("limit").and_then(Value::as_u64) {
                Some(n) => (n as usize).clamp(1, ITEM_CAP),
                None => ITEM_CAP,
            };
            let result = inspector.call(&format!("{col}.list"), json!({}))?;
            // Every list collection replies with `{time, count, <key>: [rows]}`;
            // rows live in the first array field other than the meta entries.
            let rows = result
                .as_object()
                .and_then(|obj| {
                    obj.iter()
                        .find(|(k, v)| v.is_array() && *k != "time" && *k != "count")
                        .map(|(_, v)| v)
                })
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let count = result
                .get("count")
                .and_then(Value::as_u64)
                .unwrap_or(rows.len() as u64);
            Ok(json!({
                "collection": col,
                "total_count": count,
                "returned": rows.len().min(limit),
                "items": rows.into_iter().take(limit).collect::<Vec<_>>(),
            }))
        }
        other => Err(format!("unknown tool: {other}")),
    }
}
