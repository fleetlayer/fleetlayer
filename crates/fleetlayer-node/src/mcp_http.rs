//! A remote MCP server (Streamable HTTP, such as Executor's `/mcp`) as a shared tool: the node speaks to it over HTTP
//! with credentials read from files under its own Unix user, and to the caller the way it speaks to a stdio server,
//! one JSON-RPC message per line. So the gate, its policy and its audit work the same for both, and the agent never
//! holds the server's token.
//!
//! Each message from the caller is one POST. A reply comes back as JSON, or as an event stream that ends with the reply
//! (progress notifications and server requests before it are passed on as they arrive). The session ID from
//! `initialize` goes on every later request, and the session is ended (DELETE) when the caller disconnects. The server's
//! own stream for messages it starts (GET) isn't opened: tools that reply to calls don't need it.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::sync::{Mutex, mpsc};

/// One remote server and the headers to send it (credentials already read from their files).
#[derive(Clone)]
pub struct Upstream {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

#[derive(Default)]
struct Session {
    id: Option<String>,
    protocol: Option<String>,
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Start talking to the server: answers the caller's end of a pipe, which reads and writes lines like a stdio server's
/// stdout and stdin. The bridge ends when the caller's end is dropped, ending the server's session.
pub fn start(up: Upstream) -> DuplexStream {
    let (caller_end, bridge_end) = tokio::io::duplex(1 << 20);
    tokio::spawn(run(up, bridge_end));
    caller_end
}

async fn run(up: Upstream, end: DuplexStream) {
    let (r, mut w) = tokio::io::split(end);
    let http = client();
    let session = Arc::new(Mutex::new(Session::default()));
    let (out_tx, mut out_rx) = mpsc::channel::<String>(256);
    let writer = tokio::spawn(async move {
        while let Some(line) = out_rx.recv().await {
            if w.write_all(format!("{line}\n").as_bytes()).await.is_err() {
                break;
            }
            let _ = w.flush().await;
        }
    });
    let mut lines = BufReader::new(r).lines();
    let mut inflight = tokio::task::JoinSet::new();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let init = msg["method"] == "initialize";
        let job = post(http.clone(), up.clone(), session.clone(), msg, out_tx.clone());
        if init {
            // Everything after `initialize` needs the session it opens.
            job.await;
        } else {
            inflight.spawn(job);
        }
    }
    // The caller has gone: let calls under way finish for a little while, then end the session.
    let _ =
        tokio::time::timeout(Duration::from_secs(10), async { while inflight.join_next().await.is_some() {} }).await;
    inflight.abort_all();
    let id = session.lock().await.id.clone();
    if let Some(id) = id {
        let mut req = http.delete(&up.url).header("mcp-session-id", id).timeout(Duration::from_secs(5));
        for (k, v) in &up.headers {
            req = req.header(k, v);
        }
        let _ = req.send().await;
    }
    drop(out_tx);
    let _ = writer.await;
}

/// POST one message and pass on what comes back. A failure to reach the server is answered to the caller as a JSON-RPC
/// error for a request (a notification has nobody to answer).
async fn post(
    http: reqwest::Client,
    up: Upstream,
    session: Arc<Mutex<Session>>,
    msg: Value,
    out: mpsc::Sender<String>,
) {
    let id = msg.get("id").cloned().filter(|_| msg.get("method").is_some());
    let fail = |why: String| {
        let out = out.clone();
        let id = id.clone();
        async move {
            if let Some(id) = id {
                let e = serde_json::json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000,
                    "message": format!("the shared tool's server: {why}")}});
                let _ = out.send(e.to_string()).await;
            }
        }
    };
    let (sid, protocol) = {
        let s = session.lock().await;
        (s.id.clone(), s.protocol.clone())
    };
    let mut req = http
        .post(&up.url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(msg.to_string());
    for (k, v) in &up.headers {
        req = req.header(k, v);
    }
    if let Some(s) = sid {
        req = req.header("mcp-session-id", s);
    }
    if let Some(p) = protocol {
        req = req.header("mcp-protocol-version", p);
    }
    let mut resp = match req.send().await {
        Ok(r) => r,
        Err(e) => return fail(format!("couldn't reach it ({e})")).await,
    };
    if let Some(s) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
        session.lock().await.id = Some(s.to_string());
    }
    let status = resp.status();
    if status == reqwest::StatusCode::ACCEPTED || status == reqwest::StatusCode::NO_CONTENT {
        return;
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let body: String = body.chars().take(300).collect();
        return fail(format!("it answered {status}: {body}")).await;
    }
    let sse = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| c.starts_with("text/event-stream"));
    let pass = |text: &str| -> Option<bool> {
        // Whether this was the reply to the request (so the stream can end).
        let v: Value = serde_json::from_str(text).ok()?;
        let items = match v {
            Value::Array(a) => a,
            one => vec![one],
        };
        let mut replied = false;
        for item in items {
            if let Some(p) = item.pointer("/result/protocolVersion").and_then(Value::as_str)
                && item.pointer("/result/serverInfo").is_some()
                && let Ok(mut s) = session.try_lock()
            {
                s.protocol = Some(p.to_string());
            }
            if item.get("method").is_none() && id.as_ref().is_some_and(|i| item.get("id") == Some(i)) {
                replied = true;
            }
            let _ = out.try_send(item.to_string());
        }
        Some(replied)
    };
    if !sse {
        match resp.text().await {
            Ok(body) if pass(&body).is_some() => {}
            Ok(body) => {
                let body: String = body.chars().take(300).collect();
                fail(format!("it answered something that isn't JSON: {body}")).await
            }
            Err(e) => fail(format!("its answer was cut off ({e})")).await,
        }
        return;
    }
    let mut buf = String::new();
    loop {
        match resp.chunk().await {
            Ok(Some(bytes)) => {
                buf.push_str(&String::from_utf8_lossy(&bytes));
                while let Some((event, rest)) = next_event(&buf) {
                    buf = rest;
                    if let Some(data) = event
                        && pass(&data) == Some(true)
                    {
                        return;
                    }
                }
            }
            Ok(None) => break,
            Err(e) => return fail(format!("its answer was cut off ({e})")).await,
        }
    }
    if id.is_some() {
        fail("it ended its answer without replying".into()).await;
    }
}

/// The first complete event in an event stream's text: its data (None for an event with none), and the text after
/// it. None while the first event isn't complete yet.
fn next_event(buf: &str) -> Option<(Option<String>, String)> {
    let norm = buf.replace("\r\n", "\n");
    let end = norm.find("\n\n")?;
    let (event, rest) = (&norm[..end], &norm[end + 2..]);
    let mut data: Vec<&str> = vec![];
    let mut kind = "message";
    for line in event.lines() {
        if let Some(d) = line.strip_prefix("data:") {
            data.push(d.strip_prefix(' ').unwrap_or(d));
        } else if let Some(k) = line.strip_prefix("event:") {
            kind = k.trim();
        }
    }
    let data = (kind == "message" && !data.is_empty()).then(|| data.join("\n"));
    Some((data, rest.to_string()))
}

/// The check behind the node's health: initialize, list the tools with the node's credentials, end the session. Fails
/// when the server is down and when it refuses the credentials (a revoked token), not only when nothing answers.
pub async fn probe(up: &Upstream) -> Result<usize, String> {
    let caller = start(up.clone());
    let (r, mut w) = tokio::io::split(caller);
    let mut lines = BufReader::new(r).lines();
    let send = |v: Value| format!("{v}\n");
    let init = serde_json::json!({"jsonrpc": "2.0", "id": "probe-1", "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "fleetlayer-node", "version": env!("CARGO_PKG_VERSION")}}});
    let run = async {
        w.write_all(send(init).as_bytes()).await.map_err(|e| e.to_string())?;
        let reply = read_reply(&mut lines, "probe-1").await?;
        if reply.get("result").is_none() {
            return Err(format!("initialize failed: {}", reply["error"]["message"].as_str().unwrap_or("no result")));
        }
        let note = serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        w.write_all(send(note).as_bytes()).await.map_err(|e| e.to_string())?;
        let list = serde_json::json!({"jsonrpc": "2.0", "id": "probe-2", "method": "tools/list", "params": {}});
        w.write_all(send(list).as_bytes()).await.map_err(|e| e.to_string())?;
        let reply = read_reply(&mut lines, "probe-2").await?;
        match reply.pointer("/result/tools").and_then(Value::as_array) {
            Some(t) => Ok(t.len()),
            None => Err(format!("listing tools failed: {}", reply["error"]["message"].as_str().unwrap_or("no tools"))),
        }
    };
    let out =
        tokio::time::timeout(Duration::from_secs(30), run).await.unwrap_or_else(|_| Err("no answer in 30s".into()));
    drop(w);
    out
}

async fn read_reply<R: tokio::io::AsyncBufRead + Unpin>(
    lines: &mut tokio::io::Lines<R>,
    id: &str,
) -> Result<Value, String> {
    while let Some(line) = lines.next_line().await.map_err(|e| e.to_string())? {
        if let Ok(v) = serde_json::from_str::<Value>(&line)
            && v["id"] == id
        {
            return Ok(v);
        }
    }
    Err("the connection ended".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use std::net::SocketAddr;

    #[test]
    fn events_are_split_and_their_data_joined() {
        let (d, rest) = next_event("event: message\ndata: {\"a\":\ndata: 1}\n\nid: 2\r\ndata: x\r\n\r\n").unwrap();
        assert_eq!(d.as_deref(), Some("{\"a\":\n1}"));
        let (d, rest) = next_event(&rest).unwrap();
        assert_eq!((d.as_deref(), rest.as_str()), (Some("x"), ""));
        assert!(next_event("data: half").is_none());
        assert_eq!(next_event(": ping\n\n").unwrap().0, None);
        assert_eq!(next_event("event: other\ndata: y\n\n").unwrap().0, None);
    }

    /// A small Streamable HTTP server: it wants a bearer token, opens a session at `initialize`, answers `tools/list`
    /// as JSON and `tools/call` as an event stream with a progress notification first.
    async fn server() -> SocketAddr {
        use axum::body::Body;
        use axum::http::{HeaderMap, Response, StatusCode};
        async fn mcp(headers: HeaderMap, body: String) -> Result<Response<Body>, Infallible> {
            let h = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
            if h("authorization") != "Bearer right" {
                return Ok(Response::builder().status(StatusCode::UNAUTHORIZED).body(Body::from("no")).unwrap());
            }
            let v: Value = serde_json::from_str(&body).unwrap();
            let id = v["id"].clone();
            let method = v["method"].as_str().unwrap_or_default();
            if method != "initialize" && h("mcp-session-id") != "s1" {
                return Ok(Response::builder().status(StatusCode::NOT_FOUND).body(Body::from("no session")).unwrap());
            }
            let json = |v: Value| {
                Response::builder()
                    .header("content-type", "application/json")
                    .header("mcp-session-id", "s1")
                    .body(Body::from(v.to_string()))
                    .unwrap()
            };
            Ok(match method {
                "initialize" => json(serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {
                    "protocolVersion": "2025-06-18", "serverInfo": {"name": "t"}, "capabilities": {}}})),
                "tools/list" => {
                    assert_eq!(h("mcp-protocol-version"), "2025-06-18");
                    json(serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [{"name": "call"}]}}))
                }
                "tools/call" => {
                    let note = serde_json::json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {}});
                    let reply = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"content": []}});
                    Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(Body::from(format!("data: {note}\r\n\r\nevent: message\ndata: {reply}\n\n")))
                        .unwrap()
                }
                _ => Response::builder().status(StatusCode::ACCEPTED).body(Body::empty()).unwrap(),
            })
        }
        let app = axum::Router::new().route("/mcp", axum::routing::post(mcp).delete(|| async { "" }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        addr
    }

    fn up(addr: SocketAddr, token: &str) -> Upstream {
        Upstream {
            url: format!("http://{addr}/mcp"),
            headers: vec![("authorization".into(), format!("Bearer {token}"))],
        }
    }

    #[tokio::test]
    async fn the_probe_lists_tools_and_fails_on_refused_credentials() {
        let addr = server().await;
        assert_eq!(probe(&up(addr, "right")).await, Ok(1));
        let e = probe(&up(addr, "revoked")).await.unwrap_err();
        assert!(e.contains("401"), "{e}");
    }

    #[tokio::test]
    async fn an_event_stream_passes_notifications_then_the_reply() {
        let addr = server().await;
        let (r, mut w) = tokio::io::split(start(up(addr, "right")));
        let mut lines = BufReader::new(r).lines();
        for m in [
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"call","arguments":{}}}"#,
        ] {
            w.write_all(format!("{m}\n").as_bytes()).await.unwrap();
        }
        let mut got = vec![];
        while got.len() < 3 {
            let line = lines.next_line().await.unwrap().unwrap();
            got.push(serde_json::from_str::<Value>(&line).unwrap());
        }
        assert_eq!(got[0]["id"], 1);
        assert_eq!(got[1]["method"], "notifications/progress");
        assert_eq!((got[2]["id"].as_i64(), got[2]["result"].is_object()), (Some(2), true));
    }
}
