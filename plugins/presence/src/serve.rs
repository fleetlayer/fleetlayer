//! `fleetlayer-presence serve --caller <who>`: the MCP server the authority's node shares as the `presence` tool, one
//! process per connection. Each tool call becomes one request on the authority's socket, with the caller the control
//! plane recorded for the connection (never anything the caller says).
//!
//! Tools: `get {person?}`, `set {person, state, for?, until?, note?}`, `plan_add {person, from, to, note?}`,
//! `plan_remove {person, id}` and `seen {person, via, device?}`. Name `person` in `call_targets` so the access policy
//! can say who may change whose status (`tool:presence/set/<person>@<node>`).

use std::path::PathBuf;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::authority::ask;

pub const TOOLS: &[&str] = &["get", "set", "plan_add", "plan_remove", "seen"];

fn tools() -> Value {
    let person = json!({"type": "string", "description": "The person's handle (`get` lists them)."});
    json!({"tools": [
        {
            "name": "get",
            "description": "Whether the person is present (at their desk), semi_present (looking at their phone), away or \
                asleep, since when and why, with their planned absences. Check before background work nobody may read.",
            "inputSchema": {"type": "object", "properties": {"person": person}}
        },
        {
            "name": "set",
            "description": "Record what the person told you: away (\"going out for lunch\", for or until a time), asleep, \
                or back. Only when the person said so.",
            "inputSchema": {"type": "object", "properties": {
                "person": person,
                "state": {"type": "string", "enum": ["away", "asleep", "back"]},
                "for": {"type": "string", "description": "How long: 30m, 1h, 2h30m."},
                "until": {"type": "string", "description": "Until when, their time: 14:00, 2026-12-20 14:00."},
                "note": {"type": "string", "description": "What they said, in a few words: lunch, school run."}
            }, "required": ["person", "state"]}
        },
        {
            "name": "plan_add",
            "description": "Record a planned absence the person told you about (a holiday): from and to, their time. A \
                bare date for `to` includes that day.",
            "inputSchema": {"type": "object", "properties": {
                "person": person,
                "from": {"type": "string", "description": "2026-12-20, or 2026-12-20 14:00."},
                "to": {"type": "string", "description": "2027-01-05, or 2027-01-05 18:00."},
                "note": {"type": "string"}
            }, "required": ["person", "from", "to"]}
        },
        {
            "name": "plan_remove",
            "description": "Remove a planned absence by its number.",
            "inputSchema": {"type": "object", "properties": {"person": person, "id": {"type": "integer"}},
                "required": ["person", "id"]}
        },
        {
            "name": "seen",
            "description": "Say the person just looked at a page (for a service that shows them pages: a deck, a relay).",
            "inputSchema": {"type": "object", "properties": {
                "person": person,
                "via": {"type": "string", "description": "What they looked at: quietdeck."},
                "device": {"type": "string", "enum": ["phone", "desk", "other"]}
            }, "required": ["person", "via"]}
        }
    ]})
}

/// Serve MCP on stdio until the caller closes it.
pub async fn run(socket: PathBuf, caller: String) -> anyhow::Result<()> {
    let mut out = tokio::io::stdout();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let method = msg["method"].as_str().unwrap_or("").to_string();
        let Some(id) = msg.get("id").cloned().filter(|_| !method.is_empty()) else { continue };
        let result = match method.as_str() {
            "initialize" => Ok(json!({
                "protocolVersion": msg["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "fleetlayer-presence", "version": env!("CARGO_PKG_VERSION")},
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(tools()),
            "tools/call" => {
                let name = msg["params"]["name"].as_str().unwrap_or("");
                let out = if TOOLS.contains(&name) {
                    ask(&socket, name, Some(&caller), msg["params"]["arguments"].clone()).await
                } else {
                    Err(anyhow::anyhow!("no tool called {name}"))
                };
                Ok(match out {
                    Ok(v) => json!({"content": [{"type": "text", "text": v.to_string()}], "structuredContent": v}),
                    Err(e) => json!({"content": [{"type": "text", "text": e.to_string()}], "isError": true}),
                })
            }
            other => Err(json!({"code": -32601, "message": format!("unknown method {other}")})),
        };
        let reply = match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e}),
        };
        if out.write_all((reply.to_string() + "\n").as_bytes()).await.is_err() || out.flush().await.is_err() {
            break;
        }
    }
    Ok(())
}
