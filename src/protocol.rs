//! Mirror of `deerpunk-inspector-protocol` (JSON-RPC 2.0 over WebSocket):
//! handshake, collection descriptors and snapshots. Server is the source of
//! truth for the collection set; unknown result shapes stay generic JSON.

use serde::Deserialize;
use serde_json::Value;

pub const PROTO_VERSION: i64 = 1;
pub const RPC_INITIALIZE: &str = "inspector.initialize";
pub const RPC_INITIALIZED: &str = "inspector.initialized";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CollectionMode {
    List,
    Get,
    Stream,
    Catalog,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CollectionGroup {
    Runtime,
    Definitions,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectionDescriptor {
    pub id: String,
    pub label: String,
    pub group: CollectionGroup,
    pub modes: Vec<CollectionMode>,
    pub id_field: String,
    #[serde(default)]
    pub nested_in: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub proto: i64,
    pub server: String,
    #[serde(default)]
    pub server_version: Option<String>,
    #[serde(default)]
    pub collections: Vec<CollectionDescriptor>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ComponentSnapshot {
    Tag {
        slug: String,
        kind: String,
    },
    Data {
        slug: String,
        #[serde(default)]
        schema: Option<Value>,
        data: Value,
    },
    /// Unregistered component: the server sends `{"slug": …,
    /// "_unregistered": true}` (no properties). Rendered as a muted card.
    /// Placed last: untagged matching must not swallow Data payloads.
    Unregistered {
        #[serde(default)]
        slug: Option<String>,
    },
}

impl ComponentSnapshot {
    /// Component name for card titles (slug may be absent on odd payloads).
    pub fn slug(&self) -> &str {
        match self {
            ComponentSnapshot::Tag { slug, .. } => slug,
            ComponentSnapshot::Data { slug, .. } => slug,
            ComponentSnapshot::Unregistered { slug } => slug.as_deref().unwrap_or("(unregistered)"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct EntityGetResult {
    pub index: u64,
    pub name: String,
    /// Prefab classname from the network-entity component (optional, legacy
    /// contract; newer hosts send `prefabId` + a `prefabs` collection).
    #[serde(default)]
    pub prefab: Option<String>,
    #[serde(rename = "prefabId", default)]
    pub prefab_id: Option<u32>,
    #[serde(default)]
    pub components: Vec<ComponentSnapshot>,
}

/// Row of the `prefabs` collection (the prefabMap): `{"id": u32,
/// "classname": "…"}`.
pub fn prefab_row(row: &Value) -> Option<(u32, String)> {
    let id = row.get("id").and_then(Value::as_u64)? as u32;
    let classname = row.get("classname").and_then(Value::as_str)?;
    Some((id, classname.to_string()))
}

/// Display name for an entity: prefabMap classname for `prefabId`, else the
/// legacy string `prefab`, else the raw entity `name`.
pub fn resolve_entity_name(
    prefab_id: Option<u32>,
    prefab: Option<&str>,
    name: &str,
    map: &std::collections::HashMap<u32, String>,
) -> String {
    prefab_id
        .and_then(|id| map.get(&id))
        .cloned()
        .or_else(|| prefab.map(str::to_string))
        .unwrap_or_else(|| name.to_string())
}

/// Optional `prefabId` key on a list row (new contract; older hosts send the
/// string `prefab` instead — support both on receive).
pub fn row_prefab_id(row: &Value) -> Option<u32> {
    row.get("prefabId")
        .and_then(Value::as_u64)
        .map(|v| v as u32)
}

/// Optional `prefab` string key on a list row (legacy contract).
pub fn row_prefab(row: &Value) -> Option<String> {
    row.get("prefab")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Optional `position` key on a list row: `[x, y, z]` numbers.
pub fn row_position(row: &Value) -> Option<[f32; 3]> {
    let arr = row.get("position").and_then(Value::as_array)?;
    if arr.len() != 3 {
        return None;
    }
    let mut out = [0.0f32; 3];
    for (i, v) in arr.iter().enumerate() {
        out[i] = v.as_f64()? as f32;
    }
    Some(out)
}

/// Compact "x y z" rendering, rounding near-integers (`4.5 → 4`, `4.25 → 4.25`).
pub fn format_position(p: &[f32; 3]) -> String {
    p.iter()
        .map(|v| {
            let r = v.round();
            if (*v - r).abs() < 0.001 {
                format!("{r}")
            } else {
                format!("{v}")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Outgoing JSON-RPC 2.0 request/notification.
pub fn request(id: u64, method: &str, params: Value) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
    .to_string()
}

pub fn notification(method: &str, params: Value) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    })
    .to_string()
}

/// Parsed incoming message: a reply, an error reply, or noise.
pub enum Incoming {
    Reply {
        id: u64,
        result: Value,
    },
    RpcError {
        id: Option<u64>,
        code: i64,
        message: String,
    },
}

pub fn parse_incoming(text: &str) -> Option<Incoming> {
    let msg: Value = serde_json::from_str(text).ok()?;
    let id = msg.get("id").and_then(Value::as_u64);
    if let Some(err) = msg.get("error") {
        return Some(Incoming::RpcError {
            id,
            code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
            message: err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("error")
                .to_string(),
        });
    }
    if msg.get("result").is_some() {
        return Some(Incoming::Reply {
            id: id?,
            result: msg["result"].clone(),
        });
    }
    None
}

/// Row extraction from list results: every collection replies with
/// `{time, count, <key>: [rows]}`; systems replies with a bare snapshot
/// object. Returns the rows of the first non-meta array field.
pub fn list_rows(result: &Value) -> Vec<Value> {
    result
        .as_object()
        .map(|o| {
            o.iter()
                .filter(|(k, v)| k.as_str() != "time" && v.is_array())
                .max_by_key(|(k, _)| o.keys().count() - o.keys().position(|x| x == *k).unwrap_or(0))
                .and_then(|(_, v)| v.as_array().cloned())
                .unwrap_or_default()
        })
        .unwrap_or_default()
}

pub fn list_has_rows(result: &Value) -> bool {
    list_rows(result).is_empty() && !result.is_null()
}

/// Stringified scalar for a table cell; arrays joined.
pub fn cell(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
        Value::Array(a) if a.len() <= 6 => a
            .iter()
            .map(|x| {
                x.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| x.to_string())
            })
            .collect::<Vec<_>>()
            .join(", "),
        other => other.to_string(),
    }
}

/// Column keys for a row set: stable order with `index`, `name` first.
pub fn row_keys(rows: &[Value]) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for row in rows.iter().take(64) {
        if let Some(o) = row.as_object() {
            for k in o.keys() {
                if !keys.contains(k) {
                    keys.push(k.clone());
                }
            }
        }
    }
    keys.sort_by_key(|k| match k.as_str() {
        "index" => 0,
        "name" => 1,
        _ => 2,
    });
    keys
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_initialize_and_snapshots() {
        let text = r#"{"jsonrpc":"2.0","id":1,"result":{"proto":1,"server":"deerpunk","collections":[{"id":"entities","label":"Entities","group":"runtime","modes":["list","get"],"idField":"index"}]}}"#;
        let incoming = parse_incoming(text).unwrap();
        let Incoming::Reply { id, result } = incoming else {
            panic!("expected reply");
        };
        assert_eq!(id, 1);
        let init: InitializeResult = serde_json::from_value(result).unwrap();
        assert_eq!(init.server, "deerpunk");
        assert_eq!(init.collections.len(), 1);
        assert_eq!(
            init.collections[0].modes,
            vec![CollectionMode::List, CollectionMode::Get]
        );
    }

    #[test]
    fn entity_row_optional_keys() {
        let new_row =
            json!({"index": 1, "name": "player", "prefab": "player", "position": [1.0, 2.5, -3.0]});
        assert_eq!(row_prefab(&new_row).as_deref(), Some("player"));
        assert_eq!(row_position(&new_row), Some([1.0, 2.5, -3.0]));
        assert_eq!(format_position(&[1.0, 2.5, -3.0]), "1 2.5 -3");

        let old_row = json!({"index": 2, "name": "mob"});
        assert_eq!(row_prefab(&old_row), None);
        assert_eq!(row_position(&old_row), None);

        let bad = json!({"index": 3, "name": "x", "position": [1.0, 2.0]});
        assert_eq!(row_position(&bad), None);
        let empty_prefab = json!({"index": 4, "name": "x", "prefab": ""});
        assert_eq!(row_prefab(&empty_prefab), None);
    }

    #[test]
    fn entity_row_prefab_id_key() {
        let row = json!({"index": 1, "name": "mob", "prefabId": 7});
        assert_eq!(row_prefab_id(&row), Some(7));
        assert_eq!(row_prefab(&row), None);
        assert_eq!(row_prefab_id(&json!({"index": 2, "name": "x"})), None);
    }

    #[test]
    fn prefab_map_rows_parse() {
        let rows = list_rows(&json!({"time": 0.0, "count": 2, "prefabs": [
            {"id": 1, "classname": "player"},
            {"id": 2, "classname": "item_drop"}
        ]}));
        let map: std::collections::HashMap<u32, String> =
            rows.iter().filter_map(prefab_row).collect();
        assert_eq!(map.get(&1).map(String::as_str), Some("player"));
        assert_eq!(map.get(&2).map(String::as_str), Some("item_drop"));
        assert!(prefab_row(&json!({"id": 3})).is_none());
        assert!(prefab_row(&json!({"classname": "x"})).is_none());
    }

    #[test]
    fn resolve_entity_name_fallbacks() {
        let mut map = std::collections::HashMap::new();
        map.insert(1u32, "player".to_string());
        assert_eq!(
            resolve_entity_name(Some(1), Some("legacy"), "raw", &map),
            "player"
        );
        // Id not in the map: falls back to the legacy string, then raw name.
        assert_eq!(
            resolve_entity_name(Some(9), Some("legacy"), "raw", &map),
            "legacy"
        );
        assert_eq!(resolve_entity_name(Some(9), None, "raw", &map), "raw");
        assert_eq!(resolve_entity_name(None, None, "raw", &map), "raw");
        // Legacy-only row ignores the map.
        assert_eq!(
            resolve_entity_name(None, Some("legacy"), "raw", &map),
            "legacy"
        );
    }

    #[test]
    fn unregistered_component_parses_and_keeps_data() {
        let unreg: ComponentSnapshot =
            serde_json::from_value(json!({"slug": "SecretComp", "_unregistered": true})).unwrap();
        assert!(matches!(unreg, ComponentSnapshot::Unregistered { .. }));
        assert_eq!(unreg.slug(), "SecretComp");
        // Data payloads must still match the Data variant, not fall through.
        let data: ComponentSnapshot = serde_json::from_value(json!({
            "slug": "TransformComponent", "data": {"position": [0, 0, 0]}
        }))
        .unwrap();
        assert!(matches!(data, ComponentSnapshot::Data { .. }));
    }

    #[test]
    fn entity_get_result_prefab_optional() {
        let with: EntityGetResult = serde_json::from_value(json!({
            "index": 1, "name": "p", "prefab": "player",
            "components": [{"slug": "T", "data": {}}]
        }))
        .unwrap();
        assert_eq!(with.prefab.as_deref(), Some("player"));
        let without: EntityGetResult =
            serde_json::from_value(json!({"index": 2, "name": "q", "components": []})).unwrap();
        assert_eq!(without.prefab, None);
    }

    #[test]
    fn untagged_snapshot_tag_and_data() {
        let tag: ComponentSnapshot =
            serde_json::from_value(json!({"slug":"Player","kind":"tag"})).unwrap();
        assert!(matches!(tag, ComponentSnapshot::Tag { .. }));
        let data: ComponentSnapshot = serde_json::from_value(
            json!({"slug":"TransformComponent","data":{"position":[0,0,0]}}),
        )
        .unwrap();
        match data {
            ComponentSnapshot::Data { slug, data, .. } => {
                assert_eq!(slug, "TransformComponent");
                assert_eq!(data["position"][0], 0);
            }
            _ => panic!("expected data"),
        }
    }

    #[test]
    fn list_rows_extracts_first_array() {
        let r = json!({"time":1.5,"count":2,"entities":[{"index":1,"name":"a"},{"index":2,"name":"b"}]});
        let rows = list_rows(&r);
        assert_eq!(rows.len(), 2);
        assert_eq!(cell(&rows[0]["name"]), "a");
        let keys = row_keys(&rows);
        assert_eq!(keys, vec!["index", "name"]);
    }

    #[test]
    fn snapshot_object_has_no_rows() {
        let r = json!({"time":1.0,"fps":60.0,"stats":{"x":1}});
        assert!(list_rows(&r).is_empty());
    }

    #[test]
    fn request_is_jsonrpc() {
        let s = request(7, "entities.list", json!({}));
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 7);
        assert_eq!(v["method"], "entities.list");
    }
}
