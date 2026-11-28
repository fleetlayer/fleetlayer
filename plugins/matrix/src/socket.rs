//! The plugin's local socket, `<state dir>/post.sock` (mode 600, in a mode-700 folder): how the MCP server the node
//! starts for each agent connection reaches the one signed-in Matrix client, which the plugin holds. One JSON object
//! per line each way:
//!
//! - `{"op": "rooms"}` → `{"ok": [room...]}`
//! - `{"op": "post", "room", "text", "replyTo"?, "kind"?}` → `{"ok": {"eventId"}}`
//! - `{"op": "history", "room", "limit"?}` → `{"ok": [message...]}`
//!
//! Anything wrong is `{"error": "<a sentence>"}`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::reader::Shared;

/// The most one request line may be.
const MAX_LINE: usize = 64 * 1024;

pub fn path(dir: &Path) -> PathBuf {
    dir.join("post.sock")
}

/// Listen until the plugin stops.
pub async fn serve(shared: Arc<Shared>) -> std::io::Result<()> {
    let path = path(&shared.dir);
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    loop {
        let (stream, _) = listener.accept().await?;
        let shared = shared.clone();
        tokio::spawn(async move {
            let (r, mut w) = stream.into_split();
            let mut lines = BufReader::new(r).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let answer = if line.len() > MAX_LINE {
                    json!({"error": "the request is too long"})
                } else {
                    match serde_json::from_str::<Value>(&line) {
                        Ok(req) => match handle(&shared, &req).await {
                            Ok(v) => json!({"ok": v}),
                            Err(e) => json!({"error": e}),
                        },
                        Err(_) => json!({"error": "not JSON"}),
                    }
                };
                if w.write_all(format!("{answer}\n").as_bytes()).await.is_err() {
                    return;
                }
            }
        });
    }
}

fn text<'a>(req: &'a Value, key: &str) -> Result<&'a str, String> {
    req[key].as_str().ok_or_else(|| format!("{key} is missing"))
}

async fn handle(shared: &Shared, req: &Value) -> Result<Value, String> {
    match req["op"].as_str().unwrap_or_default() {
        "rooms" => Ok(Value::Array(shared.room_list().await)),
        "post" => {
            let id = shared
                .post(
                    text(req, "room")?,
                    text(req, "text")?,
                    req["replyTo"].as_str().filter(|s| !s.is_empty()),
                    req["kind"].as_str().unwrap_or("notice"),
                )
                .await?;
            Ok(json!({"eventId": id}))
        }
        "history" => {
            let limit = u32::try_from(crate::mcp::limit_arg(&req["limit"])?).unwrap_or(100);
            let messages = shared.history(text(req, "room")?, limit).await?;
            Ok(Value::Array(
                messages
                    .into_iter()
                    .map(|m| {
                        json!({"eventId": m.event_id, "sender": m.sender, "senderName": m.sender_name,
                               "at": jiff::Timestamp::from_millisecond(m.ts_ms).ok(), "text": m.text,
                               "replyTo": m.reply_to})
                    })
                    .collect(),
            ))
        }
        other => Err(format!("unknown op {other:?}")),
    }
}

/// Ask the plugin one thing (the MCP server's side).
pub async fn ask(dir: &Path, req: &Value) -> Result<Value, String> {
    let path = path(dir);
    let stream = UnixStream::connect(&path).await.map_err(|e| {
        format!("can't reach the Matrix plugin at {} ({e}): is it running on this node, as this user?", path.display())
    })?;
    let (r, mut w) = stream.into_split();
    w.write_all(format!("{req}\n").as_bytes()).await.map_err(|e| e.to_string())?;
    let mut line = String::new();
    let read = tokio::time::timeout(std::time::Duration::from_secs(60), BufReader::new(r).read_line(&mut line)).await;
    match read {
        Ok(Ok(n)) if n > 0 => {}
        Ok(Ok(_)) => return Err("the Matrix plugin closed the connection".into()),
        Ok(Err(e)) => return Err(e.to_string()),
        Err(_) => return Err("the Matrix plugin didn't answer within 60s".into()),
    }
    let v: Value = serde_json::from_str(&line).map_err(|_| "the Matrix plugin's answer isn't JSON".to_string())?;
    match v.get("error").and_then(Value::as_str) {
        Some(e) => Err(e.to_string()),
        None => Ok(v["ok"].clone()),
    }
}
