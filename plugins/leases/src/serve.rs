//! `fleetlayer-leases serve --caller <who>`: the MCP server the authority's node shares as a tool, one process per
//! connection. Each tool call becomes one request on the authority's socket, with the caller the control plane
//! recorded for the connection (never anything the caller says), and the authority's progress lines become MCP
//! progress notifications. The connection closing ends what it kept (a wait, a hold).
//!
//! Tools: `acquire {kind, reason, payload?, wait?, ttl?, idle?, tie?, pid?}`, `release {id}`, `list {kind?, all?}`,
//! `show {id}`, `wait {id}` and `hold {id}`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

/// Send one request to the authority's socket; `on_progress` gets each progress line. The answer's `result`, or its
/// `error` as an error.
pub async fn ask(
    socket: &Path,
    op: &str,
    caller: Option<&str>,
    args: Value,
    mut on_progress: impl FnMut(&str),
) -> anyhow::Result<Value> {
    let stream = tokio::net::UnixStream::connect(socket).await.map_err(|e| {
        anyhow::anyhow!("can't reach the leases authority at {} ({e}): is its plugin running here?", socket.display())
    })?;
    let (r, mut w) = stream.into_split();
    let req = json!({"op": op, "caller": caller, "args": args});
    w.write_all((req.to_string() + "\n").as_bytes()).await?;
    let mut lines = BufReader::new(r).lines();
    // Keep the writing half open: closing it tells the authority this caller went away.
    let _keep = w;
    while let Some(line) = lines.next_line().await? {
        let v: Value = serde_json::from_str(&line)?;
        if let Some(p) = v["progress"].as_str() {
            on_progress(p);
            continue;
        }
        if let Some(e) = v["error"].as_str() {
            anyhow::bail!("{e}");
        }
        return Ok(v["result"].clone());
    }
    anyhow::bail!("the leases authority closed the connection without an answer")
}

fn tools() -> Value {
    let id = json!({"type": "object", "properties": {"id": {"type": "integer"}}, "required": ["id"]});
    json!({"tools": [
        {
            "name": "acquire",
            "description": "Take a lease of a kind (a build slot, an approved claim). Answers with the lease once it's held, \
                or asking (waiting for approval), or an error when every slot is held and wait is false.",
            "inputSchema": {"type": "object", "properties": {
                "kind": {"type": "string", "description": "The kind of lease (`list` shows them)."},
                "reason": {"type": "string", "description": "What it's for, in one line."},
                "payload": {"type": "object", "description": "What exactly is claimed, for kinds that take one."},
                "wait": {"type": "boolean", "description": "Wait in line (and for approval) until it's held."},
                "ttl": {"type": "string", "description": "Its limit: 30m, 2h (at most the kind's max_ttl)."},
                "idle": {"type": "string", "description": "A session holder idle this long loses it."},
                "tie": {"type": "string", "enum": ["session", "process"]},
                "pid": {"type": "integer", "description": "The process it's tied to, on the caller's machine."}
            }, "required": ["kind", "reason"]}
        },
        {"name": "release", "description": "End a lease of yours.", "inputSchema": id},
        {
            "name": "list",
            "description": "Every kind, who holds and who waits, and the leases that ended lately.",
            "inputSchema": {"type": "object", "properties": {"kind": {"type": "string"}, "all": {"type": "boolean"}}}
        },
        {"name": "show", "description": "One lease, or how it ended.", "inputSchema": id},
        {"name": "wait", "description": "Wait until a lease of yours that asks or waits is held.", "inputSchema": id},
        {
            "name": "hold",
            "description": "Keep a process-tied lease of yours while this call runs; answers when the lease ends.",
            "inputSchema": id
        }
    ]})
}

/// Serve MCP on stdio until the caller closes it.
pub async fn run(socket: PathBuf, caller: String) -> anyhow::Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        let mut out = tokio::io::stdout();
        while let Some(line) = rx.recv().await {
            if out.write_all((line + "\n").as_bytes()).await.is_err() || out.flush().await.is_err() {
                break;
            }
        }
    });
    let socket = Arc::new(socket);
    let caller = Arc::new(caller);
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let id = msg.get("id").cloned();
        let method = msg["method"].as_str().unwrap_or("").to_string();
        let Some(id) = id.filter(|_| !method.is_empty()) else { continue }; // a notification or a response
        let reply = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
        match method.as_str() {
            "initialize" => {
                let version = msg["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18").to_string();
                let _ = tx.send(reply(json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "fleetlayer-leases", "version": env!("CARGO_PKG_VERSION")},
                })));
            }
            "ping" => {
                let _ = tx.send(reply(json!({})));
            }
            "tools/list" => {
                let _ = tx.send(reply(tools()));
            }
            "tools/call" => {
                let (tx, socket, caller) = (tx.clone(), socket.clone(), caller.clone());
                tokio::spawn(async move {
                    let name = msg["params"]["name"].as_str().unwrap_or("").to_string();
                    let args = msg["params"]["arguments"].clone();
                    let token = msg["params"]["_meta"]["progressToken"].clone();
                    let mut n = 0u64;
                    let progress_tx = tx.clone();
                    let out = if ["acquire", "release", "list", "show", "wait", "hold"].contains(&name.as_str()) {
                        ask(&socket, &name, Some(&caller), args, |p| {
                            n += 1;
                            if !token.is_null() {
                                let note = json!({"jsonrpc": "2.0", "method": "notifications/progress",
                                    "params": {"progressToken": token, "progress": n, "message": p}});
                                let _ = progress_tx.send(note.to_string());
                            }
                        })
                        .await
                    } else {
                        Err(anyhow::anyhow!("no tool called {name}"))
                    };
                    let result = match out {
                        Ok(v) => json!({"content": [{"type": "text", "text": v.to_string()}], "structuredContent": v}),
                        Err(e) => json!({"content": [{"type": "text", "text": e.to_string()}], "isError": true}),
                    };
                    let _ = tx.send(json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string());
                });
            }
            other => {
                let err = json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("unknown method {other}")}});
                let _ = tx.send(err.to_string());
            }
        }
    }
    drop(tx);
    // The caller is gone: every open call's socket closes with this process, which ends what it kept.
    writer.abort();
    std::process::exit(0)
}
